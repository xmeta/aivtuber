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
//! * The mixer a new player connects to can be replaced (`set_mixer`), which is
//!   how a lost or changed output device is rebuilt (#354); phrases queued on
//!   the old mixer are ended, because their endpoint is gone.

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
    /// Mixer a new player is connected to. Replaced when the output device is
    /// rebuilt (#354); the slot in use is ended at that point.
    mixer: Mutex<Mixer>,
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
            mixer: Mutex::new(mixer),
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

    /// Replace the mixer new players connect to (#354). Whatever is playing or
    /// queued is ended first: it is connected to the old mixer, whose device is
    /// the one that went away. Never blocks.
    pub fn set_mixer(&self, mixer: Mixer) {
        let previous = lock(&self.slot).take();
        if let Some(previous) = previous {
            previous.hard_stop();
        }
        *lock(&self.mixer) = mixer;
    }

    /// The mixer new players connect to.
    pub fn mixer(&self) -> Mixer {
        lock(&self.mixer).clone()
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
        let mixer = self.mixer();
        let mut slot = lock(&self.slot);
        // A fresh performance replaces whatever still plays (as before this
        // slice); a continuation, or a player that already drained, is reused.
        let mut current = match slot.take() {
            Some(previous) if continuation || previous.player.empty() => previous,
            Some(previous) => {
                previous.hard_stop();
                Slot::new(&mixer)
            }
            None => Slot::new(&mixer),
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

/// Result of comparing the device an output stream is open on with the host's
/// current default output device (#354).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceDecision {
    /// Same name, or nothing to compare: keep the stream.
    Keep,
    /// A different named default device: rebuild the stream.
    Changed,
}

/// Whether the open stream is still the host's default output device (#354).
/// Device-free, so the rule is tested without a sound card.
///
/// Only a different *name* counts. `None` on either side means "not enough
/// information" - no default device at all, or a host that will not name its
/// device - and keeps the stream: ending audio that still works is worse than
/// following the default one phrase later.
pub fn device_decision(opened: Option<&str>, current: Option<&str>) -> DeviceDecision {
    match (opened, current) {
        (Some(opened), Some(current)) if opened != current => DeviceDecision::Changed,
        _ => DeviceDecision::Keep,
    }
}

/// The native backend's device bookkeeping (#354): the device its stream was
/// opened from, a pending rebuild, and the counters.
///
/// Device-free on purpose. The watcher thread only has to report the host's
/// default device name and the cpal callback only has to report an error, so
/// both rules and every counter are tested on any host with no audio device.
#[derive(Debug, Default)]
pub struct DeviceWatch {
    opened: Mutex<Option<String>>,
    pending: AtomicBool,
    reconnects: AtomicU64,
    reopen_failures: AtomicU64,
    degraded: AtomicBool,
}

impl DeviceWatch {
    /// The device of the stream that was opened last.
    pub fn opened(&self) -> Option<String> {
        lock(&self.opened).clone()
    }

    /// Record the device of the stream that was just opened.
    pub fn record_opened(&self, identity: Option<String>) {
        *lock(&self.opened) = identity;
    }

    /// The watcher's lookup, off the play path: request a rebuild when the
    /// host's default device is no longer the one this player opened.
    pub fn note_default(&self, current: Option<&str>) -> DeviceDecision {
        let decision = device_decision(lock(&self.opened).as_deref(), current);
        if decision == DeviceDecision::Changed {
            self.pending.store(true, Ordering::Release);
        }
        decision
    }

    /// The stream's error callback: the endpoint is gone or invalid.
    pub fn note_stream_error(&self) {
        self.pending.store(true, Ordering::Release);
    }

    /// Take a pending rebuild. This is the only device bookkeeping the play
    /// path does, so `play` never enumerates devices itself.
    pub fn take_rebuild(&self) -> bool {
        self.pending.swap(false, Ordering::AcqRel)
    }

    /// A stream was just opened: count the reconnect and leave the degraded
    /// state.
    pub fn note_reconnected(&self) -> u64 {
        self.degraded.store(false, Ordering::Release);
        self.reconnects.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// A reopen found no usable device; `true` the first time in a run of
    /// failures, so the log says it once rather than once per phrase.
    pub fn note_reopen_failure(&self) -> bool {
        self.reopen_failures.fetch_add(1, Ordering::Relaxed);
        !self.degraded.swap(true, Ordering::AcqRel)
    }

    /// Output streams reopened after a loss or a default-device change.
    pub fn reconnects(&self) -> u64 {
        self.reconnects.load(Ordering::Relaxed)
    }

    /// Reopen attempts that found no usable output device.
    pub fn reopen_failures(&self) -> u64 {
        self.reopen_failures.load(Ordering::Relaxed)
    }
}

#[cfg(feature = "native-audio-spike")]
/// How often the watcher looks up the default output device. It runs on its own
/// thread (#354), so a phrase burst on the play path never queries the host.
const DEVICE_CHECK_INTERVAL: Duration = Duration::from_secs(1);

#[cfg(feature = "native-audio-spike")]
/// Name of the host's current default output device, if it reports one.
fn default_device_identity() -> Option<String> {
    use rodio::cpal::traits::{DeviceTrait, HostTrait};
    rodio::cpal::default_host()
        .default_output_device()
        .and_then(|device| device.description().ok())
        .map(|description| description.name().to_owned())
}

#[cfg(feature = "native-audio-spike")]
/// Watch the default output device on a thread of its own (#354). `play` only
/// reads the flag this sets; the thread ends when the player is dropped.
fn spawn_device_watcher(watch: Arc<DeviceWatch>, stopping: Arc<AtomicBool>) -> std::io::Result<()> {
    thread::Builder::new()
        .name("aivtuber-audio-device-watch".to_owned())
        .spawn(move || {
            // One line per newly seen device, not one per interval: a rebuild
            // that keeps failing retries from the play path, quietly.
            let mut announced: Option<String> = None;
            while !stopping.load(Ordering::Acquire) {
                thread::sleep(DEVICE_CHECK_INTERVAL);
                let current = default_device_identity();
                if watch.note_default(current.as_deref()) == DeviceDecision::Changed
                    && current != announced
                {
                    eprintln!(
                        "audio: default output device changed to {}; the native stream is rebuilt before the next phrase",
                        current.as_deref().unwrap_or("unknown")
                    );
                    announced = current;
                }
            }
        })
        .map(|_detached| ())
}

#[cfg(feature = "native-audio-spike")]
/// Open the default output device and record it in `watch`, whose stream error
/// callback requests a rebuild (Windows reports a removed endpoint there)
/// (#354).
fn open_device(watch: Arc<DeviceWatch>) -> Result<MixerDeviceSink, String> {
    let identity = default_device_identity();
    let callback_watch = Arc::clone(&watch);
    let opened = DeviceSinkBuilder::from_default_device().and_then(|builder| {
        builder
            .with_error_callback(move |error| {
                eprintln!("audio: native output stream error: {error}");
                callback_watch.note_stream_error();
            })
            .open_stream()
    });
    let (mut sink, identity) = match opened {
        Ok(sink) => (sink, identity),
        // The default device would not open: take the fallback the plain
        // `open_default_sink` takes, but without a name to compare against, so
        // only a stream error reopens this one.
        Err(_) => (
            DeviceSinkBuilder::open_default_sink().map_err(|error| error.to_string())?,
            None,
        ),
    };
    // A reopen drops the previous sink on purpose; the player logs that itself,
    // once, rather than one line per dropped stream.
    sink.log_on_drop(false);
    watch.record_opened(identity);
    Ok(sink)
}

#[cfg(feature = "native-audio-spike")]
/// The default output device with a [`GaplessQueue`] on its mixer.
///
/// The stream is not held for the lifetime of the player: before each `play`
/// it is reopened when a rebuild is pending (#354). A watcher thread compares
/// the default output device with the one this player opened, and the stream's
/// error callback reports a removed endpoint; both only set a flag, so the play
/// path never enumerates devices. A reopen ends the audio in flight (its
/// endpoint is the one that went away) and drops the phrases queued behind it;
/// the first phrase after it plays on the new device. Counters:
/// [`Self::reconnects`] and [`Self::reopen_failures`].
///
/// A missing device never stops the performer: `play` fails with
/// `Unavailable` (the scheduler records the failure and carries on) and `stop`
/// keeps working, because it needs no device.
pub struct RodioAudioPlayer {
    queue: GaplessQueue,
    device: Mutex<Option<MixerDeviceSink>>,
    watch: Arc<DeviceWatch>,
    /// Ends the watcher thread when the player is dropped.
    stopping: Arc<AtomicBool>,
}

#[cfg(feature = "native-audio-spike")]
impl Drop for RodioAudioPlayer {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Release);
    }
}

#[cfg(feature = "native-audio-spike")]
impl std::fmt::Debug for RodioAudioPlayer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RodioAudioPlayer")
            .field("queue", &self.queue)
            .field("reconnects", &self.reconnects())
            .field("reopen_failures", &self.reopen_failures())
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
        let watch = Arc::new(DeviceWatch::default());
        let device = open_device(Arc::clone(&watch)).map_err(|reason| {
            engine_error(
                EngineErrorKind::Unavailable,
                format!("failed to open default native audio stream: {reason}"),
            )
        })?;
        let queue = GaplessQueue::new(root, device.mixer().clone())?;
        let stopping = Arc::new(AtomicBool::new(false));
        if let Err(error) = spawn_device_watcher(Arc::clone(&watch), Arc::clone(&stopping)) {
            // Without the watcher only the stream error reopens the device; the
            // backend still works, so this is one line, not a failure.
            eprintln!(
                "audio: could not start the native device watcher ({error}); only a stream error will reopen the device"
            );
        }
        Ok(Self {
            queue,
            device: Mutex::new(Some(device)),
            watch,
            stopping,
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

    /// Output streams reopened after a loss or a default-device change.
    pub fn reconnects(&self) -> u64 {
        self.watch.reconnects()
    }

    /// Reopen attempts that found no usable output device.
    pub fn reopen_failures(&self) -> u64 {
        self.watch.reopen_failures()
    }

    /// 最初のサンプルがまだ再生されていないキュー内アイテムを取り消す (#354)。
    pub fn cancel_queued(&self, id: u64) -> bool {
        self.queue.cancel_queued(id)
    }

    /// Rebuild the output stream when one is pending (#354). Called before every
    /// `play`, which is why it does no host lookup at all: the watcher thread
    /// and the stream callback set the flag, and this only takes it.
    fn ensure_device(&self) -> Result<(), EngineError> {
        let mut device = lock(&self.device);
        let rebuild = self.watch.take_rebuild();
        if device.is_some() && !rebuild {
            return Ok(());
        }
        // The old endpoint is gone or is no longer the default: end it (with
        // the phrase in flight) before opening the new one.
        *device = None;
        match open_device(Arc::clone(&self.watch)) {
            Ok(sink) => {
                self.queue.set_mixer(sink.mixer().clone());
                *device = Some(sink);
                let count = self.watch.note_reconnected();
                let identity = self.watch.opened();
                eprintln!(
                    "audio: native output device reopened after a change or loss (reconnect {count}, device {})",
                    identity.as_deref().unwrap_or("unknown")
                );
                Ok(())
            }
            Err(reason) => {
                if self.watch.note_reopen_failure() {
                    eprintln!(
                        "audio: native output device unavailable; playback stays off until one is back: {reason}"
                    );
                }
                Err(engine_error(
                    EngineErrorKind::Unavailable,
                    format!("native audio output device unavailable: {reason}"),
                ))
            }
        }
    }
}

#[cfg(feature = "native-audio-spike")]
impl AudioPlayer for RodioAudioPlayer {
    fn play(&self, request: &AudioPlayRequest) -> Result<(), EngineError> {
        self.ensure_device()?;
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
