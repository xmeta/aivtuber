//! Gapless native playback (#83 S3d, docs/audio-backend-spike.adoc).
//!
//! One persistent `rodio` [`Player`] per reply: a continuation phrase is
//! `append`ed behind the audio in flight, so the mixer starts it on the sample
//! after the previous one ends instead of whenever the app got around to a
//! new `play`. Everything that matters is device-free and lives in
//! [`GaplessQueue`], which runs on any `rodio` [`Mixer`] (including an offline
//! one); [`RodioAudioPlayer`] only adds the default output device.
//!
//! Invariants:
//! * The queue is bounded ([`MAX_QUEUED_PHRASES`]); a full queue refuses the
//!   request with [`EngineErrorKind::Overloaded`] and counts it.
//! * Speed is applied to each source, never to the player, so a continuation
//!   with another speed cannot retune the phrase already playing.
//! * A stop never calls `Player::clear` (it blocks until the queue drains) and
//!   never appends to a stopped player (`append` blocks then too): the player
//!   is taken out, faded and dropped, and the next `play` builds a new one.
//! * Phrases queued behind the audible one are dropped at the instant of the
//!   stop, not at the end of the fade.

use crate::audio::{
    AudioPlayRequest, AudioPlayer, AudioStopOutcome, MAX_AUDIO_STOP_FADE_MS, engine_error,
    resolve_audio_reference, validate_audio_path,
};
use aivtuber_domain::{EngineError, EngineErrorKind};
use rodio::mixer::Mixer;
use rodio::{ChannelCount, Decoder, Player, SampleRate, Source};
#[cfg(feature = "native-audio-spike")]
use rodio::{DeviceSinkBuilder, MixerDeviceSink};
use std::fs::File;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::Duration;

/// Most phrases the persistent player may hold (playing plus queued). The app
/// releases a continuation about one lookahead early, so two is the steady
/// state; the third is slack for a short phrase.
pub const MAX_QUEUED_PHRASES: usize = 3;

/// リコール済みだがオーディオミキサーがまだ取り出していないソースを含めた
/// 生の `rodio::Player` キュー上限（有界性維持のため）。
const MAX_RAW_APPENDED: usize = MAX_QUEUED_PHRASES * 2;

const ITEM_QUEUED: u8 = 0;
const ITEM_STARTED: u8 = 1;
const ITEM_CANCELLED: u8 = 2;
const ITEM_DRAINED: u8 = 3;

/// Passes samples through until it is asked to drop unstarted audio. Once its
/// first sample has been produced it always plays on (the phrase being faded
/// out belongs to the fade, not to the drop).
struct DropUnstarted<S> {
    inner: S,
    drop_unstarted: Arc<AtomicBool>,
    state: Arc<AtomicU8>,
    local_started: bool,
}

impl<S: Source> Iterator for DropUnstarted<S> {
    type Item = S::Item;

    fn next(&mut self) -> Option<Self::Item> {
        if !self.local_started {
            if self.drop_unstarted.load(Ordering::Acquire) {
                self.state.store(ITEM_DRAINED, Ordering::Release);
                return None;
            }
            if self
                .state
                .compare_exchange(
                    ITEM_QUEUED,
                    ITEM_STARTED,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_err()
            {
                // リコール済みのため1サンプルも出力せず終了する
                self.state.store(ITEM_DRAINED, Ordering::Release);
                return None;
            }
            self.local_started = true;
        }
        let sample = self.inner.next();
        if sample.is_none() {
            self.state.store(ITEM_DRAINED, Ordering::Release);
        }
        sample
    }
}

impl<S: Source> Source for DropUnstarted<S> {
    fn current_span_len(&self) -> Option<usize> {
        self.inner.current_span_len()
    }

    fn channels(&self) -> ChannelCount {
        self.inner.channels()
    }

    fn sample_rate(&self) -> SampleRate {
        self.inner.sample_rate()
    }

    fn total_duration(&self) -> Option<Duration> {
        self.inner.total_duration()
    }
}

struct Slot {
    player: Player,
    drop_unstarted: Arc<AtomicBool>,
    items: Vec<(Option<u64>, Arc<AtomicU8>)>,
}

impl Slot {
    fn new(mixer: &Mixer) -> Self {
        Self {
            player: Player::connect_new(mixer),
            drop_unstarted: Arc::new(AtomicBool::new(false)),
            items: Vec::new(),
        }
    }

    fn prune(&mut self) {
        self.items
            .retain(|(_, state)| state.load(Ordering::Acquire) != ITEM_DRAINED);
    }

    fn live_count(&self) -> usize {
        self.items
            .iter()
            .filter(|(_, state)| {
                matches!(state.load(Ordering::Acquire), ITEM_QUEUED | ITEM_STARTED)
            })
            .count()
    }

    /// End everything at once: queued phrases never start, the audible one is
    /// stopped. Never blocks.
    fn hard_stop(&self) {
        self.drop_unstarted.store(true, Ordering::Release);
        self.player.stop();
    }
}

/// The device-free core of the native backend.
pub struct GaplessQueue {
    root: PathBuf,
    mixer: Mixer,
    slot: Mutex<Option<Slot>>,
    refused_queue_full: AtomicU64,
}

impl std::fmt::Debug for GaplessQueue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GaplessQueue")
            .field("root", &self.root)
            .finish_non_exhaustive()
    }
}

impl GaplessQueue {
    pub fn new(root: impl Into<PathBuf>, mixer: Mixer) -> Result<Self, EngineError> {
        let root = root.into();
        if root.as_os_str().is_empty() {
            return Err(engine_error(
                EngineErrorKind::InvalidRequest,
                "audio root must not be empty",
            ));
        }
        Ok(Self {
            root,
            mixer,
            slot: Mutex::new(None),
            refused_queue_full: AtomicU64::new(0),
        })
    }

    pub fn resolve_reference(&self, audio_ref: &str) -> Result<PathBuf, EngineError> {
        resolve_audio_reference(&self.root, audio_ref)
    }

    /// Requests refused because the queue already held
    /// [`MAX_QUEUED_PHRASES`] phrases.
    pub fn refused_queue_full(&self) -> u64 {
        self.refused_queue_full.load(Ordering::Relaxed)
    }

    /// Phrases currently playing or queued.
    pub fn queued(&self) -> usize {
        lock(&self.slot)
            .as_ref()
            .filter(|slot| !slot.player.empty())
            .map_or(0, Slot::live_count)
    }

    pub fn is_idle(&self) -> bool {
        self.queued() == 0
    }

    /// Hard stop (no fade): used by benchmarks and a zero-length fade.
    pub fn stop_now(&self) {
        if let Some(slot) = lock(&self.slot).take() {
            slot.hard_stop();
        }
    }

    /// 最初のサンプルがまだ再生されていないキュー内アイテムを取り消す (#354)。
    pub fn cancel_queued(&self, id: u64) -> bool {
        let mut slot_guard = lock(&self.slot);
        let Some(slot) = slot_guard.as_mut() else {
            return false;
        };
        slot.prune();
        let mut cancelled = false;
        for (item_id, state) in &slot.items {
            if *item_id == Some(id)
                && state
                    .compare_exchange(
                        ITEM_QUEUED,
                        ITEM_CANCELLED,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    )
                    .is_ok()
            {
                cancelled = true;
            }
        }
        cancelled
    }

    /// Queue one already-decoded source. `play` is the file-backed entry.
    pub fn enqueue<S>(
        &self,
        source: S,
        speed_factor: f64,
        continuation: bool,
    ) -> Result<(), EngineError>
    where
        S: Source + Send + 'static,
    {
        self.enqueue_with_id(None, source, speed_factor, continuation)
    }

    /// 識別子付きでデコード済みソースをキューに追加する (#354)。
    pub fn enqueue_with_id<S>(
        &self,
        id: Option<u64>,
        source: S,
        speed_factor: f64,
        continuation: bool,
    ) -> Result<(), EngineError>
    where
        S: Source + Send + 'static,
    {
        let mut slot = lock(&self.slot);
        // A fresh performance replaces whatever still plays (as before this
        // slice); a continuation, or a player that already drained, is reused.
        let mut current = match slot.take() {
            Some(previous) if continuation || previous.player.empty() => previous,
            Some(previous) => {
                previous.hard_stop();
                Slot::new(&self.mixer)
            }
            None => Slot::new(&self.mixer),
        };
        current.prune();
        if current.live_count() >= MAX_QUEUED_PHRASES || current.player.len() >= MAX_RAW_APPENDED {
            self.refused_queue_full.fetch_add(1, Ordering::Relaxed);
            *slot = Some(current);
            return Err(engine_error(
                EngineErrorKind::Overloaded,
                "native audio queue is full",
            ));
        }
        let state = Arc::new(AtomicU8::new(ITEM_QUEUED));
        current.items.push((id, Arc::clone(&state)));
        current.player.append(DropUnstarted {
            inner: source.speed((1.0 / speed_factor) as f32),
            drop_unstarted: Arc::clone(&current.drop_unstarted),
            state,
            local_started: false,
        });
        *slot = Some(current);
        Ok(())
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poison| poison.into_inner())
}

impl AudioPlayer for GaplessQueue {
    fn play(&self, request: &AudioPlayRequest) -> Result<(), EngineError> {
        if !request.speed_factor.is_finite() || request.speed_factor <= 0.0 {
            return Err(engine_error(
                EngineErrorKind::InvalidRequest,
                "audio speed_factor must be finite and positive",
            ));
        }
        let candidate = self.resolve_reference(&request.audio_ref)?;
        let path = validate_audio_path(&self.root, &candidate)?;
        let file = File::open(&path).map_err(|error| {
            engine_error(
                EngineErrorKind::Unavailable,
                format!("failed to open native audio file: {error}"),
            )
        })?;
        let source = Decoder::try_from(file).map_err(|error| {
            engine_error(
                EngineErrorKind::InvalidRequest,
                format!("failed to decode native audio file: {error}"),
            )
        })?;
        self.enqueue_with_id(
            request.id,
            source,
            request.speed_factor,
            request.continuation,
        )
    }

    /// A real fade: the volume ramps linearly to zero in 10 ms steps on a
    /// short-lived thread, then the player stops. The call returns at once.
    /// Queued-ahead phrases are dropped right here, so none of them can start
    /// during the fade; the next `play` builds a new player.
    fn stop(&self, fade_ms: u32) -> Result<AudioStopOutcome, EngineError> {
        let Some(slot) = lock(&self.slot).take() else {
            return Ok(AudioStopOutcome::NothingPlaying);
        };
        slot.drop_unstarted.store(true, Ordering::Release);
        if slot.player.empty() || slot.live_count() == 0 {
            slot.hard_stop();
            return Ok(AudioStopOutcome::NothingPlaying);
        }
        let fade_ms = fade_ms.min(MAX_AUDIO_STOP_FADE_MS);
        if fade_ms == 0 {
            slot.hard_stop();
            return Ok(AudioStopOutcome::HardStopped);
        }
        thread::spawn(move || {
            const STEP_MS: u32 = 10;
            let steps = (fade_ms / STEP_MS).max(1);
            let start = slot.player.volume();
            for step in 1..=steps {
                thread::sleep(Duration::from_millis(u64::from(fade_ms / steps)));
                slot.player
                    .set_volume(start * (1.0 - step as f32 / steps as f32));
            }
            slot.hard_stop();
        });
        Ok(AudioStopOutcome::Faded { fade_ms })
    }

    fn supports_gapless_queue(&self) -> bool {
        true
    }

    fn cancel_queued(&self, id: u64) -> bool {
        GaplessQueue::cancel_queued(self, id)
    }
}

#[cfg(feature = "native-audio-spike")]
/// The default output device with a [`GaplessQueue`] on its mixer.
pub struct RodioAudioPlayer {
    _stream: MixerDeviceSink,
    queue: GaplessQueue,
}

#[cfg(feature = "native-audio-spike")]
impl std::fmt::Debug for RodioAudioPlayer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RodioAudioPlayer")
            .field("queue", &self.queue)
            .finish_non_exhaustive()
    }
}

#[cfg(feature = "native-audio-spike")]
impl RodioAudioPlayer {
    pub fn new(root: impl Into<PathBuf>) -> Result<Self, EngineError> {
        let root = root.into();
        if root.as_os_str().is_empty() {
            return Err(engine_error(
                EngineErrorKind::InvalidRequest,
                "audio root must not be empty",
            ));
        }
        let stream = DeviceSinkBuilder::open_default_sink().map_err(|error| {
            engine_error(
                EngineErrorKind::Unavailable,
                format!("failed to open default native audio stream: {error}"),
            )
        })?;
        let queue = GaplessQueue::new(root, stream.mixer().clone())?;
        Ok(Self {
            _stream: stream,
            queue,
        })
    }

    pub fn resolve_reference(&self, audio_ref: &str) -> Result<PathBuf, EngineError> {
        self.queue.resolve_reference(audio_ref)
    }

    /// Hard stop without a fade.
    pub fn stop(&self) {
        self.queue.stop_now();
    }

    pub fn is_idle(&self) -> bool {
        self.queue.is_idle()
    }

    pub fn refused_queue_full(&self) -> u64 {
        self.queue.refused_queue_full()
    }

    /// 最初のサンプルがまだ再生されていないキュー内アイテムを取り消す (#354)。
    pub fn cancel_queued(&self, id: u64) -> bool {
        self.queue.cancel_queued(id)
    }
}

#[cfg(feature = "native-audio-spike")]
impl AudioPlayer for RodioAudioPlayer {
    fn play(&self, request: &AudioPlayRequest) -> Result<(), EngineError> {
        self.queue.play(request)
    }

    fn stop(&self, fade_ms: u32) -> Result<AudioStopOutcome, EngineError> {
        AudioPlayer::stop(&self.queue, fade_ms)
    }

    fn supports_gapless_queue(&self) -> bool {
        true
    }

    fn cancel_queued(&self, id: u64) -> bool {
        self.queue.cancel_queued(id)
    }
}
