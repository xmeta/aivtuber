//! Composition of the scene context (issue #88 S1, ADR-0007 D-3).
//!
//! [`SceneContextService`] joins the runtime's [`SceneContextTracker`] (the
//! ranked, bounded snapshot) with a reader of the OBS program scene. The OBS
//! read is slow I/O, so it runs on its own small thread that only ever
//! overwrites one shared slot; the main loop reads the slot without waiting.
//! A dead or slow OBS therefore costs the loop nothing: the reading ages out
//! and the scene falls back to whatever lower-ranked source is fresh.
//!
//! Boundaries kept here:
//! * **Operator-owned mapping only.** The scene name read from OBS is only a
//!   key into the operator's table (`ObsSceneMap`); an unmapped name is silence.
//! * **No content-plane input.** Nothing here reads an event, a chat message or
//!   model output.
//! * **Perception is only an input to context.** The service passes the
//!   perception tracker to the scene tracker, which keeps the rank rules; the
//!   snapshot it produces can reach scheduling only through its trusted
//!   interruptibility ([`SceneContextTracker::scheduling_interruptibility`]).
//! * **Off by default.** Nothing is composed unless the operator supplies an
//!   OBS scene table (or perception is composed); without a service the
//!   runtime serializes exactly as it did before scene context existed.

use aivtuber_adapters::ObsWebSocketAdapter;
use aivtuber_domain::{EngineError, Interruptibility, SceneContextSnapshot, SceneTransition};
use aivtuber_runtime::{
    PerceptionTracker, RuntimeError, SceneContextConfig, SceneContextMetrics, SceneContextTracker,
};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

/// The main loop recomputes the snapshot at most this often; a request also
/// refreshes it at the request's own logical time.
pub const SCENE_REFRESH_INTERVAL_MS: u64 = 500;

/// Reads the OBS program scene name. The production implementation is
/// [`ObsProgramSceneSource`]; tests supply scripted ones.
pub trait ProgramSceneSource: Send {
    fn current_program_scene(&mut self) -> Result<String, EngineError>;
}

/// OBS websocket reader (obs-websocket `GetCurrentProgramScene`, read-only).
pub struct ObsProgramSceneSource {
    adapter: ObsWebSocketAdapter,
}

impl ObsProgramSceneSource {
    pub fn new(adapter: ObsWebSocketAdapter) -> Self {
        Self { adapter }
    }
}

impl ProgramSceneSource for ObsProgramSceneSource {
    fn current_program_scene(&mut self) -> Result<String, EngineError> {
        self.adapter.current_program_scene()
    }
}

/// The latest poll outcome. `seq` increases on every poll so the main loop can
/// tell a fresh reading from an old one even when the name is unchanged.
#[derive(Debug, Default)]
struct PolledScene {
    seq: u64,
    scene: Option<String>,
}

struct ScenePoller {
    shared: Arc<Mutex<PolledScene>>,
    /// Dropping the sender wakes and ends the poll thread.
    _stop: Sender<()>,
}

impl ScenePoller {
    fn start(mut source: Box<dyn ProgramSceneSource>, interval: Duration) -> Self {
        let shared = Arc::new(Mutex::new(PolledScene::default()));
        let (stop, stopped) = mpsc::channel::<()>();
        let slot = shared.clone();
        // Detached on purpose: a thread blocked in an OBS request ends when
        // the request returns and the stop channel is found closed.
        let _ = thread::Builder::new()
            .name("scene-poller".to_owned())
            .spawn(move || {
                loop {
                    let scene = source.current_program_scene().ok();
                    if let Ok(mut polled) = slot.lock() {
                        polled.seq += 1;
                        polled.scene = scene;
                    }
                    match stopped.recv_timeout(interval) {
                        Err(RecvTimeoutError::Timeout) => {}
                        Ok(()) | Err(RecvTimeoutError::Disconnected) => break,
                    }
                }
            });
        Self {
            shared,
            _stop: stop,
        }
    }

    fn latest(&self) -> (u64, Option<String>) {
        self.shared
            .lock()
            .map(|polled| (polled.seq, polled.scene.clone()))
            .unwrap_or((0, None))
    }
}

pub struct SceneContextService {
    tracker: SceneContextTracker,
    poller: Option<ScenePoller>,
    last_seq: u64,
    last_refresh_ms: Option<u64>,
    captions_dropped_for_budget: u64,
}

impl std::fmt::Debug for SceneContextService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SceneContextService")
            .field("polling", &self.poller.is_some())
            .field("metrics", &self.tracker.metrics())
            .finish_non_exhaustive()
    }
}

impl SceneContextService {
    /// Compose with no OBS reader (perception and tests feed the tracker).
    pub fn new(config: SceneContextConfig) -> Result<Self, RuntimeError> {
        Ok(Self {
            tracker: SceneContextTracker::new(config)?,
            poller: None,
            last_seq: 0,
            last_refresh_ms: None,
            captions_dropped_for_budget: 0,
        })
    }

    /// Compose with an OBS program scene reader polled every `interval`.
    pub fn with_obs_source(
        config: SceneContextConfig,
        source: Box<dyn ProgramSceneSource>,
        interval: Duration,
    ) -> Result<Self, RuntimeError> {
        let mut service = Self::new(config)?;
        service.poller = Some(ScenePoller::start(source, interval));
        Ok(service)
    }

    /// A screen caption that did not fit the Thinking request budget and was
    /// dropped whole (memory keeps its room).
    pub fn captions_dropped_for_budget(&self) -> u64 {
        self.captions_dropped_for_budget
    }

    pub(crate) fn note_caption_dropped(&mut self) {
        self.captions_dropped_for_budget += 1;
    }

    pub fn tracker(&self) -> &SceneContextTracker {
        &self.tracker
    }

    pub fn tracker_mut(&mut self) -> &mut SceneContextTracker {
        &mut self.tracker
    }

    pub fn metrics(&self) -> SceneContextMetrics {
        self.tracker.metrics()
    }

    pub fn snapshot(&self) -> Option<&SceneContextSnapshot> {
        self.tracker.snapshot()
    }

    pub fn scheduling_interruptibility(&self, now_ms: u64) -> Interruptibility {
        self.tracker.scheduling_interruptibility(now_ms)
    }

    pub fn scheduling_context(&self, now_ms: u64) -> aivtuber_domain::TrustedInterruptibility {
        self.tracker.scheduling_context(now_ms)
    }

    pub fn take_transitions(&mut self, max: usize) -> Vec<SceneTransition> {
        self.tracker.take_transitions(max)
    }

    /// Fold the latest OBS reading into the tracker. Never blocks.
    fn absorb_obs(&mut self, now_ms: u64) {
        let Some(poller) = &self.poller else {
            return;
        };
        let (seq, scene) = poller.latest();
        if seq == self.last_seq {
            return;
        }
        self.last_seq = seq;
        match scene {
            Some(name) => self.tracker.observe_obs_program_scene(&name, now_ms),
            None => self.tracker.clear_obs_scene(),
        }
    }

    /// Main-loop tick: absorb the OBS reading and recompute the snapshot at
    /// most every [`SCENE_REFRESH_INTERVAL_MS`].
    pub fn service(&mut self, now_ms: u64, perception: Option<(&mut PerceptionTracker, &str)>) {
        self.absorb_obs(now_ms);
        if self
            .last_refresh_ms
            .is_some_and(|last| now_ms < last.saturating_add(SCENE_REFRESH_INTERVAL_MS))
        {
            return;
        }
        self.last_refresh_ms = Some(now_ms);
        self.tracker.refresh(perception, now_ms);
    }

    /// The snapshot a request at `now_ms` should carry, recomputed now so it is
    /// never staler than the request.
    pub fn snapshot_for_request(
        &mut self,
        now_ms: u64,
        perception: Option<(&mut PerceptionTracker, &str)>,
    ) -> Option<SceneContextSnapshot> {
        self.absorb_obs(now_ms);
        self.last_refresh_ms = Some(now_ms);
        self.tracker.refresh(perception, now_ms).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aivtuber_domain::{EngineErrorKind, ObsSceneMap, SceneClass};
    use std::collections::VecDeque;
    use std::time::Instant;

    struct Scripted {
        replies: VecDeque<Result<String, EngineError>>,
        delay: Duration,
    }

    impl ProgramSceneSource for Scripted {
        fn current_program_scene(&mut self) -> Result<String, EngineError> {
            thread::sleep(self.delay);
            self.replies
                .pop_front()
                .unwrap_or_else(|| Err(EngineError::new(EngineErrorKind::Unavailable, "no more")))
        }
    }

    fn config() -> SceneContextConfig {
        SceneContextConfig {
            obs_map: ObsSceneMap::from_json_bytes(
                br#"{"schema_version":"0.1.0","scenes":{"Boss":{"scene_class":"gameplay"}}}"#,
            )
            .expect("map"),
            ..SceneContextConfig::default()
        }
    }

    fn wait_until(mut done: impl FnMut() -> bool) -> bool {
        let started = Instant::now();
        while started.elapsed() < Duration::from_secs(5) {
            if done() {
                return true;
            }
            thread::sleep(Duration::from_millis(5));
        }
        false
    }

    #[test]
    fn the_poller_feeds_the_tracker_and_a_failed_read_clears_obs_state() {
        let source = Scripted {
            replies: VecDeque::from([Ok("Boss".to_owned())]),
            delay: Duration::ZERO,
        };
        let mut service = SceneContextService::with_obs_source(
            config(),
            Box::new(source),
            Duration::from_millis(20),
        )
        .expect("service");
        let mut now = 0;
        assert!(wait_until(|| {
            now += 1_000;
            service.service(now, None);
            service.snapshot().is_some()
        }));
        assert_eq!(
            service.snapshot().map(|s| s.scene_class),
            Some(SceneClass::Gameplay)
        );
        // The script is exhausted, so the next polls fail: OBS state is cleared
        // and, with no other source, the scene falls back to unknown.
        assert!(wait_until(|| {
            now += 1_000;
            service.service(now, None);
            service.snapshot().map(|s| s.scene_class) == Some(SceneClass::Unknown)
        }));
        assert!(service.metrics().obs_cleared >= 1);
    }

    #[test]
    fn a_slow_obs_never_blocks_the_loop() {
        let source = Scripted {
            replies: VecDeque::from([Ok("Boss".to_owned())]),
            delay: Duration::from_millis(600),
        };
        let mut service = SceneContextService::with_obs_source(
            config(),
            Box::new(source),
            Duration::from_millis(20),
        )
        .expect("service");
        let started = Instant::now();
        for step in 0..50 {
            service.service(step * 600, None);
        }
        assert!(
            started.elapsed() < Duration::from_millis(300),
            "ticks must not wait on OBS"
        );
        assert!(service.snapshot().is_none(), "nothing read yet");
    }

    #[test]
    fn a_service_without_a_source_stays_silent() {
        let mut service = SceneContextService::new(config()).expect("service");
        service.service(1_000, None);
        assert!(service.snapshot().is_none());
        assert!(service.snapshot_for_request(2_000, None).is_none());
    }
}
