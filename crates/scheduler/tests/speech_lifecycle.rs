//! Speech lifecycle truth owned by the scheduler (issue #222 S2, ADR-0004 D5).
//!
//! These tests use only the scheduler's public surface and the domain
//! `SpeechLifecycleEvent`: a consumer (#81 floor control, #83 phrase streaming)
//! never needs a provider event.

use aivtuber_domain::{
    SpeechCancelReason, SpeechLifecycleEvent, SpeechPhase, SpeechReached, SpeechTimeBasis,
};
use aivtuber_scheduler::{
    BlendChannel, OutputFailureKind, OutputObservation, PhraseLink, PlannedPerformance, Priority,
    Rejection, Scheduler, SchedulerConfig, SpeechPrepared,
};
use std::collections::BTreeSet;

fn scheduler() -> Scheduler {
    Scheduler::new(SchedulerConfig {
        min_reaction_spacing_ms: 0,
        ..SchedulerConfig::default()
    })
}

fn audio(
    event_id: &str,
    asset_id: &str,
    priority: Priority,
    start_at_ms: u64,
    duration_ms: u64,
    points: &[u64],
) -> PlannedPerformance {
    PlannedPerformance {
        event_id: event_id.to_owned(),
        asset_id: asset_id.to_owned(),
        priority,
        interruptible: !points.is_empty(),
        interrupt_points_ms: points.to_vec(),
        start_at_ms,
        duration_ms,
        generation: 0,
        exclusive: false,
        channels: BTreeSet::from([BlendChannel::Audio]),
    }
}

fn prepared(event_id: &str, asset_id: &str) -> SpeechPrepared {
    SpeechPrepared {
        event_id: event_id.to_owned(),
        asset_id: asset_id.to_owned(),
        phrase: 0,
        final_phrase: true,
        speech_text_digest: Some("d".repeat(64)),
        projection: Some("rules:1|lexicon:none".to_owned()),
    }
}

fn drain(scheduler: &mut Scheduler) -> Vec<SpeechLifecycleEvent> {
    let events = scheduler.drain_speech_lifecycle(usize::MAX);
    for event in &events {
        event.validate().expect("every emitted event is valid");
    }
    events
}

fn of<'a>(events: &'a [SpeechLifecycleEvent], event_id: &str) -> Vec<&'a SpeechLifecycleEvent> {
    events
        .iter()
        .filter(|event| event.speech.event_id == event_id)
        .collect()
}

fn phases(events: &[&SpeechLifecycleEvent]) -> Vec<SpeechPhase> {
    events.iter().map(|event| event.phase).collect()
}

fn cancelled(
    reason: SpeechCancelReason,
    reached: SpeechReached,
    spoken_ms: Option<u64>,
) -> SpeechPhase {
    SpeechPhase::Cancelled {
        reason,
        reached,
        spoken_ms,
    }
}

#[test]
fn a_spoken_unit_walks_every_phase_in_order_and_seq_only_grows() {
    let mut s = scheduler();
    s.note_speech_prepared(0, prepared("e1", "asset.a"));
    s.schedule(audio(
        "e1",
        "asset.a",
        Priority::Conversation,
        0,
        1000,
        &[300, 700, 1000],
    ))
    .unwrap();
    s.advance_to(500);
    s.advance_to(1200);
    let events = drain(&mut s);
    let unit = of(&events, "e1");
    assert_eq!(
        phases(&unit),
        vec![
            SpeechPhase::Prepared,
            SpeechPhase::Committed,
            SpeechPhase::Started,
            SpeechPhase::Boundary { offset_ms: 300 },
            SpeechPhase::Boundary { offset_ms: 700 },
            SpeechPhase::Finished,
        ],
        "the end point (1000) is Finished, not a boundary"
    );
    assert!(events.windows(2).all(|pair| pair[0].seq < pair[1].seq));
    assert_eq!(unit[0].speech.generation, None);
    assert!(unit[1..].iter().all(|e| e.speech.generation == Some(1)));
    assert_eq!(unit[3].at_ms, 300);
    assert_eq!(unit[4].at_ms, 700);
    assert_eq!(unit[5].at_ms, 1000);
    // The reporter's digest and projection identity ride along; never text.
    assert_eq!(
        unit[0].speech.speech_text_digest.as_deref(),
        Some(&*"d".repeat(64))
    );
    assert_eq!(
        unit[5].speech.projection.as_deref(),
        Some("rules:1|lexicon:none")
    );
    assert!(
        unit.iter()
            .all(|e| e.basis == SpeechTimeBasis::LogicalTimeline)
    );
    let metrics = s.speech_lifecycle().metrics();
    assert_eq!(
        (metrics.finished, metrics.live, metrics.rejected),
        (1, 0, 0)
    );
}

#[test]
fn tts_completion_alone_is_prepared_and_never_finished() {
    let mut s = scheduler();
    s.note_speech_prepared(0, prepared("e1", "asset.a"));
    s.advance_to(10_000);
    let events = drain(&mut s);
    assert_eq!(phases(&of(&events, "e1")), vec![SpeechPhase::Prepared]);
    let snapshot = s.speech_lifecycle().snapshot();
    assert_eq!(snapshot.live.len(), 1, "still only prepared, not spoken");
}

#[test]
fn a_cached_unit_without_a_report_is_prepared_implicitly_at_commit() {
    let mut s = scheduler();
    s.schedule(audio(
        "cached",
        "starter.hello",
        Priority::Conversation,
        0,
        400,
        &[],
    ))
    .unwrap();
    s.complete_all();
    let events = drain(&mut s);
    let unit = of(&events, "cached");
    assert_eq!(unit[0].phase, SpeechPhase::Prepared);
    assert_eq!(unit[0].speech.speech_text_digest, None);
    assert_eq!(unit.last().unwrap().phase, SpeechPhase::Finished);
}

#[test]
fn items_without_an_audio_channel_are_not_speech() {
    let mut s = scheduler();
    let mut face = audio("face", "face.a", Priority::Conversation, 0, 200, &[]);
    face.channels = BTreeSet::from([BlendChannel::Face]);
    s.schedule(face).unwrap();
    s.complete_all();
    assert!(drain(&mut s).is_empty());
}

#[test]
fn every_cancel_source_records_its_reason_and_how_far_the_unit_got() {
    // Operator stop while playing: reached started, spoken duration recorded.
    let mut s = scheduler();
    s.schedule(audio("e", "a", Priority::Conversation, 0, 1000, &[500]))
        .unwrap();
    s.advance_to(100);
    s.stop_all(400);
    let events = drain(&mut s);
    assert_eq!(
        of(&events, "e").last().unwrap().phase,
        cancelled(
            SpeechCancelReason::OperatorStop,
            SpeechReached::Started,
            Some(400)
        )
    );

    // Operator stop while still queued: reached committed.
    let mut s = scheduler();
    s.schedule_at(0, audio("e", "a", Priority::Conversation, 5000, 500, &[]))
        .unwrap();
    s.stop_all(100);
    let events = drain(&mut s);
    assert_eq!(
        phases(&of(&events, "e")),
        vec![
            SpeechPhase::Prepared,
            SpeechPhase::Committed,
            cancelled(
                SpeechCancelReason::OperatorStop,
                SpeechReached::Committed,
                None
            ),
        ]
    );

    // Shutdown is the same scheduling effect with its own reason.
    let mut s = scheduler();
    s.schedule(audio("e", "a", Priority::Conversation, 0, 1000, &[]))
        .unwrap();
    s.stop_all_for(250, SpeechCancelReason::Shutdown);
    let events = drain(&mut s);
    assert_eq!(
        of(&events, "e").last().unwrap().phase,
        cancelled(
            SpeechCancelReason::Shutdown,
            SpeechReached::Started,
            Some(250)
        )
    );

    // Confirmed barge-in: playing and queued units both cancelled as BargeIn.
    let mut s = scheduler();
    s.schedule(audio("play", "a", Priority::Conversation, 0, 1000, &[500]))
        .unwrap();
    s.schedule_at(
        0,
        audio("later", "b", Priority::Conversation, 4000, 500, &[250]),
    )
    .unwrap();
    let stop = s.stop_speaking_for(Priority::Conversation, 300);
    assert_eq!((stop.playing.len(), stop.queued.len()), (1, 1));
    let events = drain(&mut s);
    assert_eq!(
        of(&events, "play").last().unwrap().phase,
        cancelled(
            SpeechCancelReason::BargeIn,
            SpeechReached::Started,
            Some(300)
        )
    );
    assert_eq!(
        of(&events, "later").last().unwrap().phase,
        cancelled(SpeechCancelReason::BargeIn, SpeechReached::Committed, None)
    );

    // Higher-priority work takes the timeline at the declared safe point.
    let mut s = scheduler();
    s.schedule(audio(
        "low",
        "a",
        Priority::Conversation,
        0,
        1000,
        &[400, 1000],
    ))
    .unwrap();
    s.advance_to(100);
    s.schedule_at(100, audio("op", "b", Priority::Operator, 100, 300, &[]))
        .unwrap();
    s.advance_to(5000);
    let events = drain(&mut s);
    let low = of(&events, "low");
    assert_eq!(
        phases(&low[3..]),
        vec![
            SpeechPhase::Boundary { offset_ms: 400 },
            cancelled(
                SpeechCancelReason::Preempted,
                SpeechReached::Started,
                Some(400)
            ),
        ],
        "the safe point is reported, then the preemption at it"
    );
    assert_eq!(
        of(&events, "op").last().unwrap().phase,
        SpeechPhase::Finished
    );

    // Queued work displaced by higher-priority work.
    let mut s = scheduler();
    s.schedule_at(
        0,
        audio("queued", "a", Priority::Commentary, 5000, 500, &[]),
    )
    .unwrap();
    s.schedule_at(0, audio("op", "b", Priority::Operator, 5000, 500, &[]))
        .unwrap();
    let events = drain(&mut s);
    assert_eq!(
        of(&events, "queued").last().unwrap().phase,
        cancelled(
            SpeechCancelReason::Superseded,
            SpeechReached::Committed,
            None
        )
    );

    // The scheduler refusing a prepared plan: cancelled before commit.
    let mut s = Scheduler::new(SchedulerConfig::default());
    s.schedule(audio(
        "first",
        "asset.same",
        Priority::Conversation,
        0,
        500,
        &[],
    ))
    .unwrap();
    s.note_speech_prepared(10, prepared("second", "asset.same"));
    assert_eq!(
        s.schedule_at(
            10,
            audio("second", "asset.same", Priority::Conversation, 10, 500, &[])
        ),
        Err(Rejection::Cooldown)
    );
    let events = drain(&mut s);
    let second = of(&events, "second");
    assert_eq!(
        phases(&second),
        vec![
            SpeechPhase::Prepared,
            cancelled(
                SpeechCancelReason::SchedulerRejected,
                SpeechReached::BeforeCommit,
                None
            ),
        ]
    );
    assert_eq!(second[1].speech.generation, None);

    // A duplicate event id is refused without touching the live unit's life.
    let mut s = scheduler();
    s.schedule(audio("dup", "a", Priority::Conversation, 0, 500, &[]))
        .unwrap();
    s.note_speech_prepared(0, prepared("dup", "a"));
    assert_eq!(
        s.schedule(audio("dup", "b", Priority::Conversation, 0, 500, &[])),
        Err(Rejection::DuplicateEvent)
    );
    s.complete_all();
    let events = drain(&mut s);
    assert_eq!(
        phases(&of(&events, "dup")),
        vec![
            SpeechPhase::Prepared,
            SpeechPhase::Committed,
            SpeechPhase::Started,
            SpeechPhase::Finished
        ]
    );

    // After synthesis but before commit: gate rejection and generation cancel.
    let mut s = scheduler();
    s.note_speech_prepared(0, prepared("gate", "a"));
    s.note_speech_prepared(0, prepared("gen", "b"));
    s.cancel_prepared_speech(5, "gate", SpeechCancelReason::GateRejected);
    s.cancel_prepared_speech(5, "gen", SpeechCancelReason::GenerationCancelled);
    let events = drain(&mut s);
    assert_eq!(
        of(&events, "gate").last().unwrap().phase,
        cancelled(
            SpeechCancelReason::GateRejected,
            SpeechReached::BeforeCommit,
            None
        )
    );
    assert_eq!(
        of(&events, "gen").last().unwrap().phase,
        cancelled(
            SpeechCancelReason::GenerationCancelled,
            SpeechReached::BeforeCommit,
            None
        )
    );
}

#[test]
fn failed_muted_and_late_output_cancel_the_unit_and_it_is_never_finished() {
    for (kind, reason) in [
        (
            OutputFailureKind::AdapterError,
            SpeechCancelReason::OutputFailure,
        ),
        (OutputFailureKind::Muted, SpeechCancelReason::Muted),
        (
            OutputFailureKind::StaleOrLate,
            SpeechCancelReason::StaleOrLate,
        ),
    ] {
        let mut s = scheduler();
        let plan = s
            .schedule(audio("e", "a", Priority::Conversation, 0, 1000, &[]))
            .unwrap();
        s.advance_to(200);
        s.report_speech_output(plan.generation, OutputObservation::Failed(kind), 200);
        // The performer's own timeline runs to its natural end afterwards.
        s.complete_all();
        let events = drain(&mut s);
        let unit = of(&events, "e");
        assert_eq!(
            unit.last().unwrap().phase,
            cancelled(reason, SpeechReached::Started, Some(200)),
            "{kind:?}"
        );
        assert!(
            !unit.iter().any(|e| e.phase == SpeechPhase::Finished),
            "{kind:?}: a failed command is never finished"
        );
        assert!(s.speech_lifecycle().metrics().suppressed_after_terminal >= 1);
    }
}

#[test]
fn accepted_and_drained_output_only_strengthen_the_finished_basis() {
    let mut s = scheduler();
    let a = s
        .schedule(audio("accepted", "a", Priority::Conversation, 0, 100, &[]))
        .unwrap();
    s.report_speech_output(a.generation, OutputObservation::Accepted, 0);
    let b = s
        .schedule_at(
            0,
            audio("drained", "b", Priority::Conversation, 200, 100, &[]),
        )
        .unwrap();
    s.advance_to(250);
    s.report_speech_output(b.generation, OutputObservation::Accepted, 250);
    s.report_speech_output(b.generation, OutputObservation::Drained, 260);
    s.complete_all();
    let events = drain(&mut s);
    let basis = |id: &str| of(&events, id).last().unwrap().basis;
    assert_eq!(basis("accepted"), SpeechTimeBasis::OutputAccepted);
    assert_eq!(basis("drained"), SpeechTimeBasis::OutputDrained);
}

#[test]
fn operator_mute_cancels_every_live_unit_without_touching_the_timeline() {
    let mut s = scheduler();
    s.schedule(audio("play", "a", Priority::Conversation, 0, 1000, &[]))
        .unwrap();
    s.schedule_at(
        0,
        audio("next", "b", Priority::Conversation, 3000, 500, &[]),
    )
    .unwrap();
    s.advance_to(100);
    s.cancel_all_speech(100, SpeechCancelReason::Muted);
    assert_eq!(s.items().len(), 2, "muting does not stop the performer");
    s.complete_all();
    let events = drain(&mut s);
    for id in ["play", "next"] {
        let unit = of(&events, id);
        assert!(matches!(
            unit.last().unwrap().phase,
            SpeechPhase::Cancelled {
                reason: SpeechCancelReason::Muted,
                ..
            }
        ));
        assert!(!unit.iter().any(|e| e.phase == SpeechPhase::Finished));
    }
}

fn phrase(response_id: u64, seq: u32, final_phrase: bool) -> PhraseLink {
    PhraseLink {
        response_id,
        seq,
        final_phrase,
    }
}

fn phrase_prepared(root: &str, link: PhraseLink) -> SpeechPrepared {
    SpeechPrepared {
        event_id: link.plan_event_id(root),
        asset_id: format!("asset.p{}", link.seq),
        phrase: link.seq,
        final_phrase: link.final_phrase,
        speech_text_digest: Some("c".repeat(64)),
        projection: None,
    }
}

#[test]
fn a_three_phrase_reply_reports_phrase_sequence_and_final_only_on_the_last() {
    let mut s = scheduler();
    for seq in 0..3 {
        let link = phrase(5, seq, seq == 2);
        s.note_speech_prepared(0, phrase_prepared("root", link));
        let plan = audio(
            &link.plan_event_id("root"),
            &format!("asset.p{seq}"),
            Priority::Conversation,
            0,
            300,
            &[],
        );
        s.schedule_phrase_at(0, plan, link).unwrap();
    }
    s.complete_all();
    let events = drain(&mut s);
    let started: Vec<_> = events
        .iter()
        .filter(|e| e.phase == SpeechPhase::Started)
        .map(|e| (e.speech.phrase, e.speech.final_phrase, e.at_ms))
        .collect();
    assert_eq!(
        started,
        vec![(0, false, 0), (1, false, 300), (2, true, 600)]
    );
    // A hand-off at one instant reads as finished-then-started.
    let at_300: Vec<_> = events
        .iter()
        .filter(|e| {
            e.at_ms == 300 && !matches!(e.phase, SpeechPhase::Prepared | SpeechPhase::Committed)
        })
        .map(|e| (e.speech.phrase, e.phase))
        .collect();
    assert_eq!(
        at_300,
        vec![(0, SpeechPhase::Finished), (1, SpeechPhase::Started)]
    );
    assert_eq!(
        events
            .iter()
            .filter(|e| e.phase == SpeechPhase::Finished)
            .map(|e| (e.speech.phrase, e.speech.final_phrase))
            .collect::<Vec<_>>(),
        vec![(0, false), (1, false), (2, true)]
    );
    assert!(events.windows(2).all(|pair| pair[0].seq < pair[1].seq));
}

#[test]
fn a_failed_middle_phrase_leaves_the_first_finished_and_the_last_never_spoken() {
    let mut s = scheduler();
    let p0 = phrase(7, 0, false);
    s.note_speech_prepared(0, phrase_prepared("root", p0));
    s.schedule_phrase_at(
        0,
        audio("root", "asset.p0", Priority::Conversation, 0, 300, &[]),
        p0,
    )
    .unwrap();

    // Phrase 1 fails its output gate after synthesis.
    let p1 = phrase(7, 1, false);
    s.note_speech_prepared(0, phrase_prepared("root", p1));
    s.cancel_prepared_speech(
        0,
        &p1.plan_event_id("root"),
        SpeechCancelReason::GateRejected,
    );

    // Phrase 2 cannot continue a response whose phrase 1 never committed.
    let p2 = phrase(7, 2, true);
    s.note_speech_prepared(0, phrase_prepared("root", p2));
    assert_eq!(
        s.schedule_phrase_at(
            0,
            audio(
                &p2.plan_event_id("root"),
                "asset.p2",
                Priority::Conversation,
                0,
                300,
                &[]
            ),
            p2
        ),
        Err(Rejection::ContinuationBroken)
    );
    s.complete_all();
    let events = drain(&mut s);
    assert_eq!(
        of(&events, "root").last().unwrap().phase,
        SpeechPhase::Finished
    );
    let p1_events = of(&events, "root#p1");
    assert_eq!(
        p1_events.last().unwrap().phase,
        cancelled(
            SpeechCancelReason::GateRejected,
            SpeechReached::BeforeCommit,
            None
        )
    );
    let p2_events = of(&events, "root#p2");
    assert_eq!(
        phases(&p2_events),
        vec![
            SpeechPhase::Prepared,
            cancelled(
                SpeechCancelReason::SchedulerRejected,
                SpeechReached::BeforeCommit,
                None
            ),
        ]
    );
    assert!(p2_events.iter().all(|e| e.speech.final_phrase));
    assert!(
        !events.iter().any(|e| e.speech.phrase > 0
            && matches!(e.phase, SpeechPhase::Started | SpeechPhase::Finished)),
        "a cancelled future phrase is never reported as spoken"
    );
}

/// Two provider adapters with different native signals normalize to the same
/// `OutputObservation`s, so a consumer sees identical lifecycle sequences.
#[test]
fn two_provider_styles_yield_identical_lifecycles_for_the_same_scenario() {
    enum NativeSignal {
        ProcessSpawned,
        ProcessExited,
        VendorStarted,
        VendorFinishedCallback,
    }
    fn normalize(signal: NativeSignal) -> OutputObservation {
        match signal {
            NativeSignal::ProcessSpawned | NativeSignal::VendorStarted => {
                OutputObservation::Accepted
            }
            NativeSignal::ProcessExited | NativeSignal::VendorFinishedCallback => {
                OutputObservation::Drained
            }
        }
    }
    let run = |signals: [NativeSignal; 2]| {
        let mut s = scheduler();
        let plan = s
            .schedule(audio("e", "a", Priority::Conversation, 0, 500, &[250]))
            .unwrap();
        let [first, second] = signals;
        s.report_speech_output(plan.generation, normalize(first), 0);
        s.advance_to(250);
        s.report_speech_output(plan.generation, normalize(second), 250);
        s.complete_all();
        drain(&mut s)
    };
    let process = run([NativeSignal::ProcessSpawned, NativeSignal::ProcessExited]);
    let vendor = run([
        NativeSignal::VendorStarted,
        NativeSignal::VendorFinishedCallback,
    ]);
    assert_eq!(process, vendor);
    assert_eq!(
        process.last().unwrap().basis,
        SpeechTimeBasis::OutputDrained
    );
}

#[test]
fn a_stray_report_for_a_retired_generation_is_counted_and_changes_nothing() {
    let mut s = scheduler();
    let plan = s
        .schedule(audio("e", "a", Priority::Conversation, 0, 100, &[]))
        .unwrap();
    s.complete_all();
    let before = drain(&mut s);
    assert_eq!(before.last().unwrap().phase, SpeechPhase::Finished);
    s.report_speech_output(
        plan.generation,
        OutputObservation::Failed(OutputFailureKind::AdapterError),
        500,
    );
    assert!(drain(&mut s).is_empty());
    assert_eq!(s.speech_lifecycle().metrics().late_output_reports, 1);
}

#[test]
fn the_outbound_queue_is_bounded_and_drops_are_visible_as_seq_gaps() {
    let mut s = scheduler();
    // Far more units than the queue holds, never drained.
    for index in 0..1200u64 {
        let id = format!("u{index}");
        s.schedule_at(
            index * 10,
            audio(
                &id,
                &format!("asset.{index}"),
                Priority::Conversation,
                index * 10,
                5,
                &[],
            ),
        )
        .unwrap();
    }
    s.complete_all();
    let metrics = s.speech_lifecycle().metrics();
    assert!(metrics.queued <= 1024, "queue is bounded");
    assert!(metrics.dropped > 0, "overflow is counted");
    let events = s.drain_speech_lifecycle(usize::MAX);
    assert!(events.len() <= 1024);
    assert!(
        events.first().unwrap().seq > 1,
        "the consumer sees the loss as a seq gap"
    );
}
