//! App-level tests of the performer's recent turns (#80 S1): off by default and
//! byte-identical, an empty ring still asks for an honest answer, and no
//! viewer text ever enters the ring.

use super::*;
use aivtuber_domain::{ThinkingContextSource, ThinkingRequest};

fn app_with(thinking: &CapturingThinking) -> ProductionApp<FixedGenerateRoute> {
    generation_app(
        FixedGenerateRoute {
            reply_context: ReflexContext::default(),
            fallback_variant_group: None,
        },
        thinking.clone(),
        MockTts::default(),
        SecretRedactor::default(),
        None,
        RecordingAudio::default(),
        RecordingAvatar::default(),
    )
}

fn chat_turn<R: RoutePlanner>(app: &mut ProductionApp<R>, sequence: u64, text: &str, now_ms: u64) {
    let mut event = chat_event(sequence);
    event.payload.insert("text".to_owned(), json!(text));
    let raw = serde_json::to_vec(&event).expect("event json");
    app.process_content_bytes(&raw, now_ms, sequence)
        .expect("generation submission");
    wait_for_generation_completion(app, now_ms);
}

fn last_request(thinking: &CapturingThinking) -> ThinkingRequest {
    thinking
        .requests
        .lock()
        .expect("requests")
        .last()
        .expect("a thinking request")
        .clone()
}

fn recent_item(request: &ThinkingRequest) -> Option<String> {
    request
        .curated_context
        .iter()
        .find(|item| item.text.starts_with("recent_turns:"))
        .map(|item| item.text.clone())
}

#[test]
fn with_the_flag_off_the_request_has_no_recent_turns() {
    let thinking = CapturingThinking::default();
    let mut app = app_with(&thinking);
    app.startup().expect("startup");
    chat_turn(&mut app, 60, "hello there", 1_000);
    let request = last_request(&thinking);
    assert!(request.curated_context.is_empty());
    assert!(
        !serde_json::to_string(&request)
            .unwrap()
            .contains("recent_turns")
    );
    assert!(app.recent_turns().is_none());
}

#[test]
fn an_empty_ring_still_sends_the_none_item_so_the_answer_is_honest() {
    let thinking = CapturingThinking::default();
    let mut app = app_with(&thinking).with_recent_turns();
    app.startup().expect("startup");
    chat_turn(&mut app, 61, "さっき何の話してたっけ？", 1_000);
    let request = last_request(&thinking);
    request.validate().expect("valid request");
    assert_eq!(recent_item(&request).as_deref(), Some("recent_turns: none"));
    let item = &request.curated_context[0];
    assert_eq!(item.source, ThinkingContextSource::RecentInteraction);
}

#[test]
fn the_ring_holds_only_the_performers_own_words_never_the_viewers() {
    let thinking = CapturingThinking::default();
    let mut app = app_with(&thinking).with_recent_turns();
    app.startup().expect("startup");
    chat_turn(&mut app, 62, "ZZVIEWERSECRETZZ", 1_000);
    chat_turn(&mut app, 63, "YYSECONDVIEWERYY", 2_000);
    let ring = app.recent_turns().expect("composed");
    assert!(
        !ring.is_empty(),
        "the first reply should have been recorded"
    );
    let request = last_request(&thinking);
    let item = recent_item(&request).expect("recent item");
    assert!(item.contains("chat_reply: "));
    for viewer in ["ZZVIEWERSECRETZZ", "YYSECONDVIEWERYY"] {
        assert!(!item.contains(viewer));
    }
}
