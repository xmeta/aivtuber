//! Scene context: the ranked, bounded answer to "what is the stream showing"
//! (issue #88 S1, ADR-0007 D-3 and D-10).
//!
//! [`SceneContextTracker`] joins two sources and nothing else:
//! * **rank 2, known OBS state.** The composition root reports the OBS program
//!   scene name; the operator-owned [`ObsSceneMap`] turns it into a scene class.
//!   The name is only a lookup key: it is never retained, copied into a
//!   snapshot or logged, and a name the operator did not map is silence, not a
//!   guess.
//! * **rank 4, perception.** The existing [`PerceptionTracker::resolve_scene`]
//!   applies the D-3 hierarchy per field: a higher-ranked value always wins, a
//!   perceived value fills a field only where every higher source is silent.
//!
//! Rules enforced here, not left to callers:
//! * **Chat can never assert a scene.** No method takes an event, a viewer
//!   field or any text from the content plane; the only inputs are the OBS
//!   scene name the composition root read from OBS and the perception tracker.
//! * **Only rank 1 to 3 may affect scheduling.** [`Self::scheduling_interruptibility`]
//!   is the single accessor the busy/deferral and floor consumers use. A
//!   perceived scene (rank 4) is steerable by on-screen text, so it informs a
//!   Reflex or Thinking request and can never mark the performer busy, defer a
//!   reply or declare a moment uninterruptible; the snapshot type refuses to
//!   carry that combination at all.
//! * **Typed transitions on class change only.** A refreshed observation, a
//!   label change or a confidence change bumps the snapshot version but is not
//!   an event; the transition ring is bounded.
//! * **Monotonic version, logical time.** Time is the caller's monotonic or
//!   logical clock; the version increases on every change of the snapshot, so a
//!   recorded decision names exactly the value it used.
//!
//! This module owns no scheduler, output, tool, avatar, OBS, memory or security
//! access and mints no capability or permit.

use crate::{HigherRankedScene, PerceptionTracker, RuntimeError, SceneFieldSource};
use aivtuber_domain::{
    Interruptibility, MAX_SCENE_LABELS, ObsSceneMap, ObsSceneMapEntry,
    SCENE_CONTEXT_SCHEMA_VERSION, SceneClass, SceneContextSnapshot, SceneSourceRank,
    SceneTransition, is_scene_label,
};
use serde::Serialize;
use std::collections::VecDeque;

/// Longest OBS scene name the tracker looks up, in bytes. A longer name cannot
/// be a table key.
const MAX_LOOKUP_NAME_BYTES: usize = aivtuber_domain::MAX_OBS_SCENE_NAME_BYTES;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SceneContextConfig {
    /// The operator's OBS scene table. Empty means OBS state says nothing.
    pub obs_map: ObsSceneMap,
    /// An OBS reading older than this is silent. The composition root polls
    /// faster than this, so only a dead OBS session ages out.
    pub obs_max_age_ms: u64,
    /// Most transitions kept until taken.
    pub transition_ring: usize,
}

impl Default for SceneContextConfig {
    fn default() -> Self {
        Self {
            obs_map: ObsSceneMap {
                schema_version: aivtuber_domain::OBS_SCENE_MAP_SCHEMA_VERSION.to_owned(),
                scenes: Default::default(),
            },
            obs_max_age_ms: 15_000,
            transition_ring: 16,
        }
    }
}

impl SceneContextConfig {
    pub fn validate(&self) -> Result<(), RuntimeError> {
        let bad = RuntimeError::InvalidConfiguration;
        self.obs_map
            .validate()
            .map_err(|_| bad("scene context OBS scene map is invalid"))?;
        if !(1..=300_000).contains(&self.obs_max_age_ms) {
            return Err(bad("scene context obs_max_age_ms must be in 1..=300000"));
        }
        if !(1..=64).contains(&self.transition_ring) {
            return Err(bad("scene context transition_ring must be in 1..=64"));
        }
        Ok(())
    }
}

/// Counters only; no scene name, label text or caption.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct SceneContextMetrics {
    pub obs_readings: u64,
    /// OBS reported a scene the operator did not map (silence, not a guess).
    pub obs_unmapped: u64,
    pub obs_cleared: u64,
    pub snapshots_changed: u64,
    pub class_transitions: u64,
    pub transitions_dropped: u64,
    /// Perceived labels outside the closed vocabulary.
    pub labels_dropped: u64,
    /// A candidate snapshot failed its own validation (a bug, never expected).
    pub invalid_candidates: u64,
}

#[derive(Debug, Clone)]
struct ObsReading {
    /// `None`: OBS answered but the operator did not map the scene.
    entry: Option<ObsSceneMapEntry>,
    observed_at_ms: u64,
}

#[derive(Debug)]
pub struct SceneContextTracker {
    config: SceneContextConfig,
    obs: Option<ObsReading>,
    current: Option<SceneContextSnapshot>,
    /// Until when the current snapshot's trusted part may be used for
    /// scheduling; read-only checks cannot refresh it themselves.
    trusted_until_ms: u64,
    version: u64,
    last_now_ms: u64,
    transitions: VecDeque<SceneTransition>,
    metrics: SceneContextMetrics,
}

impl SceneContextTracker {
    pub fn new(config: SceneContextConfig) -> Result<Self, RuntimeError> {
        config.validate()?;
        Ok(Self {
            transitions: VecDeque::with_capacity(config.transition_ring),
            config,
            obs: None,
            current: None,
            trusted_until_ms: 0,
            version: 0,
            last_now_ms: 0,
            metrics: SceneContextMetrics::default(),
        })
    }

    pub fn config(&self) -> &SceneContextConfig {
        &self.config
    }

    pub fn metrics(&self) -> SceneContextMetrics {
        self.metrics
    }

    /// Whether the operator mapped any OBS scene. Without a table, OBS state is
    /// silent by construction.
    pub fn has_obs_map(&self) -> bool {
        !self.config.obs_map.scenes.is_empty()
    }

    /// Record the OBS program scene the composition root read from OBS. The
    /// name is looked up in the operator's table and dropped.
    pub fn observe_obs_program_scene(&mut self, scene_name: &str, now_ms: u64) {
        let now_ms = self.advance(now_ms);
        self.metrics.obs_readings += 1;
        let entry = if scene_name.len() <= MAX_LOOKUP_NAME_BYTES {
            self.config.obs_map.scenes.get(scene_name).cloned()
        } else {
            None
        };
        if entry.is_none() {
            self.metrics.obs_unmapped += 1;
        }
        self.obs = Some(ObsReading {
            entry,
            observed_at_ms: now_ms,
        });
        // The next refresh re-establishes trust; until then scheduling asks nothing.
        self.trusted_until_ms = 0;
    }

    /// OBS could not be read: its state is no longer known.
    pub fn clear_obs_scene(&mut self) {
        self.trusted_until_ms = 0;
        if self.obs.take().is_some() {
            self.metrics.obs_cleared += 1;
        }
    }

    pub fn snapshot(&self) -> Option<&SceneContextSnapshot> {
        self.current.as_ref()
    }

    /// The interruptibility scheduling may use at `now_ms`. Only a trusted
    /// (rank 1 to 3) and still fresh source ever yields anything but
    /// `unknown`: perception cannot reach this answer.
    pub fn scheduling_interruptibility(&self, now_ms: u64) -> Interruptibility {
        match &self.current {
            Some(snapshot)
                if snapshot.source_rank.is_trusted() && now_ms < self.trusted_until_ms =>
            {
                snapshot.interruptibility
            }
            _ => Interruptibility::Unknown,
        }
    }

    /// Remove and return up to `max` class transitions, oldest first.
    pub fn take_transitions(&mut self, max: usize) -> Vec<SceneTransition> {
        let count = max.min(self.transitions.len());
        self.transitions.drain(..count).collect()
    }

    fn advance(&mut self, now_ms: u64) -> u64 {
        self.last_now_ms = self.last_now_ms.max(now_ms);
        self.last_now_ms
    }

    /// Recompute the snapshot at `now_ms` from the fresh OBS reading and, when
    /// composed, the perception tracker for `alias`. Returns the current
    /// snapshot, or `None` while no source has ever spoken.
    pub fn refresh(
        &mut self,
        perception: Option<(&mut PerceptionTracker, &str)>,
        now_ms: u64,
    ) -> Option<&SceneContextSnapshot> {
        let now_ms = self.advance(now_ms);
        let fresh_obs = self.obs.as_ref().filter(|reading| {
            now_ms.saturating_sub(reading.observed_at_ms) <= self.config.obs_max_age_ms
        });
        let obs_entry = fresh_obs.and_then(|reading| reading.entry.as_ref());
        let obs_at_ms = fresh_obs.map(|reading| reading.observed_at_ms);
        let higher = HigherRankedScene {
            scene_class: obs_entry.map(|entry| entry.scene_class),
            labels: obs_entry
                .filter(|entry| !entry.labels.is_empty())
                .map(|entry| entry.labels.clone()),
        };
        let interruptibility_candidate = obs_entry.map(|entry| entry.interruptibility);

        let (class, class_rank, labels, labels_rank, source_ref, confidence, calibrated, seen_ms) =
            match perception {
                Some((tracker, alias)) => {
                    let resolution = tracker.resolve_scene(alias, &higher, now_ms);
                    let perceived = tracker.scene_snapshot(alias, now_ms);
                    let rank_of = |source| match source {
                        SceneFieldSource::HigherRanked => SceneSourceRank::ObsState,
                        SceneFieldSource::Perception => SceneSourceRank::Perception,
                        SceneFieldSource::Unknown => SceneSourceRank::None,
                    };
                    let class_rank = rank_of(resolution.scene_class_source);
                    let labels_rank = rank_of(resolution.labels_source);
                    let uses_perception = class_rank == SceneSourceRank::Perception
                        || labels_rank == SceneSourceRank::Perception;
                    let perceived_at = perceived.as_ref().map(|p| now_ms.saturating_sub(p.age_ms));
                    (
                        resolution.scene_class,
                        class_rank,
                        resolution.labels,
                        labels_rank,
                        uses_perception.then(|| alias.to_owned()),
                        perceived
                            .as_ref()
                            .filter(|_| uses_perception)
                            .and_then(|p| p.confidence),
                        uses_perception && perceived.as_ref().is_some_and(|p| p.calibrated),
                        [obs_at_ms, perceived_at.filter(|_| uses_perception)]
                            .into_iter()
                            .flatten()
                            .max(),
                    )
                }
                None => {
                    let class = higher.scene_class.unwrap_or(SceneClass::Unknown);
                    let class_rank = if higher.scene_class.is_some() {
                        SceneSourceRank::ObsState
                    } else {
                        SceneSourceRank::None
                    };
                    let labels = higher.labels.clone().unwrap_or_default();
                    let labels_rank = if labels.is_empty() {
                        SceneSourceRank::None
                    } else {
                        SceneSourceRank::ObsState
                    };
                    (
                        class,
                        class_rank,
                        labels,
                        labels_rank,
                        None,
                        None,
                        false,
                        obs_at_ms,
                    )
                }
            };

        // The closed vocabulary is final: anything else is dropped, counted.
        let before = labels.len();
        let mut labels: Vec<String> = labels
            .into_iter()
            .filter(|label| is_scene_label(label))
            .take(MAX_SCENE_LABELS)
            .collect();
        self.metrics.labels_dropped += (before - labels.len().min(before)) as u64;
        labels.dedup();
        let labels_rank = if labels.is_empty() {
            SceneSourceRank::None
        } else {
            labels_rank
        };

        // Only the class's own trusted source speaks for interruptibility.
        let interruptibility = if class_rank.is_trusted() {
            interruptibility_candidate.unwrap_or_default()
        } else {
            Interruptibility::Unknown
        };

        if class_rank == SceneSourceRank::None
            && labels_rank == SceneSourceRank::None
            && self.current.is_none()
        {
            return None;
        }

        let candidate = SceneContextSnapshot {
            schema_version: SCENE_CONTEXT_SCHEMA_VERSION.to_owned(),
            version: self.version.max(1),
            scene_class: class,
            labels,
            interruptibility,
            source_rank: class_rank,
            labels_source_rank: labels_rank,
            source_ref,
            confidence,
            calibrated,
            observed_at_ms: seen_ms.unwrap_or(0),
        };
        if candidate.validate().is_err() {
            self.metrics.invalid_candidates += 1;
            return self.current.as_ref();
        }

        let changed = self.current.as_ref().is_none_or(|current| {
            let mut same_version = candidate.clone();
            same_version.version = current.version;
            *current != same_version
        });
        if changed {
            let previous_class = self
                .current
                .as_ref()
                .map_or(SceneClass::Unknown, |current| current.scene_class);
            self.version += 1;
            let mut next = candidate;
            next.version = self.version;
            self.metrics.snapshots_changed += 1;
            if next.scene_class != previous_class {
                self.metrics.class_transitions += 1;
                if self.transitions.len() >= self.config.transition_ring {
                    self.transitions.pop_front();
                    self.metrics.transitions_dropped += 1;
                }
                self.transitions.push_back(SceneTransition {
                    from: previous_class,
                    to: next.scene_class,
                    version: next.version,
                    at_ms: now_ms,
                    source_rank: next.source_rank,
                });
            }
            self.current = Some(next);
        }
        self.trusted_until_ms = if class_rank.is_trusted() {
            obs_at_ms.map_or(0, |at| at.saturating_add(self.config.obs_max_age_ms))
        } else {
            0
        };
        self.current.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PerceptionConfig;
    use aivtuber_domain::{
        AuthenticatedControl, AuthorizationMethod, Capability, ControlSecret, LocalControlIngress,
        OperatorCommandInput, PERCEPTION_OBSERVATION_SCHEMA_VERSION, PerceptionFrame,
        PerceptionKind, PerceptionObservation, PerceptionObservationBatch, PerceptionPayload,
        PerceptionProducer, PerceptionRedaction, PerceptionSceneClass, PerceptionScenePayload,
        PerceptionSource, PrivacyClass,
    };
    use aivtuber_telemetry::SecretRedactor;
    use std::collections::{BTreeMap, BTreeSet};

    fn operator() -> AuthenticatedControl {
        let secret = [0x31_u8; 32];
        LocalControlIngress::new(
            "local-test",
            "operator:test",
            AuthorizationMethod::OperatorHotkey,
            BTreeSet::from([Capability::PerceptionControl]),
            ControlSecret::new(secret),
        )
        .expect("ingress")
        .authenticate(
            OperatorCommandInput {
                event_id: "evt-scene".to_owned(),
                correlation_id: "corr-scene".to_owned(),
                sequence: 1,
                observed_at: "2026-11-10T00:00:00Z".to_owned(),
                action: "perception.control".to_owned(),
                payload: BTreeMap::new(),
            },
            &secret,
        )
        .expect("authenticated")
        .authority()
        .clone()
    }

    fn map() -> ObsSceneMap {
        ObsSceneMap::from_json_bytes(
            br#"{"schema_version":"0.1.0","scenes":{
                "Starting":{"scene_class":"intermission","interruptibility":"interruptible"},
                "Boss":{"scene_class":"gameplay","interruptibility":"uninterruptible","labels":["boss_fight"]},
                "Chat":{"scene_class":"just_chatting","interruptibility":"interruptible"}
            }}"#,
        )
        .expect("map")
    }

    fn tracker_with_map() -> SceneContextTracker {
        SceneContextTracker::new(SceneContextConfig {
            obs_map: map(),
            ..SceneContextConfig::default()
        })
        .expect("tracker")
    }

    fn perception() -> PerceptionTracker {
        let mut tracker = PerceptionTracker::new(
            PerceptionConfig {
                source_privacy_floors: BTreeMap::from([(
                    "game-main".to_owned(),
                    PrivacyClass::Private,
                )]),
                ..PerceptionConfig::default()
            },
            SecretRedactor::new(["sk-live-secret"]),
        )
        .expect("perception");
        tracker
            .enable_source(&operator(), "game-main", 0)
            .expect("enable");
        tracker
    }

    fn feed(
        tracker: &mut PerceptionTracker,
        seq: u64,
        at: u64,
        class: PerceptionSceneClass,
        labels: &[&str],
    ) {
        let batch = PerceptionObservationBatch {
            schema_version: PERCEPTION_OBSERVATION_SCHEMA_VERSION.to_owned(),
            provider_id: "vision".to_owned(),
            provider_version: "1".to_owned(),
            observations: vec![PerceptionObservation {
                kind: PerceptionKind::ScreenScene,
                observation_id: format!("obs-{seq}"),
                source: PerceptionSource {
                    alias: "game-main".to_owned(),
                },
                frame: PerceptionFrame {
                    capture_seq: seq,
                    captured_at_ms: at,
                    width: 768,
                    height: 432,
                },
                observed_at_ms: at,
                expires_at_ms: at + 20_000,
                confidence: Some(0.7),
                calibrated: false,
                producer: PerceptionProducer {
                    model_id: "test".to_owned(),
                    config_digest: "0".repeat(64),
                    prompt_version: "p1".to_owned(),
                },
                privacy: PrivacyClass::Public,
                redaction: PerceptionRedaction {
                    policy_version: "r1".to_owned(),
                    secret_hits: 0,
                    truncated: false,
                },
                payload: PerceptionPayload::Scene(PerceptionScenePayload {
                    scene_class: class,
                    labels: labels.iter().map(|l| (*l).to_owned()).collect(),
                }),
            }],
        };
        tracker.ingest_batch(&batch, at).expect("ingest");
    }

    #[test]
    fn nothing_known_yields_no_snapshot() {
        let mut scene = tracker_with_map();
        let mut perceived = perception();
        assert!(
            scene
                .refresh(Some((&mut perceived, "game-main")), 10)
                .is_none()
        );
        assert!(scene.refresh(None, 20).is_none());
        assert_eq!(scene.metrics().snapshots_changed, 0);
    }

    #[test]
    fn obs_scene_maps_through_the_operator_table_and_is_trusted() {
        let mut scene = tracker_with_map();
        scene.observe_obs_program_scene("Boss", 100);
        let snapshot = scene.refresh(None, 100).cloned().expect("snapshot");
        assert_eq!(snapshot.scene_class, SceneClass::Gameplay);
        assert_eq!(snapshot.source_rank, SceneSourceRank::ObsState);
        assert_eq!(snapshot.labels, ["boss_fight"]);
        assert_eq!(snapshot.interruptibility, Interruptibility::Uninterruptible);
        assert_eq!(snapshot.version, 1);
        assert!(snapshot.validate().is_ok());
        assert_eq!(
            scene.scheduling_interruptibility(100),
            Interruptibility::Uninterruptible
        );
        // The trusted reading ages out for scheduling without a refresh.
        assert_eq!(
            scene.scheduling_interruptibility(100 + 15_000),
            Interruptibility::Unknown
        );
    }

    #[test]
    fn an_unmapped_obs_scene_is_silence_not_a_guess() {
        let mut scene = tracker_with_map();
        scene.observe_obs_program_scene("Some Scene The Operator Never Mapped", 100);
        assert!(scene.refresh(None, 100).is_none());
        assert_eq!(scene.metrics().obs_unmapped, 1);
        // A name too long to be a key is also just unmapped.
        scene.observe_obs_program_scene(&"x".repeat(4_096), 110);
        assert!(scene.refresh(None, 110).is_none());
    }

    #[test]
    fn higher_rank_always_wins_and_perception_fills_gaps() {
        let mut scene = tracker_with_map();
        let mut perceived = perception();
        feed(
            &mut perceived,
            1,
            100,
            PerceptionSceneClass::Menu,
            &["settings"],
        );

        // Perception alone: rank 4, no interruptibility.
        let alone = scene
            .refresh(Some((&mut perceived, "game-main")), 110)
            .cloned()
            .expect("snapshot");
        assert_eq!(alone.scene_class, SceneClass::Menu);
        assert_eq!(alone.source_rank, SceneSourceRank::Perception);
        assert_eq!(alone.labels_source_rank, SceneSourceRank::Perception);
        assert_eq!(alone.interruptibility, Interruptibility::Unknown);
        assert_eq!(alone.source_ref.as_deref(), Some("game-main"));
        assert_eq!(alone.confidence, Some(0.7));

        // OBS disagrees: OBS wins the class; its labels win the labels.
        scene.observe_obs_program_scene("Boss", 120);
        let both = scene
            .refresh(Some((&mut perceived, "game-main")), 120)
            .cloned()
            .expect("snapshot");
        assert_eq!(both.scene_class, SceneClass::Gameplay);
        assert_eq!(both.source_rank, SceneSourceRank::ObsState);
        assert_eq!(both.labels, ["boss_fight"]);
        assert_eq!(both.labels_source_rank, SceneSourceRank::ObsState);
        assert!(both.version > alone.version);
        assert_eq!(both.source_ref, None);

        // OBS mapped a scene with no labels: perception supplies the labels
        // only, the class stays trusted.
        scene.observe_obs_program_scene("Chat", 130);
        let mixed = scene
            .refresh(Some((&mut perceived, "game-main")), 130)
            .cloned()
            .expect("snapshot");
        assert_eq!(mixed.scene_class, SceneClass::JustChatting);
        assert_eq!(mixed.source_rank, SceneSourceRank::ObsState);
        assert_eq!(mixed.labels, ["settings"]);
        assert_eq!(mixed.labels_source_rank, SceneSourceRank::Perception);
        assert_eq!(mixed.interruptibility, Interruptibility::Interruptible);
        assert!(mixed.validate().is_ok());
    }

    /// The authority rule: perception may inform context and may never touch
    /// scheduling inputs.
    #[test]
    fn perception_never_reaches_scheduling_or_interruptibility() {
        let mut scene = tracker_with_map();
        let mut perceived = perception();
        // Every perceivable class, with and without labels.
        for (seq, class) in [
            PerceptionSceneClass::Gameplay,
            PerceptionSceneClass::Menu,
            PerceptionSceneClass::Loading,
            PerceptionSceneClass::Cutscene,
            PerceptionSceneClass::Result,
            PerceptionSceneClass::Desktop,
        ]
        .into_iter()
        .enumerate()
        {
            let at = 100 + seq as u64 * 1_000;
            feed(
                &mut perceived,
                seq as u64 + 1,
                at,
                class,
                &["boss_fight", "victory"],
            );
            let snapshot = scene
                .refresh(Some((&mut perceived, "game-main")), at)
                .cloned()
                .expect("snapshot");
            assert_eq!(snapshot.source_rank, SceneSourceRank::Perception);
            assert_eq!(snapshot.interruptibility, Interruptibility::Unknown);
            assert_eq!(
                scene.scheduling_interruptibility(at),
                Interruptibility::Unknown
            );
        }
        // The class that "looks" most interrupt-worthy changes nothing either.
        assert_eq!(
            scene.scheduling_interruptibility(5_000),
            Interruptibility::Unknown
        );
    }

    #[test]
    fn perceived_scene_falls_back_when_obs_goes_silent_or_stale() {
        let mut scene = tracker_with_map();
        let mut perceived = perception();
        feed(&mut perceived, 1, 100, PerceptionSceneClass::Gameplay, &[]);
        scene.observe_obs_program_scene("Starting", 100);
        let first = scene
            .refresh(Some((&mut perceived, "game-main")), 100)
            .cloned()
            .expect("snapshot");
        assert_eq!(first.scene_class, SceneClass::Intermission);
        assert_eq!(first.interruptibility, Interruptibility::Interruptible);

        // OBS stops answering: it ages out and perception speaks, with no
        // interruptibility.
        let later = scene
            .refresh(Some((&mut perceived, "game-main")), 100 + 15_001)
            .cloned();
        // Perception observed at 100 expired at 20_100; at 15_101 it is fresh.
        let later = later.expect("snapshot");
        assert_eq!(later.scene_class, SceneClass::Gameplay);
        assert_eq!(later.interruptibility, Interruptibility::Unknown);

        // Everything expired: back to unknown, still a versioned snapshot.
        let none = scene
            .refresh(Some((&mut perceived, "game-main")), 100 + 40_000)
            .cloned()
            .expect("snapshot");
        assert_eq!(none.scene_class, SceneClass::Unknown);
        assert_eq!(none.source_rank, SceneSourceRank::None);
        assert!(none.validate().is_ok());
    }

    #[test]
    fn transitions_fire_on_class_change_only() {
        let mut scene = tracker_with_map();
        let mut perceived = perception();
        scene.observe_obs_program_scene("Chat", 100);
        scene.refresh(Some((&mut perceived, "game-main")), 100);
        // A label-only change, a refreshed observation and a confidence change
        // bump the version but are not transitions.
        feed(
            &mut perceived,
            1,
            200,
            PerceptionSceneClass::Menu,
            &["settings"],
        );
        scene.refresh(Some((&mut perceived, "game-main")), 200);
        feed(&mut perceived, 2, 300, PerceptionSceneClass::Menu, &["map"]);
        scene.refresh(Some((&mut perceived, "game-main")), 300);
        scene.observe_obs_program_scene("Boss", 400);
        scene.refresh(Some((&mut perceived, "game-main")), 400);
        // Repeated refresh with nothing new changes nothing.
        let version = scene.snapshot().map(|s| s.version);
        scene.refresh(Some((&mut perceived, "game-main")), 400);
        scene.refresh(Some((&mut perceived, "game-main")), 401);
        assert_eq!(scene.snapshot().map(|s| s.version), version);

        let transitions = scene.take_transitions(10);
        let classes: Vec<_> = transitions.iter().map(|t| (t.from, t.to)).collect();
        assert_eq!(
            classes,
            [
                (SceneClass::Unknown, SceneClass::JustChatting),
                (SceneClass::JustChatting, SceneClass::Gameplay),
            ]
        );
        assert!(transitions.windows(2).all(|w| w[0].version < w[1].version));
        assert!(scene.take_transitions(10).is_empty());
        assert!(scene.metrics().snapshots_changed > scene.metrics().class_transitions);
    }

    #[test]
    fn version_is_monotonic_and_the_transition_ring_is_bounded() {
        let mut scene = SceneContextTracker::new(SceneContextConfig {
            obs_map: map(),
            transition_ring: 2,
            ..SceneContextConfig::default()
        })
        .expect("tracker");
        let mut last = 0;
        for (i, name) in ["Boss", "Chat", "Boss", "Starting", "Chat"]
            .iter()
            .enumerate()
        {
            scene.observe_obs_program_scene(name, i as u64 * 10);
            let version = scene
                .refresh(None, i as u64 * 10)
                .expect("snapshot")
                .version;
            assert!(version > last);
            last = version;
        }
        assert_eq!(scene.take_transitions(10).len(), 2);
        assert_eq!(scene.metrics().transitions_dropped, 3);
    }

    #[test]
    fn labels_outside_the_closed_vocabulary_are_dropped() {
        // An operator profile may widen the perception tracker's vocabulary;
        // the snapshot's closed vocabulary is final.
        let mut perceived = PerceptionTracker::new(
            PerceptionConfig {
                source_privacy_floors: BTreeMap::from([(
                    "game-main".to_owned(),
                    PrivacyClass::Private,
                )]),
                label_vocabulary: BTreeSet::from(["combat".to_owned(), "fishing".to_owned()]),
                ..PerceptionConfig::default()
            },
            SecretRedactor::new(["sk-live-secret"]),
        )
        .expect("perception");
        perceived
            .enable_source(&operator(), "game-main", 0)
            .expect("enable");
        feed(
            &mut perceived,
            1,
            100,
            PerceptionSceneClass::Gameplay,
            &["combat", "fishing"],
        );
        let mut scene = tracker_with_map();
        let snapshot = scene
            .refresh(Some((&mut perceived, "game-main")), 100)
            .cloned()
            .expect("snapshot");
        assert_eq!(snapshot.labels, ["combat"]);
        assert_eq!(scene.metrics().labels_dropped, 1);
    }

    #[test]
    fn the_scene_module_has_no_content_plane_or_authority_dependencies() {
        let source = include_str!("scene_context.rs");
        let production = source.split("#[cfg(test)]").next().unwrap();
        let code: String = production
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        for forbidden in [
            "EventEnvelope",
            "SourceClass",
            "ThinkingContent",
            "Scheduler",
            "Authorized",
            "MemoryWritePermit",
            "StreamAction",
            "ToolAction",
            "AvatarAction",
            "authorize_",
            "SecurityRuntime",
            "AuthenticatedControl",
        ] {
            assert!(
                !code.contains(forbidden),
                "{forbidden} appears in scene context code"
            );
        }
    }
}
