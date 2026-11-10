//! YouTube Live chat ingress against a loopback fake of the Data API (#293).
//! No network: the sidecar's real `ureq` client talks to a local socket.

use aivtuber_adapters::{
    PollEnd, UreqYoutubeHttp, YOUTUBE_DEFAULT_MAX_TEXT_CHARS, YoutubeApiKey, YoutubeChatPoller,
    YoutubePollerConfig, resolve_live_chat_id,
};
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

const KEY: &str = "AIzaSyFAKE-KEY-0123456789abcdefghijklmnop";

struct FakeApi {
    base: String,
    requests: Arc<Mutex<Vec<String>>>,
}

/// Serves `script` (status, body) in order, one connection per response, then
/// closes. Each recorded request is the raw head (request line and headers).
fn fake_api(script: Vec<(u16, String)>) -> FakeApi {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let base = format!("http://{}", listener.local_addr().unwrap());
    let requests = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&requests);
    thread::spawn(move || {
        for (status, body) in script {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut head = Vec::new();
            let mut buf = [0_u8; 1024];
            while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                match stream.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => head.extend_from_slice(&buf[..n]),
                }
            }
            seen.lock()
                .unwrap()
                .push(String::from_utf8_lossy(&head).into_owned());
            let _ = write!(
                stream,
                "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    FakeApi { base, requests }
}

fn text(id: &str, message: &str) -> Value {
    json!({
        "id": id,
        "snippet": {
            "type": "textMessageEvent",
            "publishedAt": "2026-11-10T12:00:00.5+00:00",
            "textMessageDetails": { "messageText": message }
        },
        "authorDetails": { "channelId": "UCaaaaaaaaaaaaaaaaaaaaaa", "displayName": "Secret Name" }
    })
}

fn page(items: Vec<Value>, token: &str, interval_ms: u64) -> (u16, String) {
    (
        200,
        json!({
            "nextPageToken": token,
            "pollingIntervalMillis": interval_ms,
            "items": items
        })
        .to_string(),
    )
}

fn error(status: u16, reason: &str) -> (u16, String) {
    (
        status,
        json!({ "error": { "code": status, "errors": [{ "reason": reason }] } }).to_string(),
    )
}

fn poller(
    api: &FakeApi,
    tweak: impl FnOnce(&mut YoutubePollerConfig),
) -> YoutubeChatPoller<UreqYoutubeHttp> {
    let mut config = YoutubePollerConfig::new(&api.base, "LIVECHATID123");
    config.emit_backlog = true;
    config.request_timeout = Duration::from_secs(5);
    tweak(&mut config);
    YoutubeChatPoller::new(
        UreqYoutubeHttp::default(),
        YoutubeApiKey::new(KEY).unwrap(),
        config,
    )
}

#[test]
fn paging_polling_interval_and_dedupe() {
    let api = fake_api(vec![
        page(vec![text("a", "one"), text("b", "two")], "TOK1", 7_000),
        // `b` is repeated by the server; `c` is new; the API asks for less
        // than the operator floor.
        page(vec![text("b", "two"), text("c", "three")], "TOK2", 1_000),
    ]);
    let mut poller = poller(&api, |_| {});

    let first = poller.poll_once();
    assert_eq!(first.records.len(), 2);
    // Never faster than the API's pollingIntervalMillis.
    assert_eq!(first.wait, Duration::from_millis(7_000));
    assert_eq!(first.end, None);

    let second = poller.poll_once();
    assert_eq!(second.records.len(), 1);
    assert!(second.records[0].contains("youtube-c"));
    // The operator floor (default 5 s) wins over a smaller API interval.
    assert_eq!(second.wait, Duration::from_millis(5_000));
    assert_eq!(poller.stats.duplicates, 1);

    let requests = api.requests.lock().unwrap();
    assert!(!requests[0].contains("pageToken"));
    assert!(requests[0].contains("liveChatId=LIVECHATID123"));
    assert!(requests[0].contains("part=snippet%2CauthorDetails"));
    assert!(requests[1].contains("pageToken=TOK1"));
}

#[test]
fn backlog_on_first_page_is_not_replayed_by_default() {
    let api = fake_api(vec![
        page(vec![text("old1", "old"), text("old2", "old")], "T1", 5_000),
        page(vec![text("new1", "fresh")], "T2", 5_000),
    ]);
    let mut poller = poller(&api, |c| c.emit_backlog = false);
    assert!(poller.poll_once().records.is_empty());
    assert_eq!(poller.stats.skipped_backlog, 2);
    let second = poller.poll_once();
    assert_eq!(second.records.len(), 1);
    assert!(second.records[0].contains("youtube-new1"));
}

#[test]
fn malformed_and_oversized_messages_are_rejected_and_counted() {
    let long = "x".repeat(YOUTUBE_DEFAULT_MAX_TEXT_CHARS + 1);
    let items = vec![
        json!({ "snippet": { "type": "textMessageEvent" } }), // no id
        json!({ "id": "no-snippet" }),
        json!({ "id": "no-text", "snippet": { "type": "textMessageEvent",
            "publishedAt": "2026-11-10T12:00:00+00:00" },
            "authorDetails": { "channelId": "UCx" } }),
        json!({ "id": "bad-type", "snippet": 7 }),
        text("too-long", &long),
        json!({ "id": "no-author", "snippet": { "type": "textMessageEvent",
            "publishedAt": "2026-11-10T12:00:00+00:00",
            "textMessageDetails": { "messageText": "hi" } } }),
        text("ok", "fine"),
    ];
    let api = fake_api(vec![page(items, "T", 5_000)]);
    let mut poller = poller(&api, |_| {});
    let step = poller.poll_once();
    assert_eq!(step.records.len(), 1);
    assert!(step.records[0].contains("youtube-ok"));
    assert_eq!(poller.stats.dropped_oversize, 1);
    assert_eq!(poller.stats.dropped_malformed, 5);
}

#[test]
fn paid_events_map_and_membership_is_counted_not_emitted() {
    let paid = json!({
        "id": "sc1",
        "snippet": { "type": "superChatEvent", "publishedAt": "2026-11-10T12:00:00+00:00",
            "superChatDetails": { "amountMicros": "1500000", "currency": "USD",
                "amountDisplayString": "$1.50", "userComment": "thanks", "tier": 1 } },
        "authorDetails": { "channelId": "UCpaid" }
    });
    let sticker = json!({
        "id": "ss1",
        "snippet": { "type": "superStickerEvent", "publishedAt": "2026-11-10T12:00:00+00:00",
            "superStickerDetails": { "amountMicros": "200000000", "currency": "JPY",
                "superStickerMetadata": { "altText": "heart" } } },
        "authorDetails": { "channelId": "UCpaid" }
    });
    let member = |id: &str, kind: &str| {
        json!({ "id": id, "snippet": { "type": kind, "publishedAt": "2026-11-10T12:00:00+00:00" },
            "authorDetails": { "channelId": "UCm" } })
    };
    let api = fake_api(vec![page(
        vec![
            paid,
            sticker,
            member("m1", "newSponsorEvent"),
            member("m2", "memberMilestoneChatEvent"),
            member("m3", "membershipGiftingEvent"),
            member("m4", "tombstone"),
        ],
        "T",
        5_000,
    )]);
    let mut poller = poller(&api, |_| {});
    let step = poller.poll_once();
    assert_eq!(step.records.len(), 2);
    let first: Value = serde_json::from_str(&step.records[0]).unwrap();
    assert_eq!(first["kind"], "chat.donation");
    assert_eq!(first["source_class"], "donation");
    assert_eq!(first["trust_level"], "untrusted");
    assert_eq!(first["payload"]["amount"], 1.5);
    assert_eq!(first["payload"]["currency"], "USD");
    assert_eq!(poller.stats.emitted_paid, 2);
    assert_eq!(poller.stats.membership_unmapped, 3);
    assert_eq!(poller.stats.not_chat, 1);
}

#[test]
fn quota_and_server_errors_back_off_exponentially_then_recover() {
    let api = fake_api(vec![
        error(403, "quotaExceeded"),
        error(403, "quotaExceeded"),
        error(503, "backendError"),
        error(503, "backendError"),
        page(vec![text("a", "back")], "T", 5_000),
        error(500, "backendError"),
    ]);
    let mut poller = poller(&api, |c| c.min_poll_ms = 0);
    assert_eq!(poller.poll_once().wait, Duration::from_secs(60));
    assert_eq!(poller.stats.quota_backoffs, 1);
    assert_eq!(poller.poll_once().wait, Duration::from_secs(120));
    // Failure count keeps growing across kinds; 5xx uses its own schedule.
    assert_eq!(poller.poll_once().wait, Duration::from_secs(8));
    assert_eq!(poller.poll_once().wait, Duration::from_secs(16));
    assert_eq!(poller.stats.transient_errors, 2);
    let ok = poller.poll_once();
    assert_eq!(ok.records.len(), 1);
    // Success resets the schedule.
    assert_eq!(poller.poll_once().wait, Duration::from_secs(2));
}

#[test]
fn chat_ended_stops_cleanly() {
    let api = fake_api(vec![error(403, "liveChatEnded")]);
    let mut p = poller(&api, |_| {});
    let step = p.poll_once();
    assert_eq!(step.end, Some(PollEnd::ChatEnded));
    assert!(step.records.is_empty());

    let ended_event = json!({ "id": "e1", "snippet": { "type": "chatEndedEvent",
        "publishedAt": "2026-11-10T12:00:00+00:00" } });
    let api = fake_api(vec![page(vec![text("a", "bye"), ended_event], "T", 5_000)]);
    let mut p = poller(&api, |_| {});
    let step = p.poll_once();
    assert_eq!(step.records.len(), 1);
    assert_eq!(step.end, Some(PollEnd::ChatEnded));

    let offline = (
        200,
        json!({ "offlineAt": "2026-11-10T13:00:00Z", "pollingIntervalMillis": 5000, "items": [] })
            .to_string(),
    );
    let api = fake_api(vec![offline]);
    assert_eq!(
        poller(&api, |_| {}).poll_once().end,
        Some(PollEnd::ChatEnded)
    );
}

#[test]
fn misconfiguration_is_fatal_and_does_not_retry() {
    for (status, reason) in [
        (400, "keyInvalid"),
        (404, "liveChatNotFound"),
        (403, "forbidden"),
    ] {
        let api = fake_api(vec![error(status, reason)]);
        let step = poller(&api, |_| {}).poll_once();
        let Some(PollEnd::Fatal(message)) = step.end else {
            panic!("expected fatal for {status}");
        };
        assert!(message.contains(&status.to_string()));
        assert!(!message.contains(KEY));
    }
}

#[test]
fn malformed_response_body_backs_off_and_is_counted() {
    let api = fake_api(vec![(200, "<html>not json</html>".to_owned())]);
    let mut p = poller(&api, |c| c.min_poll_ms = 0);
    let step = p.poll_once();
    assert_eq!(step.wait, Duration::from_secs(2));
    assert_eq!(p.stats.malformed_responses, 1);
    assert_eq!(step.end, None);
}

#[test]
fn api_key_is_a_header_never_in_the_url_or_debug_output() {
    let api = fake_api(vec![page(vec![text("a", "hi")], "T", 5_000)]);
    let mut p = poller(&api, |_| {});
    let debug = format!("{p:?}");
    assert!(!debug.contains(KEY), "{debug}");
    assert!(debug.contains("<redacted>"));
    p.poll_once();
    let requests = api.requests.lock().unwrap();
    let head = requests[0].to_ascii_lowercase();
    let request_line = head.lines().next().unwrap();
    assert!(!request_line.contains(&KEY.to_ascii_lowercase()));
    assert!(!request_line.contains("key="));
    assert!(head.contains(&format!("x-goog-api-key: {}", KEY.to_ascii_lowercase())));
}

#[test]
fn key_is_scrubbed_from_transport_errors() {
    // Nothing listens here: the connect error must not echo the key even if a
    // lower layer included it.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    let mut p = poller(
        &FakeApi {
            base,
            requests: Arc::default(),
        },
        |c| c.min_poll_ms = 0,
    );
    let step = p.poll_once();
    assert_eq!(step.wait, Duration::from_secs(2));
    assert_eq!(p.stats.transient_errors, 1);
    let key = YoutubeApiKey::new(KEY).unwrap();
    assert_eq!(
        key.redact(&format!("GET /x?key={KEY} failed")),
        "GET /x?key=[REDACTED] failed"
    );
    assert_eq!(format!("{key:?}"), "YoutubeApiKey(<redacted>)");
}

#[test]
fn insecure_remote_endpoint_is_refused_before_any_io() {
    let mut config = YoutubePollerConfig::new("http://example.com", "CHAT");
    config.min_poll_ms = 0;
    let mut p = YoutubeChatPoller::new(
        UreqYoutubeHttp::default(),
        YoutubeApiKey::new(KEY).unwrap(),
        config,
    );
    let step = p.poll_once();
    assert!(step.records.is_empty());
    assert_eq!(p.stats.transient_errors, 1);
}

#[test]
fn video_id_resolves_to_active_live_chat_id() {
    let body =
        json!({ "items": [{ "liveStreamingDetails": { "activeLiveChatId": "CHAT-ID_1=" } }] });
    let api = fake_api(vec![(200, body.to_string())]);
    let key = YoutubeApiKey::new(KEY).unwrap();
    let id = resolve_live_chat_id(
        &UreqYoutubeHttp::default(),
        &api.base,
        &key,
        "dQw4w9WgXcQ",
        Duration::from_secs(5),
    )
    .unwrap();
    assert_eq!(id, "CHAT-ID_1=");
    let requests = api.requests.lock().unwrap();
    assert!(requests[0].contains("part=liveStreamingDetails"));
    assert!(requests[0].contains("id=dQw4w9WgXcQ"));

    // Not live: no activeLiveChatId; and an invalid id never reaches the network.
    let api = fake_api(vec![(
        200,
        json!({ "items": [{ "liveStreamingDetails": {} }] }).to_string(),
    )]);
    assert!(
        resolve_live_chat_id(
            &UreqYoutubeHttp::default(),
            &api.base,
            &key,
            "dQw4w9WgXcQ",
            Duration::from_secs(5)
        )
        .unwrap_err()
        .contains("no active live chat")
    );
    assert!(
        resolve_live_chat_id(
            &UreqYoutubeHttp::default(),
            &api.base,
            &key,
            "bad id",
            Duration::from_secs(1)
        )
        .is_err()
    );
}
