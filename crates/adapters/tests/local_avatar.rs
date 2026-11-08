//! CI tests for the local avatar adapter. A scripted in-process mock renderer
//! (a plain tungstenite client) stands in for the browser page: no GPU, no
//! browser (ADR-0003 CI strategy).

use aivtuber_adapters::{
    DropReason, HostFrame, LocalAvatarConfig, LocalAvatarHandle, LocalAvatarServer,
    LocalAvatarState, RejectReason, RendererFrame, WireCommand,
};
use aivtuber_domain::Viseme;
use serde_json::Value;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tungstenite::client::IntoClientRequest;
use tungstenite::{Error as WsError, HandshakeError, Message, WebSocket};

const HASH: &str = "12c2b97e00000000000000000000000000000000000000000000000000631a2f";

fn config() -> LocalAvatarConfig {
    LocalAvatarConfig {
        port: 0,
        ..LocalAvatarConfig::default()
    }
}

fn start(config: LocalAvatarConfig) -> (LocalAvatarServer, LocalAvatarHandle) {
    let server = LocalAvatarServer::start(config).expect("server starts");
    let handle = server.handle();
    (server, handle)
}

fn viseme(v: Viseme) -> WireCommand {
    WireCommand::Viseme {
        viseme: v,
        weight: 0.5,
    }
}

fn wait_until(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Upgrade attempt; `Err(status)` when the server refuses the handshake.
fn try_connect(
    addr: SocketAddr,
    token: &str,
    origin: Option<&str>,
) -> Result<WebSocket<TcpStream>, u16> {
    let mut request = format!("ws://{addr}/ws?token={token}")
        .into_client_request()
        .expect("request");
    if let Some(origin) = origin {
        request
            .headers_mut()
            .insert("Origin", origin.parse().expect("origin header"));
    }
    let stream = TcpStream::connect(addr).expect("tcp connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("timeout");
    match tungstenite::client(request, stream) {
        Ok((ws, _)) => Ok(ws),
        Err(HandshakeError::Failure(WsError::Http(response))) => Err(response.status().as_u16()),
        Err(other) => panic!("unexpected handshake failure: {other:?}"),
    }
}

struct Mock {
    ws: WebSocket<TcpStream>,
    epoch: String,
}

impl Mock {
    fn connect(server: &LocalAvatarServer, epoch: &str) -> Self {
        let origin = format!("http://{}", server.local_addr());
        let ws = try_connect(server.local_addr(), server.token(), Some(&origin))
            .expect("authorized upgrade");
        Self {
            ws,
            epoch: epoch.to_owned(),
        }
    }

    fn send(&mut self, frame: &RendererFrame) {
        self.ws
            .send(Message::text(frame.to_json()))
            .expect("renderer send");
    }

    fn hello(&mut self) {
        let frame = RendererFrame::Hello {
            v: 1,
            epoch: self.epoch.clone(),
            model_hash: HASH.to_owned(),
            profile: "vrm1-default@1".to_owned(),
        };
        self.send(&frame);
    }

    fn ready(&mut self) {
        let frame = RendererFrame::Ready {
            epoch: self.epoch.clone(),
        };
        self.send(&frame);
    }

    fn ack(&mut self, seq: u64) {
        let frame = RendererFrame::Ack {
            epoch: self.epoch.clone(),
            seq,
        };
        self.send(&frame);
    }

    /// Next host frame within `wait`, else `None`.
    fn recv_within(&mut self, wait: Duration) -> Option<HostFrame> {
        self.ws
            .get_mut()
            .set_read_timeout(Some(wait))
            .expect("timeout");
        match self.ws.read() {
            Ok(Message::Text(text)) => Some(HostFrame::parse(text.as_str()).expect("valid frame")),
            Ok(other) => panic!("unexpected message {other:?}"),
            Err(WsError::Io(e))
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                None
            }
            Err(_) => None,
        }
    }

    fn recv(&mut self) -> HostFrame {
        self.recv_within(Duration::from_secs(5))
            .expect("host frame")
    }

    /// hello + ready, then consumes the leading reset.
    fn become_ready(&mut self, handle: &LocalAvatarHandle) {
        self.hello();
        self.ready();
        let reset = self.recv();
        assert_eq!(reset.cmd, WireCommand::Reset {});
        assert_eq!((reset.seq, reset.epoch.as_str()), (1, self.epoch.as_str()));
        wait_until("ready", || handle.health().state == LocalAvatarState::Ready);
    }
}

fn fixture(name: &str) -> Vec<Value> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/local-avatar-1")
        .join(name);
    let text = std::fs::read_to_string(path).expect("fixture");
    serde_json::from_str(&text).expect("fixture json")
}

#[test]
fn valid_fixtures_round_trip_and_invalid_fixtures_are_refused() {
    for value in fixture("valid-host.json") {
        let frame = HostFrame::parse(&value.to_string()).expect("valid host frame");
        let again: Value = serde_json::from_str(&frame.to_json()).expect("json");
        assert_eq!(again, value);
    }
    for value in fixture("valid-renderer.json") {
        let frame = RendererFrame::parse(&value.to_string()).expect("valid renderer frame");
        let again: Value = serde_json::from_str(&frame.to_json()).expect("json");
        assert_eq!(again, value);
    }
    for case in fixture("invalid-host.json") {
        let frame = case["frame"].to_string();
        assert!(
            HostFrame::parse(&frame).is_err(),
            "accepted: {}",
            case["name"]
        );
    }
    for case in fixture("invalid-renderer.json") {
        let frame = case["frame"].to_string();
        assert!(
            RendererFrame::parse(&frame).is_err(),
            "accepted: {}",
            case["name"]
        );
    }
    let oversized = format!(
        r#"{{"type":"ready","epoch":"{}"}}"#,
        "a".repeat(aivtuber_adapters::MAX_FRAME_BYTES)
    );
    assert!(RendererFrame::parse(&oversized).is_err());
}

#[test]
fn handshake_ready_reset_commands_and_acks() {
    let (server, handle) = start(config());
    assert_eq!(handle.health().state, LocalAvatarState::Disconnected);
    let mut mock = Mock::connect(&server, "epoch-a");
    wait_until("handshaking", || {
        handle.health().state == LocalAvatarState::Handshaking
    });
    mock.hello();
    // hello alone is "not ready yet": the host sends nothing.
    assert!(mock.recv_within(Duration::from_millis(150)).is_none());
    assert_eq!(handle.health().state, LocalAvatarState::Handshaking);

    mock.ready();
    let reset = mock.recv();
    assert_eq!(reset, HostFrame::new("epoch-a", 1, WireCommand::Reset {}));

    handle.enqueue(viseme(Viseme::Aa)).expect("queued");
    handle.enqueue(viseme(Viseme::Neutral)).expect("queued");
    let first = mock.recv();
    let second = mock.recv();
    assert_eq!((first.seq, second.seq), (2, 3));
    assert_eq!(first.cmd, viseme(Viseme::Aa));
    assert_eq!(second.cmd, viseme(Viseme::Neutral));

    for seq in 1..=3 {
        mock.ack(seq);
    }
    wait_until("acks", || handle.health().acked == 3);
    let health = handle.health();
    assert_eq!(health.state, LocalAvatarState::Ready);
    assert_eq!(
        (health.sent, health.stray_acks, health.reconnects),
        (3, 0, 0)
    );
}

#[test]
fn wrong_token_missing_origin_and_foreign_origin_are_403() {
    let (server, _handle) = start(config());
    let addr = server.local_addr();
    let good_origin = format!("http://{addr}");
    assert_eq!(
        try_connect(addr, "not-the-token", Some(&good_origin)).err(),
        Some(403)
    );
    assert_eq!(try_connect(addr, server.token(), None).err(), Some(403));
    assert_eq!(
        try_connect(addr, server.token(), Some("http://evil.example")).err(),
        Some(403)
    );
    // Same host, different port: still a foreign origin.
    assert_eq!(
        try_connect(addr, server.token(), Some("http://127.0.0.1:1")).err(),
        Some(403)
    );
    // `localhost` is a different origin string and is not accepted either.
    let localhost = format!("http://localhost:{}", addr.port());
    assert_eq!(
        try_connect(addr, server.token(), Some(&localhost)).err(),
        Some(403)
    );
    assert!(try_connect(addr, server.token(), Some(&good_origin)).is_ok());
}

#[test]
fn second_connection_is_rejected_with_409_while_one_is_live() {
    let (server, handle) = start(config());
    let addr = server.local_addr();
    let origin = format!("http://{addr}");
    let first = Mock::connect(&server, "epoch-a");
    wait_until("handshaking", || {
        handle.health().state == LocalAvatarState::Handshaking
    });
    assert_eq!(
        try_connect(addr, server.token(), Some(&origin)).err(),
        Some(409)
    );
    // An unauthenticated peer learns nothing about the live connection.
    assert_eq!(try_connect(addr, "bad", Some(&origin)).err(), Some(403));
    // The first connection was not displaced.
    assert_eq!(handle.health().state, LocalAvatarState::Handshaking);

    drop(first);
    wait_until("disconnect", || {
        handle.health().state == LocalAvatarState::Disconnected
    });
    assert!(try_connect(addr, server.token(), Some(&origin)).is_ok());
}

#[test]
fn reconnect_with_new_epoch_drops_old_commands_and_sends_reset_first() {
    let (server, handle) = start(config());
    let mut old = Mock::connect(&server, "epoch-a");
    old.hello(); // handshaking: queued commands are legal but not yet sent
    wait_until("handshaking", || {
        handle.health().state == LocalAvatarState::Handshaking
    });
    handle
        .enqueue(viseme(Viseme::Oh))
        .expect("queued for epoch-a");
    drop(old);
    wait_until("disconnect", || {
        handle.health().state == LocalAvatarState::Disconnected
    });
    assert_eq!(handle.health().dropped_count(DropReason::StaleEpoch), 1);

    let mut new = Mock::connect(&server, "epoch-b");
    new.become_ready(&handle); // asserts the first frame is Reset, seq 1, epoch-b
    assert!(
        new.recv_within(Duration::from_millis(300)).is_none(),
        "old-epoch command must not be replayed"
    );
    handle.enqueue(viseme(Viseme::Ee)).expect("queued");
    let next = new.recv();
    assert_eq!((next.epoch.as_str(), next.seq), ("epoch-b", 2));
    assert_eq!(next.cmd, viseme(Viseme::Ee));
    let health = handle.health();
    assert_eq!(health.reconnects, 1);
    assert_eq!(health.sent, 2);
}

#[test]
fn stale_or_unknown_acks_are_never_counted_as_applied() {
    let (server, handle) = start(config());
    let mut mock = Mock::connect(&server, "epoch-a");
    mock.become_ready(&handle);
    handle.enqueue(viseme(Viseme::Aa)).expect("queued");
    assert_eq!(mock.recv().seq, 2);

    // Wrong epoch, never-sent seq: untrusted noise, not progress.
    mock.send(&RendererFrame::Ack {
        epoch: "epoch-old".to_owned(),
        seq: 2,
    });
    mock.send(&RendererFrame::Ack {
        epoch: "epoch-a".to_owned(),
        seq: 99,
    });
    // The renderer's own stale_seq refusal is counted by reason.
    mock.send(&RendererFrame::Reject {
        epoch: "epoch-a".to_owned(),
        seq: 2,
        reason: RejectReason::StaleSeq,
    });
    // Already settled: a duplicate is stray too.
    mock.ack(2);
    wait_until("strays", || handle.health().stray_acks == 3);
    let health = handle.health();
    assert_eq!(health.acked, 0);
    assert_eq!(health.rejected_count(RejectReason::StaleSeq), 1);

    // Host seq stays strictly increasing.
    handle.enqueue(viseme(Viseme::Ih)).expect("queued");
    assert_eq!(mock.recv().seq, 3);
}

#[test]
fn malformed_renderer_frame_closes_the_connection() {
    let (server, handle) = start(config());
    let mut mock = Mock::connect(&server, "epoch-a");
    mock.ws
        .send(Message::text(
            r#"{"type":"ack","epoch":"e","seq":1,"capability":"x"}"#,
        ))
        .expect("send");
    wait_until("disconnect", || {
        let h = handle.health();
        h.state == LocalAvatarState::Disconnected && h.protocol_errors == 1
    });
}

#[test]
fn full_queue_drops_and_counts() {
    let (server, handle) = start(LocalAvatarConfig {
        queue_capacity: 3,
        ..config()
    });
    let mut mock = Mock::connect(&server, "epoch-a");
    mock.hello(); // never ready: nothing drains the queue
    wait_until("handshaking", || {
        handle.health().state == LocalAvatarState::Handshaking
    });
    let results: Vec<_> = (0..5).map(|_| handle.enqueue(viseme(Viseme::Aa))).collect();
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 3);
    assert_eq!(results[3], Err(DropReason::QueueFull));
    assert_eq!(handle.health().dropped_count(DropReason::QueueFull), 2);
}

#[test]
fn commands_whose_window_passed_are_dropped_not_sent() {
    let (server, handle) = start(LocalAvatarConfig {
        max_command_age: Duration::from_millis(50),
        ..config()
    });
    let mut mock = Mock::connect(&server, "epoch-a");
    mock.hello();
    wait_until("handshaking", || {
        handle.health().state == LocalAvatarState::Handshaking
    });
    handle.enqueue(viseme(Viseme::Aa)).expect("queued");
    std::thread::sleep(Duration::from_millis(150));
    mock.ready();
    assert_eq!(mock.recv().cmd, WireCommand::Reset {});
    assert!(mock.recv_within(Duration::from_millis(300)).is_none());
    wait_until("expired count", || {
        handle.health().dropped_count(DropReason::Expired) == 1
    });
}

#[test]
fn unresponsive_renderer_degrades_then_recovers_without_unbounded_growth() {
    let (server, handle) = start(LocalAvatarConfig {
        queue_capacity: 128,
        max_command_age: Duration::from_secs(30),
        ..config()
    });
    let mut mock = Mock::connect(&server, "epoch-a");
    mock.become_ready(&handle);
    for _ in 0..100 {
        handle.enqueue(viseme(Viseme::Aa)).expect("queued");
    }
    // Reset already in flight (1) + 63 more; the rest wait in the queue.
    let mut seqs = Vec::new();
    wait_until("degraded", || {
        handle.health().state == LocalAvatarState::Degraded
    });
    while let Some(frame) = mock.recv_within(Duration::from_millis(200)) {
        seqs.push(frame.seq);
    }
    assert_eq!(seqs.len(), 63);
    for seq in 1..=64 {
        mock.ack(seq);
    }
    wait_until("recovered", || handle.health().acked == 64);
    let mut rest = 0;
    while mock.recv_within(Duration::from_millis(300)).is_some() {
        rest += 1;
    }
    assert_eq!(rest, 37);
}

#[test]
fn renderer_down_never_blocks_enqueue() {
    let (server, handle) = start(config());
    // No renderer at all.
    let started = Instant::now();
    let mut worst = Duration::ZERO;
    for _ in 0..20_000 {
        let t = Instant::now();
        assert_eq!(
            handle.enqueue(viseme(Viseme::Aa)),
            Err(DropReason::NotReady)
        );
        worst = worst.max(t.elapsed());
    }
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "20k enqueues took {:?}",
        started.elapsed()
    );
    assert!(
        worst < Duration::from_millis(50),
        "slowest enqueue {worst:?}"
    );
    assert_eq!(handle.health().dropped_count(DropReason::NotReady), 20_000);

    // A connected-but-stalled renderer (never reads, never readies) too.
    let _stalled = Mock::connect(&server, "epoch-a");
    wait_until("handshaking", || {
        handle.health().state == LocalAvatarState::Handshaking
    });
    let started = Instant::now();
    for _ in 0..20_000 {
        let _ = handle.enqueue(viseme(Viseme::Aa));
    }
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "stalled: {:?}",
        started.elapsed()
    );
    let health = handle.health();
    assert_eq!(health.dropped_count(DropReason::QueueFull), 20_000 - 64);
}

#[test]
fn listens_on_loopback_only_and_serves_static_pages_safely() {
    let dir = std::env::temp_dir().join(format!("local-avatar-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("dir");
    std::fs::write(dir.join("index.html"), "<!doctype html><title>t</title>").expect("index");
    let secret_outside = dir
        .parent()
        .expect("parent")
        .join("local-avatar-outside.txt");
    std::fs::write(&secret_outside, "outside").expect("outside");

    let (server, _handle) = start(config().with_page_dir(&dir));
    let addr = server.local_addr();
    assert!(addr.ip().is_loopback());
    assert_eq!(addr.ip().to_string(), "127.0.0.1");
    assert_ne!(addr.port(), 0);

    let get = |path: &str, host: &str| -> String {
        let mut stream = TcpStream::connect(addr).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("timeout");
        write!(
            stream,
            "GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n"
        )
        .expect("write");
        let mut out = String::new();
        stream.read_to_string(&mut out).expect("read");
        out
    };
    let host = addr.to_string();
    let page = get("/", &host);
    assert!(page.starts_with("HTTP/1.1 200"), "{page}");
    assert!(page.contains("Content-Security-Policy"));
    assert!(page.contains("<title>t</title>"));
    assert!(get("/../local-avatar-outside.txt", &host).starts_with("HTTP/1.1 404"));
    assert!(get("/%2e%2e/local-avatar-outside.txt", &host).starts_with("HTTP/1.1 404"));
    assert!(get("/missing.js", &host).starts_with("HTTP/1.1 404"));
    // DNS-rebinding guard: a foreign Host header is refused.
    assert!(get("/", "evil.example").starts_with("HTTP/1.1 403"));

    let debug = format!("{server:?}");
    assert!(!debug.contains(server.token()), "token leaked into Debug");
    assert!(!format!("{:?}", server.handle()).contains(server.token()));
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_file(&secret_outside);
}

#[test]
fn tokens_are_per_launch_and_random() {
    let (a, _) = start(config());
    let (b, _) = start(config());
    assert_eq!(a.token().len(), 64);
    assert_ne!(a.token(), b.token());
}
