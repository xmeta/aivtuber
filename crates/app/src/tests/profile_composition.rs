use super::*;

fn profile_app() -> ProductionApp<Box<dyn RoutePlanner>> {
    app(
        Box::new(FixedSilentRoute) as Box<dyn RoutePlanner>,
        Box::new(RecordingAudio::default()),
        Box::new(RecordingAvatar::default()),
        Box::new(NoopStreamOutput),
    )
}

#[test]
fn cached_profile_composes_intent_planner_behind_dyn_planner() {
    // The cached profile composes the deterministic IntentRoutePlanner
    // behind the same `Box<dyn RoutePlanner>` the executable uses.
    let boxed: Box<dyn RoutePlanner> = Box::new(IntentRoutePlanner);
    let mut app = app(
        boxed,
        Box::new(RecordingAudio::default()),
        Box::new(RecordingAvatar::default()),
        Box::new(NoopStreamOutput),
    );
    app.startup().expect("startup");

    let mut event = chat_event(70);
    event.payload.insert(
        "intent".to_owned(),
        serde_json::Value::String("reaction.agree".to_owned()),
    );
    let raw = serde_json::to_vec(&event).expect("event json");
    let outcome = app
        .process_content_bytes(&raw, 0, 70)
        .expect("cached profile routing");
    // IntentRoutePlanner routes by intent only; without a matching
    // cached asset resolution the route degrades deterministically.
    let _ = outcome.playback;
}

#[test]
fn box_dyn_route_planner_forwards_decision_and_fallback_accessors() {
    let mut boxed: Box<dyn RoutePlanner> = Box::new(template_planner());
    let mut event = chat_event(71);
    event.payload.insert(
        "template_id".to_owned(),
        serde_json::Value::String("thanks.donation".to_owned()),
    );
    event.payload.insert(
        "name".to_owned(),
        serde_json::Value::String("テスター".to_owned()),
    );
    // The forwarding impl must expose the same accessors as the inner
    // planner so executables can treat either composition uniformly.
    assert!(boxed.route(&event).is_ok());
    assert!(boxed.decision_record().is_some());
    assert_eq!(boxed.template_fallback(), None);
}

#[test]
fn profile_fingerprint_is_recorded_for_each_profile() {
    for profile in CompositionProfile::ALL {
        let fingerprint = config_fingerprint(profile, profile.generative_route());
        assert!(fingerprint.starts_with("profile-v1-"));
        let summary =
            ProfileSummary::for_profile(profile, profile.generative_route(), fingerprint.clone());
        assert_eq!(summary.config_fingerprint, fingerprint);
        assert!(summary.log_line().contains(&fingerprint));
    }
}

#[test]
fn profile_validation_gates_match_composed_route_graph() {
    // The executable composes the generative runtime only when the
    // profile can reach it; validate() must enforce the same rule.
    CompositionProfile::Cached
        .validate(false, false)
        .expect("cached needs nothing");
    CompositionProfile::Reflex
        .validate(false, true)
        .expect("reflex needs jev credentials");
    CompositionProfile::Full
        .validate(true, true)
        .expect("full needs jev + generative configuration");
    for profile in [CompositionProfile::Reflex, CompositionProfile::Full] {
        assert!(profile.validate(false, false).is_err());
    }
}

#[test]
fn silent_startup_app_composes_and_shuts_down_under_any_profile_choice() {
    // Every profile shares one ProductionApp; the planner is the only
    // profile-specific component. Verify composition + shutdown works
    // with the boxed planner the executables actually pass.
    let mut app = profile_app();
    app.startup().expect("startup");
    let event = chat_event(72);
    let raw = serde_json::to_vec(&event).expect("event json");
    app.process_content_bytes(&raw, 0, 72).expect("content");
    app.shutdown(1_000);
}
