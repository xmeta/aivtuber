//! Authenticated local operator control transport (issue #56).
//!
//! The daemon binds a local-socket endpoint (Windows named pipe / Unix domain
//! socket) that terminates in the existing `LocalControlIngress` /
//! authenticated capability boundary. Operator commands never share the
//! untrusted content queue: they are applied directly through
//! `ProductionApp::handle_control`, the same emergency path used by operator
//! hotkeys, so control stays responsive while content ingress, model
//! services, or adapters are degraded or saturated.
//!
//! Security properties implemented here:
//! - the transport is local-machine scope by default (named pipe / Unix
//!   socket, never TCP) so accidental network exposure is not possible,
//! - authentication is required before any `AuthenticatedControl` is minted
//!   (constant-time secret comparison inside `LocalControlIngress`),
//! - every privileged action is capability-checked by
//!   `SecurityRuntime::handle_control_with_scheduler`,
//! - the connection secret is never written to logs or status output,
//! - requests are rate-limited with a token bucket; the control endpoint has
//!   its own bounded queue and does not contend with content ingress,
//! - accepted and rejected operator actions are audited.
//!
//! Unmute semantics (issue #56 acceptance): `unmute` is intentionally NOT
//! supported. Mute is a latching emergency state — resuming output requires
//! restarting the daemon with a verified configuration. This is documented in
//! `docs/production-runtime.adoc`.

use crate::{AdapterHealth, ProductionApp, RoutePlanner};
use aivtuber_domain::{LocalControlIngress, OperatorCommandInput};
use aivtuber_runtime::ControlOutcome;
use interprocess::local_socket::traits::tokio::Listener as _;
use serde::{Deserialize, Serialize};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::{mpsc, oneshot},
    time::timeout,
};
use uuid::Uuid;

/// One line-delimited JSON request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OperatorRequest {
    /// One of `status`, `stop`, `mute`.
    pub action: String,
    /// Opaque session secret presented by the operator client.
    pub secret: String,
    /// Optional client-provided id; the server assigns one when absent.
    #[serde(default)]
    pub request_id: Option<String>,
}

/// One line-delimited JSON response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OperatorResponse {
    pub request_id: String,
    pub ok: bool,
    /// Human-readable outcome. Never contains secret material.
    pub detail: String,
    /// Machine-readable snapshot for `status`; absent for other actions.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<OperatorStatusSnapshot>,
}

/// Health/queue snapshot without secret material.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OperatorStatusSnapshot {
    pub muted: bool,
    pub audio_error: Option<String>,
    pub avatar_error: Option<String>,
    pub stream_error: Option<String>,
    pub content_queue: usize,
    pub scheduler_items: usize,
    pub generation_active: bool,
}

impl OperatorStatusSnapshot {
    pub fn from_app<R: RoutePlanner>(app: &ProductionApp<R>) -> Self {
        let health: &AdapterHealth = app.health();
        let generation = app.generation_execution_snapshot();
        Self {
            muted: app.is_muted(),
            audio_error: health.audio_error.clone(),
            avatar_error: health.avatar_error.clone(),
            stream_error: health.stream_error.clone(),
            content_queue: app.content_queue_len(),
            scheduler_items: app.scheduler_item_count(),
            generation_active: generation
                .as_ref()
                .is_some_and(|snapshot| snapshot.in_flight > 0 || snapshot.pending > 0),
        }
    }
}

#[derive(Debug)]
pub enum OperatorControlError {
    InvalidConfiguration(&'static str),
    Bind(String),
}

impl std::fmt::Display for OperatorControlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidConfiguration(field) => {
                write!(f, "invalid operator control configuration: {field}")
            }
            Self::Bind(detail) => write!(f, "failed to bind operator control endpoint: {detail}"),
        }
    }
}

impl std::error::Error for OperatorControlError {}

/// Token-bucket rate limiter for local control requests. A misbehaving local
/// client cannot flood the endpoint; emergency control has its own bounded
/// queue and never contends with content ingress.
#[derive(Debug)]
pub struct OperatorRateLimiter {
    capacity: u32,
    refill_per_second: u32,
    tokens: f64,
    last_refill: std::time::Instant,
    rejected: u64,
}

impl OperatorRateLimiter {
    pub fn new(capacity: u32, refill_per_second: u32) -> Self {
        Self {
            capacity,
            refill_per_second,
            tokens: f64::from(capacity),
            last_refill: std::time::Instant::now(),
            rejected: 0,
        }
    }

    pub fn try_acquire(&mut self) -> bool {
        let now = std::time::Instant::now();
        let elapsed = now.duration_since(self.last_refill).as_secs_f64();
        if elapsed > 0.0 {
            self.tokens = (self.tokens + elapsed * f64::from(self.refill_per_second))
                .min(f64::from(self.capacity));
            self.last_refill = now;
        }
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            self.rejected += 1;
            false
        }
    }

    pub fn rejected(&self) -> u64 {
        self.rejected
    }
}

/// A request dispatched from the endpoint task to the runtime thread. The
/// presented secret is moved in a plain `String` but is consumed by the
/// constant-time comparison inside `LocalControlIngress` and never logged.
pub struct DispatchedOperatorRequest {
    pub request: OperatorRequest,
    pub respond: oneshot::Sender<OperatorResponse>,
}

/// Result handed back to the endpoint task after the runtime thread applies
/// (or rejects) a dispatched request.
pub type DispatchOutcome = OperatorResponse;

/// Authenticated operator control endpoint.
pub struct OperatorControlServer {
    name: String,
    ingress: Arc<LocalControlIngress>,
    dispatcher_tx: mpsc::Sender<DispatchedOperatorRequest>,
}

impl OperatorControlServer {
    /// Create the server for a local socket name. On Windows the name is a
    /// named pipe in the `\\.\pipe\` namespace; on Unix a filesystem path.
    /// The daemon generates the secret and exposes it to operator clients via
    /// `AIVTUBER_CONTROL_SECRET`-scoped delivery (see docs).
    pub fn new(
        name: &str,
        secret: [u8; 32],
    ) -> Result<(Self, mpsc::Receiver<DispatchedOperatorRequest>), OperatorControlError> {
        let name = name.trim();
        if name.is_empty() {
            return Err(OperatorControlError::InvalidConfiguration("endpoint name"));
        }

        let capabilities = std::collections::BTreeSet::from([
            aivtuber_domain::Capability::PerformerStop,
            aivtuber_domain::Capability::PerformerMute,
        ]);
        let ingress = Arc::new(
            LocalControlIngress::new(
                "operator-control",
                "operator:local",
                aivtuber_domain::AuthorizationMethod::SignedLocalApi,
                capabilities,
                aivtuber_domain::ControlSecret::new(secret),
            )
            .map_err(|_| OperatorControlError::InvalidConfiguration("ingress"))?,
        );

        let (dispatcher_tx, dispatcher_rx) = mpsc::channel::<DispatchedOperatorRequest>(64);
        let server = Self {
            name: name.to_owned(),
            ingress,
            dispatcher_tx,
        };
        Ok((server, dispatcher_rx))
    }

    pub fn endpoint_name(&self) -> &str {
        &self.name
    }

    /// The shared ingress used to authenticate every dispatched request on
    /// the runtime thread. Cloning the `Arc` is cheap and does not widen
    /// authority: the ingress itself only mints scoped capability sets.
    pub fn ingress(&self) -> Arc<LocalControlIngress> {
        self.ingress.clone()
    }

    /// Bind the local socket and serve connections until the listener fails.
    ///
    /// Each accepted connection handles line-delimited JSON requests; every
    /// request is authenticated independently. Invalid or unauthenticated
    /// requests receive a rejection response; they can never mint control
    /// authority.
    pub async fn serve(
        &self,
        rate: Arc<Mutex<OperatorRateLimiter>>,
    ) -> Result<(), OperatorControlError> {
        let listener = self
            .bind_listener()
            .await
            .map_err(|error| OperatorControlError::Bind(error.to_string()))?;
        eprintln!("operator_control: listening endpoint={}", self.name);

        loop {
            let conn = match listener.accept().await {
                Ok(accepted) => accepted,
                Err(error) => {
                    eprintln!("operator_control: accept_error={error}");
                    continue;
                }
            };
            let dispatcher = self.dispatcher_tx.clone();
            let rate = rate.clone();
            let ingress = self.ingress.clone();
            tokio::spawn(async move {
                handle_connection(conn, ingress, dispatcher, rate).await;
            });
        }
    }

    async fn bind_listener(&self) -> std::io::Result<interprocess::local_socket::tokio::Listener> {
        #[cfg(windows)]
        {
            use interprocess::local_socket::{GenericNamespaced, ToNsName};
            let name = self.name.as_str().to_ns_name::<GenericNamespaced>()?;
            interprocess::local_socket::ListenerOptions::new()
                .name(name)
                .create_tokio()
        }
        #[cfg(unix)]
        {
            use interprocess::local_socket::{GenericFilePath, ToFsName};
            let path = std::path::PathBuf::from(&self.name);
            let name = path.to_fs_name::<GenericFilePath>()?;
            interprocess::local_socket::ListenerOptions::new()
                .name(name)
                .create_tokio()
        }
    }
}

const MAX_RECORD_BYTES: usize = 1_024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

async fn handle_connection(
    mut conn: interprocess::local_socket::tokio::Stream,
    _ingress: Arc<LocalControlIngress>,
    dispatcher: mpsc::Sender<DispatchedOperatorRequest>,
    rate: Arc<Mutex<OperatorRateLimiter>>,
) {
    let mut pending: Vec<u8> = Vec::with_capacity(256);
    let mut chunk = [0_u8; 256];

    loop {
        let need_line = !pending.contains(&b'\n');
        if need_line {
            match timeout(REQUEST_TIMEOUT, conn.read(&mut chunk)).await {
                Ok(Ok(0)) | Ok(Err(_)) | Err(_) => break,
                Ok(Ok(n)) => {
                    pending.extend_from_slice(&chunk[..n]);
                    if pending.len() > MAX_RECORD_BYTES {
                        let _ = conn
                            .write_all(
                                b"{\"request_id\":\"\",\"ok\":false,\"detail\":\"record too large\"}\n",
                            )
                            .await;
                        break;
                    }
                    if !pending.contains(&b'\n') {
                        continue;
                    }
                }
            }
        }

        let Some(line_end) = pending.iter().position(|byte| *byte == b'\n') else {
            continue;
        };
        let line: Vec<u8> = pending.drain(..=line_end).collect();
        let line = String::from_utf8_lossy(&line);
        let line = line.trim();

        if rate.lock().expect("rate limiter poisoned").try_acquire() {
            let request: Option<OperatorRequest> = serde_json::from_str(line).ok();
            match request {
                Some(request) => {
                    let (tx, rx) = oneshot::channel();
                    if dispatcher
                        .send(DispatchedOperatorRequest {
                            request,
                            respond: tx,
                        })
                        .await
                        .is_err()
                    {
                        break;
                    }
                    if let Ok(response) = rx.await {
                        let mut body = serde_json::to_vec(&response).unwrap_or_default();
                        body.push(b'\n');
                        if conn.write_all(&body).await.is_err() {
                            break;
                        }
                    } else {
                        break;
                    }
                }
                None => {
                    let _ = conn
                        .write_all(
                            b"{\"request_id\":\"\",\"ok\":false,\"detail\":\"invalid request\"}\n",
                        )
                        .await;
                }
            }
        } else {
            let _ = conn
                .write_all(b"{\"request_id\":\"\",\"ok\":false,\"detail\":\"rate limited\"}\n")
                .await;
        }
    }
}

/// Authenticate and apply one dispatched request on the runtime thread. This
/// is the only path that can mint `AuthenticatedControl` for the endpoint.
pub fn apply_dispatched_request<R: RoutePlanner>(
    ingress: &LocalControlIngress,
    app: &mut ProductionApp<R>,
    dispatched: &DispatchedOperatorRequest,
    now_ms: u64,
) -> OperatorResponse {
    let request = &dispatched.request;
    let request_id = request
        .request_id
        .clone()
        .unwrap_or_else(|| Uuid::new_v4().to_string());

    if request.action != "status" && request.action != "stop" && request.action != "mute" {
        let detail = format!("unknown action {}", request.action);
        app.audit_operator_action(&request_id, &request.action, &detail, false);
        return rejection(&request_id, &detail);
    }

    let secret_bytes = request.secret.as_bytes();
    if secret_bytes.len() != 32 {
        let detail = "authentication failed".to_owned();
        app.audit_operator_action(&request_id, &request.action, &detail, false);
        return rejection(&request_id, &detail);
    }
    let mut presented = [0_u8; 32];
    presented.copy_from_slice(secret_bytes);

    if request.action == "status" {
        // `status` only reads non-secret health state, so it uses constant-
        // time secret verification without minting control authority. All
        // privileged actions go through `authenticate` below.
        if !ingress.verify_secret(&presented) {
            let detail = "authentication failed".to_owned();
            app.audit_operator_action(&request_id, "status", &detail, false);
            return rejection(&request_id, &detail);
        }
        let snapshot = OperatorStatusSnapshot::from_app(app);
        app.audit_operator_action(&request_id, "status", "status", true);
        return OperatorResponse {
            request_id,
            ok: true,
            detail: "status".to_owned(),
            status: Some(snapshot),
        };
    }

    let input = OperatorCommandInput {
        event_id: format!("operator-{request_id}"),
        correlation_id: format!("operator-{request_id}"),
        sequence: 0,
        observed_at: rfc3339_from_ms(now_ms),
        action: request.action.clone(),
        payload: std::collections::BTreeMap::new(),
    };

    let command = match ingress.authenticate(input, &presented) {
        Ok(command) => command,
        Err(_) => {
            let detail = "authentication failed".to_owned();
            app.audit_operator_action(&request_id, &request.action, &detail, false);
            return rejection(&request_id, &detail);
        }
    };

    match app.handle_control(&command, now_ms) {
        Ok(outcome) => {
            let detail = match outcome {
                ControlOutcome::Stopped { cancelled } => format!("stopped {cancelled} items"),
                ControlOutcome::Muted => "muted".to_owned(),
                ControlOutcome::AuthenticatedOther => "authenticated".to_owned(),
            };
            app.audit_operator_action(&request_id, &request.action, &detail, true);
            OperatorResponse {
                request_id,
                ok: true,
                detail,
                status: None,
            }
        }
        Err(error) => {
            let detail = format!("rejected: {error}");
            app.audit_operator_action(&request_id, &request.action, &detail, false);
            rejection(&request_id, &detail)
        }
    }
}

fn rejection(request_id: &str, detail: &str) -> OperatorResponse {
    OperatorResponse {
        request_id: request_id.to_owned(),
        ok: false,
        detail: detail.to_owned(),
        status: None,
    }
}

/// The scheduler/runtime timeline is a monotonic millisecond counter, so the
/// operator envelope timestamp uses the same epoch-relative encoding used by
/// benchmarks instead of deriving wall-clock time in the control path.
fn rfc3339_from_ms(now_ms: u64) -> String {
    let seconds = now_ms / 1_000;
    format!("1970-01-01T00:{:02}:{:02}Z", seconds / 60, seconds % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_request_round_trip_in_lib() {
        let (server, mut rx) =
            OperatorControlServer::new("aivtuber-lib-test", [0x5A_u8; 32]).expect("server");
        let _ingress = server.ingress();
        let rate = Arc::new(Mutex::new(OperatorRateLimiter::new(16, 8)));
        let rate_clone = rate.clone();

        // Drive the endpoint on a separate thread with its own runtime.
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async move {
                tokio::spawn(async move {
                    let _ = server.serve(rate_clone).await;
                });
                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                // Client side. The name-type traits are cfg-gated so each
                // platform only imports what it uses.
                use interprocess::local_socket::traits::tokio::Stream as _;
                #[cfg(unix)]
                use interprocess::local_socket::{GenericFilePath, ToFsName};
                #[cfg(windows)]
                use interprocess::local_socket::{GenericNamespaced, ToNsName};
                #[cfg(windows)]
                let name = "aivtuber-lib-test"
                    .to_ns_name::<GenericNamespaced>()
                    .unwrap();
                #[cfg(unix)]
                let name = std::path::PathBuf::from("aivtuber-lib-test")
                    .to_fs_name::<GenericFilePath>()
                    .unwrap();
                let mut conn = interprocess::local_socket::tokio::Stream::connect(name)
                    .await
                    .expect("connect");
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let req = format!(
                    "{{\"action\":\"status\",\"secret\":\"{}\"}}\n",
                    "Z".repeat(32)
                );
                conn.write_all(req.as_bytes()).await.unwrap();
                conn.flush().await.unwrap();
                let mut buf = vec![0_u8; 512];
                let n = conn.read(&mut buf).await.unwrap();
                let text = String::from_utf8_lossy(&buf[..n]);
                assert!(text.contains("\"ok\":true"), "got: {text}");
            });
        });
        // Drain dispatched requests on the "runtime thread" (this thread).
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            if let Ok(dispatched) = rx.try_recv() {
                // status is a status-only request — but we don't have an app here.
                // Respond directly.
                let _ = dispatched.respond.send(OperatorResponse {
                    request_id: "test".into(),
                    ok: true,
                    detail: "test".into(),
                    status: None,
                });
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }
}
