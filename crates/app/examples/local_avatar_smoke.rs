//! Driver for the headless renderer smoke test (`renderer/local-avatar/smoke`).
//!
//! Starts the REAL `LocalAvatarOutput` (loopback server, token, Origin/Host
//! checks, epoch/seq state machine) serving a page directory, then acts as a
//! line-oriented remote control for the browser-side test over stdio:
//!
//! * stdout: one JSON line per reply. The first line is
//!   `{"event":"listening","url":"..."}`; the URL carries the per-launch
//!   secret, so only a local test harness should read this process's stdout.
//! * stdin: `health` prints the adapter health; `quit` exits; any other line
//!   is an `AvatarPlaybackCommand` JSON object (the same serde form the
//!   scheduler emits) executed through `AvatarOutput::execute`.
//!
//! Not a production binary: it fixes the control secret and is only built for
//! the smoke job (`cargo build --example local_avatar_smoke`).

use aivtuber_adapters::{LocalAvatarConfig, LocalAvatarServer};
use aivtuber_app::{AvatarOutput, LocalAvatarOutput};
use aivtuber_domain::{
    AuthenticatedControl, AuthorizationMethod, Capability, ControlSecret, LocalControlIngress,
    OperatorCommandInput,
};
use aivtuber_runtime::AvatarPlaybackCommand;
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::io::{BufRead, Write};
use std::path::PathBuf;

fn authority() -> Result<AuthenticatedControl, Box<dyn Error>> {
    // Test-only secret: this example never touches a real runtime profile.
    let secret = [7_u8; 32];
    let ingress = LocalControlIngress::new(
        "local-avatar-smoke",
        "runtime:motor",
        AuthorizationMethod::SignedLocalApi,
        BTreeSet::from([Capability::AvatarControl]),
        ControlSecret::new(secret),
    )?;
    let command = ingress.authenticate(
        OperatorCommandInput {
            event_id: "smoke-avatar-authority".to_owned(),
            correlation_id: "smoke".to_owned(),
            sequence: 0,
            observed_at: "1970-01-01T00:00:00Z".to_owned(),
            action: "avatar.control".to_owned(),
            payload: BTreeMap::new(),
        },
        &secret,
    )?;
    Ok(command.authority().clone())
}

fn health_json(output: &LocalAvatarOutput) -> serde_json::Value {
    let h = output.health();
    json!({
        "state": format!("{:?}", h.state),
        "sent": h.sent,
        "acked": h.acked,
        "rejected": h.rejected.iter().map(|(k, v)| (format!("{k:?}"), *v)).collect::<BTreeMap<_, _>>(),
        "dropped": h.dropped.iter().map(|(k, v)| (format!("{k:?}"), *v)).collect::<BTreeMap<_, _>>(),
        "reconnects": h.reconnects,
        "protocol_errors": h.protocol_errors,
        "stray_acks": h.stray_acks,
    })
}

fn main() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args().skip(1);
    let mut page_dir: Option<PathBuf> = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--page-dir" => page_dir = args.next().map(PathBuf::from),
            other => return Err(format!("unknown argument {other}").into()),
        }
    }
    let page_dir = page_dir.ok_or("--page-dir <dir> is required")?;
    let server = LocalAvatarServer::start(LocalAvatarConfig {
        port: 0,
        // Software-GL frames can take >100 ms; the smoke sends commands only
        // after `ready` and waits for acks, so the dispatch window is generous.
        max_command_age: std::time::Duration::from_secs(5),
        ..LocalAvatarConfig::default().with_page_dir(page_dir)
    })?;
    let mut output = LocalAvatarOutput::new(server, authority()?);
    let stdout = std::io::stdout();
    let reply = |value: serde_json::Value| {
        let mut out = stdout.lock();
        let _ = writeln!(out, "{value}");
        let _ = out.flush();
    };
    reply(json!({"event": "listening", "url": output.launch_url()}));
    for line in std::io::stdin().lock().lines() {
        let line = line?;
        match line.trim() {
            "" => {}
            "quit" => break,
            "health" => reply(json!({"event": "health", "health": health_json(&output)})),
            text => {
                let result = serde_json::from_str::<AvatarPlaybackCommand>(text)
                    .map_err(|e| e.to_string())
                    .and_then(|c| c.validate().map_err(|e| e.to_string()).map(|()| c))
                    .and_then(|c| output.execute(&c).map_err(|e| e.to_string()));
                match result {
                    Ok(()) => reply(json!({"event": "executed"})),
                    Err(error) => reply(json!({"event": "error", "error": error})),
                }
            }
        }
    }
    Ok(())
}
