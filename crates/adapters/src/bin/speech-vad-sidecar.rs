//! Local speech-activity sidecar (#92 S2).
//!
//! Captures the default microphone (or reads `--input-wav`), runs Silero VAD
//! v6 and writes `speech-observation` batches (schemas/speech-observation
//! .schema.json) as JSON lines on stdout, one line per raw speech edge. There
//! is no debounce and no turn-end score. Diagnostics go to stderr only.
//!
//! ```text
//! speech-vad-sidecar --model silero_vad_op18_ifless.onnx --language ja
//! ```
//!
//! Timeline: by default the parent (aivtuber-app) writes its monotonic
//! `now_ms` to this process's stdin at start and every few seconds
//! (`{"clock_sync":{"runtime_now_ms":N}}`); observations are stamped on that
//! timeline. `--clock local` stamps on the sidecar's own clock instead (no
//! parent; used by tests and for running by hand). When stdin closes the
//! parent has gone and the sidecar exits.
//!
//! Privacy: audio never leaves this process (except, with `--stt-worker`, to
//! the one local STT child over its stdin pipe) and is never written
//! anywhere. The bounded pre-roll is memory only.
//!
//! Speech-to-text (#92 S3b, optional): with `--stt-worker PROGRAM` the sidecar
//! spawns `speech-stt-worker` as a separate child, cuts VAD speech into
//! segments (at least 300 ms of pre-roll, an end-edge tail, a minimum speech
//! span) and feeds them to it. The worker's `speech-transcript` lines are
//! relayed on this process's stdout next to the observation lines, with one
//! `{"stt":{...}}` status line when the transcript path becomes unavailable.
//! A dead or hung worker costs transcripts only: VAD edges keep flowing.

use aivtuber_adapters::speech_stt::feeder::{FeederConfig, FrameSink, SegmentFeeder};
use aivtuber_adapters::speech_stt::supervisor::{SttWorker, SttWorkerConfig};
#[cfg(feature = "mic-capture")]
use aivtuber_adapters::speech_vad::StreamClock;
use aivtuber_adapters::speech_vad::{
    FRAME_MS, FeedHeartbeat, HysteresisConfig, LineSink, MAX_PREROLL_MS, ObservationStamper,
    PreRoll, RawEdge, RuntimeClockMap, SILERO_PROVIDER_VERSION, SileroVad, StamperConfig,
    VadPipeline, pump_clock_syncs,
};
use aivtuber_domain::SpeechEdge;
use std::io::BufReader;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const PROVIDER_ID: &str = "silero-vad-sidecar";
/// Output lines queued for the writer thread. Edges arrive at most once per
/// 32 ms frame, so this is several seconds of backlog.
const OUTPUT_QUEUE: usize = 64;
const SYNC_TIMEOUT_DEFAULT_MS: u64 = 3_000;

#[derive(Clone, Copy, PartialEq, Eq)]
enum ClockMode {
    Sync,
    Local,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Pace {
    Realtime,
    Max,
}

/// Speech-to-text options; present only with `--stt-worker`.
struct SttArgs {
    worker: String,
    model: Option<String>,
    model_dir: Option<String>,
    threads: Option<String>,
    extra: Vec<String>,
    feeder: FeederConfig,
    startup_timeout: Duration,
    heartbeat_timeout: Duration,
    drain: Duration,
    /// Wait this long for the worker to load before processing input. For
    /// file input (`--input-wav`) so a short clip is not skipped while the
    /// model loads; a live microphone never waits.
    wait_ready: Duration,
}

struct Args {
    stt: Option<SttArgs>,
    model: Option<String>,
    language: String,
    speaker_id: String,
    input_wav: Option<String>,
    input_device: Option<String>,
    list_devices: bool,
    pace: Pace,
    clock: ClockMode,
    sync_timeout: Duration,
    preroll_ms: u64,
    hysteresis: HysteresisConfig,
    clock_probe_ms: Option<u64>,
    duration: Option<Duration>,
    stats_every: Duration,
    /// Feed-liveness beat interval (#81 S2); 0 = no beat (the default, so a
    /// consumer that does not know the line never sees it).
    feed_heartbeat_ms: u64,
}

fn usage() -> String {
    "usage: speech-vad-sidecar --model <silero_vad_op18_ifless.onnx> --language <tag> \
     [--speaker-id ID] [--input-wav FILE [--pace realtime|max]] [--input-device NAME] \
     [--list-devices] [--clock sync|local] [--sync-timeout-ms N] [--preroll-ms N] \
     [--start-threshold P] [--end-threshold P] [--clock-probe-ms N] [--duration-secs N] \
     [--stats-secs N] [--feed-heartbeat-ms N] [--stt-worker PROGRAM [--stt-model k2v2-int8|parakeet-ja \
     --stt-model-dir DIR] [--stt-threads N] [--stt-preroll-ms N] [--stt-tail-ms N] \
     [--stt-min-speech-ms N] [--stt-max-segment-ms N] [--stt-startup-timeout-ms N] \
     [--stt-heartbeat-timeout-ms N] [--stt-drain-ms N] [--stt-wait-ready-ms N] \
     [--stt-arg ARG]...]"
        .to_owned()
}

fn parse_args() -> Result<Args, String> {
    let mut stt_worker: Option<String> = None;
    let mut stt_model = None;
    let mut stt_model_dir = None;
    let mut stt_threads = None;
    let mut stt_extra = Vec::new();
    let (mut stt_preroll, mut stt_tail, mut stt_min_speech, mut stt_max_segment) =
        (300_u64, 500_u64, 250_u64, 25_000_u64);
    let (mut stt_startup, mut stt_heartbeat, mut stt_drain) = (60_000_u64, 5_000_u64, 8_000_u64);
    let mut stt_wait_ready = 0_u64;
    let mut a = Args {
        stt: None,
        model: None,
        language: String::new(),
        speaker_id: "mic-local".to_owned(),
        input_wav: None,
        input_device: None,
        list_devices: false,
        pace: Pace::Max,
        clock: ClockMode::Sync,
        sync_timeout: Duration::from_millis(SYNC_TIMEOUT_DEFAULT_MS),
        preroll_ms: 300,
        hysteresis: HysteresisConfig::default(),
        clock_probe_ms: None,
        duration: None,
        stats_every: Duration::from_secs(10),
        feed_heartbeat_ms: 0,
    };
    let mut iter = std::env::args().skip(1);
    while let Some(flag) = iter.next() {
        let mut value = |name: &str| iter.next().ok_or_else(|| format!("{name} needs a value"));
        let int = |text: String, name: &str| {
            text.parse::<u64>()
                .map_err(|_| format!("{name} must be a non-negative integer"))
        };
        let float = |text: String, name: &str| {
            text.parse::<f32>()
                .map_err(|_| format!("{name} must be a number"))
        };
        match flag.as_str() {
            "--model" => a.model = Some(value("--model")?),
            "--language" => a.language = value("--language")?,
            "--speaker-id" => a.speaker_id = value("--speaker-id")?,
            "--input-wav" => a.input_wav = Some(value("--input-wav")?),
            "--input-device" => a.input_device = Some(value("--input-device")?),
            "--list-devices" => a.list_devices = true,
            "--pace" => {
                a.pace = match value("--pace")?.as_str() {
                    "realtime" => Pace::Realtime,
                    "max" => Pace::Max,
                    _ => return Err("--pace must be realtime or max".to_owned()),
                }
            }
            "--clock" => {
                a.clock = match value("--clock")?.as_str() {
                    "sync" => ClockMode::Sync,
                    "local" => ClockMode::Local,
                    _ => return Err("--clock must be sync or local".to_owned()),
                }
            }
            "--sync-timeout-ms" => {
                a.sync_timeout =
                    Duration::from_millis(int(value("--sync-timeout-ms")?, "--sync-timeout-ms")?)
            }
            "--preroll-ms" => {
                a.preroll_ms = int(value("--preroll-ms")?, "--preroll-ms")?;
                if a.preroll_ms > MAX_PREROLL_MS {
                    return Err(format!("--preroll-ms must be at most {MAX_PREROLL_MS}"));
                }
            }
            "--start-threshold" => {
                a.hysteresis.start_threshold =
                    float(value("--start-threshold")?, "--start-threshold")?
            }
            "--end-threshold" => {
                a.hysteresis.end_threshold = float(value("--end-threshold")?, "--end-threshold")?
            }
            "--clock-probe-ms" => {
                a.clock_probe_ms = Some(int(value("--clock-probe-ms")?, "--clock-probe-ms")?.max(1))
            }
            "--duration-secs" => {
                a.duration = Some(Duration::from_secs(int(
                    value("--duration-secs")?,
                    "--duration-secs",
                )?))
            }
            "--stats-secs" => {
                a.stats_every =
                    Duration::from_secs(int(value("--stats-secs")?, "--stats-secs")?.max(1))
            }
            "--feed-heartbeat-ms" => {
                a.feed_heartbeat_ms = int(value("--feed-heartbeat-ms")?, "--feed-heartbeat-ms")?;
                if a.feed_heartbeat_ms != 0 {
                    FeedHeartbeat::new(a.feed_heartbeat_ms)?;
                }
            }
            "--stt-worker" => stt_worker = Some(value("--stt-worker")?),
            "--stt-model" => stt_model = Some(value("--stt-model")?),
            "--stt-model-dir" => stt_model_dir = Some(value("--stt-model-dir")?),
            "--stt-threads" => stt_threads = Some(value("--stt-threads")?),
            "--stt-arg" => stt_extra.push(value("--stt-arg")?),
            "--stt-preroll-ms" => {
                stt_preroll = int(value("--stt-preroll-ms")?, "--stt-preroll-ms")?
            }
            "--stt-tail-ms" => stt_tail = int(value("--stt-tail-ms")?, "--stt-tail-ms")?,
            "--stt-min-speech-ms" => {
                stt_min_speech = int(value("--stt-min-speech-ms")?, "--stt-min-speech-ms")?
            }
            "--stt-max-segment-ms" => {
                stt_max_segment = int(value("--stt-max-segment-ms")?, "--stt-max-segment-ms")?
            }
            "--stt-startup-timeout-ms" => {
                stt_startup = int(
                    value("--stt-startup-timeout-ms")?,
                    "--stt-startup-timeout-ms",
                )?
            }
            "--stt-heartbeat-timeout-ms" => {
                stt_heartbeat = int(
                    value("--stt-heartbeat-timeout-ms")?,
                    "--stt-heartbeat-timeout-ms",
                )?
            }
            "--stt-drain-ms" => stt_drain = int(value("--stt-drain-ms")?, "--stt-drain-ms")?,
            "--stt-wait-ready-ms" => {
                stt_wait_ready = int(value("--stt-wait-ready-ms")?, "--stt-wait-ready-ms")?
            }
            "-h" | "--help" => return Err(usage()),
            other => return Err(format!("unknown argument {other}\n{}", usage())),
        }
    }
    a.hysteresis.validate()?;
    if let Some(worker) = stt_worker {
        let feeder = FeederConfig::from_ms(stt_preroll, stt_tail, stt_min_speech, stt_max_segment);
        feeder.validate()?;
        a.stt = Some(SttArgs {
            worker,
            model: stt_model,
            model_dir: stt_model_dir,
            threads: stt_threads,
            extra: stt_extra,
            feeder,
            startup_timeout: Duration::from_millis(stt_startup),
            heartbeat_timeout: Duration::from_millis(stt_heartbeat),
            drain: Duration::from_millis(stt_drain),
            wait_ready: Duration::from_millis(stt_wait_ready),
        });
    } else if stt_model.is_some() || stt_model_dir.is_some() || !stt_extra.is_empty() {
        return Err("--stt-* options need --stt-worker".to_owned());
    }
    Ok(a)
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("speech-vad-sidecar: {message}");
            ExitCode::FAILURE
        }
    }
}

/// Shared timeline state: the local clock origin and the runtime mapping.
struct Timeline {
    started: Instant,
    map: Mutex<RuntimeClockMap>,
    parent_gone: AtomicBool,
}

impl Timeline {
    fn local_ms(&self) -> f64 {
        self.started.elapsed().as_secs_f64() * 1000.0
    }
    fn runtime_ms(&self, local_ms: f64) -> Result<u64, aivtuber_adapters::speech_vad::ClockError> {
        self.map.lock().expect("clock map").to_runtime_ms(local_ms)
    }
}

fn start_clock(args: &Args) -> Result<Arc<Timeline>, String> {
    let timeline = Arc::new(Timeline {
        started: Instant::now(),
        map: Mutex::new(RuntimeClockMap::default()),
        parent_gone: AtomicBool::new(false),
    });
    match args.clock {
        ClockMode::Local => {
            // Runtime time is this process's own elapsed time.
            timeline.map.lock().expect("clock map").on_sync(0, 0.0);
        }
        ClockMode::Sync => {
            let shared = timeline.clone();
            std::thread::spawn(move || {
                let stdin = std::io::stdin();
                let _ = pump_clock_syncs(BufReader::new(stdin.lock()), |runtime_now_ms| {
                    let local = shared.local_ms();
                    shared
                        .map
                        .lock()
                        .expect("clock map")
                        .on_sync(runtime_now_ms, local);
                });
                shared.parent_gone.store(true, Ordering::Relaxed);
            });
            let deadline = Instant::now() + args.sync_timeout;
            while !timeline.map.lock().expect("clock map").is_synced() {
                if Instant::now() >= deadline || timeline.parent_gone.load(Ordering::Relaxed) {
                    return Err(
                        "no clock sync from the runtime (use --clock local to run standalone)"
                            .to_owned(),
                    );
                }
                std::thread::sleep(Duration::from_millis(2));
            }
        }
    }
    Ok(timeline)
}

struct Emitter {
    stamper: ObservationStamper,
    sink: LineSink,
    edges_emitted: u64,
    stamp_errors: u64,
    stt: Option<SegmentFeeder<SttWorker>>,
    heartbeat: Option<FeedHeartbeat>,
}

impl Args {
    /// Length of the pipeline's pre-roll ring: the S2 setting, or what the STT
    /// segmenter needs to reach back from a deferred segment open.
    fn ring_ms(&self) -> u64 {
        self.stt
            .as_ref()
            .map_or(self.preroll_ms, |s| self.preroll_ms.max(s.feeder.ring_ms()))
    }
}

impl Emitter {
    /// Feed liveness (#81 S2): called only right after a chunk of audio went
    /// through the scorer, so a stalled device or scorer stops the beat. The
    /// line is dropped, never queued behind edges, when the sink is full.
    fn beat(&mut self) {
        if let Some(heartbeat) = &mut self.heartbeat
            && let Some(line) = heartbeat.due(Instant::now())
        {
            self.sink.send(line);
        }
    }

    /// Edges out, and the STT segmenter stepped on the same stream position.
    fn step(
        &mut self,
        edges: &[RawEdge],
        ring: &PreRoll,
        map: impl Fn(u64) -> Result<u64, aivtuber_adapters::speech_vad::ClockError>,
    ) {
        self.emit(edges, &map);
        if let Some(stt) = &mut self.stt {
            stt.on_step(edges, ring, &map);
        }
    }

    fn emit(
        &mut self,
        edges: &[RawEdge],
        map: impl Fn(u64) -> Result<u64, aivtuber_adapters::speech_vad::ClockError>,
    ) {
        if edges.is_empty() {
            return;
        }
        match self.stamper.lines(edges, &map) {
            Ok(lines) => {
                self.edges_emitted += edges.len() as u64;
                for line in lines {
                    self.sink.send(line);
                }
            }
            Err(error) => {
                self.stamp_errors += 1;
                eprintln!(
                    "speech-vad-sidecar: dropped {} edge(s): {error}",
                    edges.len()
                );
            }
        }
    }
}

fn run() -> Result<(), String> {
    let args = parse_args()?;
    if args.list_devices {
        return list_devices();
    }
    if args.language.is_empty() {
        return Err("--language is required (declared, never inferred)".to_owned());
    }
    let stamper = ObservationStamper::new(StamperConfig {
        provider_id: PROVIDER_ID.to_owned(),
        provider_version: SILERO_PROVIDER_VERSION.to_owned(),
        language: args.language.clone(),
        speaker_id: args.speaker_id.clone(),
    })?;
    let mut emitter = Emitter {
        stamper,
        sink: LineSink::spawn(std::io::stdout(), OUTPUT_QUEUE),
        edges_emitted: 0,
        stamp_errors: 0,
        stt: None,
        heartbeat: match args.feed_heartbeat_ms {
            0 => None,
            ms => Some(FeedHeartbeat::new(ms)?),
        },
    };

    if let Some(interval_ms) = args.clock_probe_ms {
        let timeline = start_clock(&args)?;
        run_clock_probe(&args, interval_ms, &timeline, &mut emitter);
        return finish(emitter, None, Duration::ZERO);
    }
    let drain = args.stt.as_ref().map_or(Duration::ZERO, |s| s.drain);
    emitter.stt = start_stt(&args, &emitter);
    if args.input_wav.is_some()
        && let (Some(stt), Some(feeder)) = (&args.stt, &emitter.stt)
    {
        let deadline = Instant::now() + stt.wait_ready;
        while !feeder.sink().ready() && !feeder.sink().is_failed() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    let model = args.model.as_deref().ok_or("--model is required")?;
    let scorer = SileroVad::from_file(std::path::Path::new(model))?;
    let timeline = start_clock(&args)?;
    eprintln!(
        "speech-vad-sidecar: ready provider_version={SILERO_PROVIDER_VERSION} language={} frame_ms={FRAME_MS} preroll_ms={} stt={}",
        args.language,
        args.ring_ms(),
        emitter.stt.is_some()
    );
    let stats = match &args.input_wav {
        Some(path) => run_wav(&args, path, scorer, &timeline, &mut emitter)?,
        None => run_mic(&args, scorer, &timeline, &mut emitter)?,
    };
    finish(emitter, Some(stats), drain)
}

/// Spawn and wire the STT worker when `--stt-worker` was given. A worker that
/// cannot start has already told the app (an `unavailable` status line); the
/// sidecar carries on with VAD edges only.
fn start_stt(args: &Args, emitter: &Emitter) -> Option<SegmentFeeder<SttWorker>> {
    let stt = args.stt.as_ref()?;
    let mut worker_args = vec![
        "--language".to_owned(),
        args.language.clone(),
        "--speaker-id".to_owned(),
        args.speaker_id.clone(),
    ];
    for (flag, value) in [
        ("--model", &stt.model),
        ("--model-dir", &stt.model_dir),
        ("--threads", &stt.threads),
    ] {
        if let Some(value) = value {
            worker_args.push(flag.to_owned());
            worker_args.push(value.clone());
        }
    }
    worker_args.extend(stt.extra.iter().cloned());
    let mut config = SttWorkerConfig::new(&stt.worker, worker_args);
    config.startup_timeout = stt.startup_timeout;
    config.heartbeat_timeout = stt.heartbeat_timeout;
    // The real worker is always started with --stt-model, and its libraries
    // are pinned; a worker without a model (a test double) loads no engine.
    config.verify_engine_libs = stt.model.is_some();
    let handle = emitter.sink.handle()?;
    match SttWorker::spawn(config, handle) {
        Ok(worker) => match SegmentFeeder::new(stt.feeder, worker) {
            Ok(feeder) => {
                eprintln!("speech-vad-sidecar: stt worker started; waiting for it to load");
                Some(feeder)
            }
            Err(error) => {
                eprintln!("speech-vad-sidecar: stt disabled: {error}");
                None
            }
        },
        Err(error) => {
            eprintln!("speech-vad-sidecar: stt worker could not start: {error}; no transcripts");
            None
        }
    }
}

fn finish(
    mut emitter: Emitter,
    pipeline: Option<aivtuber_adapters::speech_vad::PipelineStats>,
    drain: Duration,
) -> Result<(), String> {
    // Let the worker finish the open segment and the relay carry its lines
    // before the output queue is closed.
    if let Some(feeder) = emitter.stt.take() {
        let feeder_stats = feeder.stats();
        let mut worker = feeder.into_sink();
        worker.shutdown(drain);
        eprintln!(
            "speech-vad-sidecar: stt feeder={feeder_stats:?} worker={:?} failure={:?}",
            worker.stats(),
            worker.failure()
        );
    }
    let edges = emitter.edges_emitted;
    let stamp_errors = emitter.stamp_errors;
    let sink = emitter.sink.finish(Duration::from_secs(2));
    eprintln!(
        "speech-vad-sidecar: final edges={edges} lines_written={} dropped_queue_full={} stamp_errors={stamp_errors} pipeline={pipeline:?}",
        sink.lines_written, sink.dropped_queue_full
    );
    Ok(())
}

fn list_devices() -> Result<(), String> {
    #[cfg(feature = "mic-capture")]
    {
        for name in aivtuber_adapters::speech_vad::mic::list_input_devices()? {
            eprintln!("{name}");
        }
        Ok(())
    }
    #[cfg(not(feature = "mic-capture"))]
    Err("built without the mic-capture feature".to_owned())
}

/// Emits alternating start/end edges stamped with the current time, without a
/// model or audio. Used to measure how far a stamp lags its arrival at the
/// parent (clock mapping error plus pipe latency).
fn run_clock_probe(args: &Args, interval_ms: u64, timeline: &Timeline, emitter: &mut Emitter) {
    let deadline = args.duration.map(|d| Instant::now() + d);
    let mut start = true;
    loop {
        if timeline.parent_gone.load(Ordering::Relaxed) && args.clock == ClockMode::Sync {
            return;
        }
        if deadline.is_some_and(|d| Instant::now() >= d) || emitter.sink.is_closed() {
            return;
        }
        let local = timeline.local_ms();
        let edge = RawEdge {
            edge: if start {
                SpeechEdge::Start
            } else {
                SpeechEdge::End
            },
            sample16: 0,
            confidence: None,
        };
        start = !start;
        emitter.emit(&[edge], |_| timeline.runtime_ms(local));
        emitter.beat();
        std::thread::sleep(Duration::from_millis(interval_ms));
    }
}

fn run_wav(
    args: &Args,
    path: &str,
    scorer: SileroVad,
    timeline: &Timeline,
    emitter: &mut Emitter,
) -> Result<aivtuber_adapters::speech_vad::PipelineStats, String> {
    let mut reader = hound::WavReader::open(path).map_err(|e| format!("wav {path}: {e}"))?;
    let spec = reader.spec();
    let channels = usize::from(spec.channels.max(1));
    let rate = spec.sample_rate;
    let samples: Box<dyn Iterator<Item = f32>> = match (spec.sample_format, spec.bits_per_sample) {
        (hound::SampleFormat::Float, 32) => Box::new(reader.samples::<f32>().map_while(Result::ok)),
        (hound::SampleFormat::Int, 16) => Box::new(
            reader
                .samples::<i16>()
                .map_while(Result::ok)
                .map(|s| f32::from(s) / 32_768.0),
        ),
        (hound::SampleFormat::Int, 24 | 32) => {
            let shift = f64::from(spec.bits_per_sample - 1);
            Box::new(
                reader
                    .samples::<i32>()
                    .map_while(Result::ok)
                    .map(move |s| (f64::from(s) / 2f64.powf(shift)) as f32),
            )
        }
        other => return Err(format!("unsupported WAV sample format {other:?}")),
    };
    let mut pipeline = VadPipeline::new(scorer, rate, args.hysteresis, args.ring_ms())?;
    // 10 ms of input per step, like a device callback.
    let step = (rate as usize / 100).max(1) * channels;
    let origin_local = timeline.local_ms();
    // For measurements: where the start of the file falls on the runtime
    // timeline (the ground truth for end-to-end latency against known clip
    // boundaries).
    if let Ok(origin_runtime) = timeline.runtime_ms(origin_local) {
        eprintln!("speech-vad-sidecar: wav_origin_runtime_ms={origin_runtime}");
    }
    let mut samples = samples.peekable();
    let mut edges = Vec::new();
    let mut chunk_index = 0_u64;
    while samples.peek().is_some() {
        let raw: Vec<f32> = samples.by_ref().take(step).collect();
        let mono: Vec<f32> = raw
            .chunks(channels)
            .map(|f| f.iter().sum::<f32>() / f.len() as f32)
            .collect();
        chunk_index += 1;
        if args.pace == Pace::Realtime {
            let due = origin_local + chunk_index as f64 * 10.0;
            let wait = due - timeline.local_ms();
            if wait > 0.0 {
                std::thread::sleep(Duration::from_secs_f64(wait / 1000.0));
            }
        }
        edges.clear();
        pipeline.push_input(&mono, &mut edges);
        emitter.step(&edges, pipeline.preroll(), |s| {
            timeline.runtime_ms(origin_local + s as f64 / 16.0)
        });
        emitter.beat();
        if emitter.sink.is_closed() {
            break;
        }
    }
    edges.clear();
    pipeline.finish(&mut edges);
    emitter.emit(&edges, |s| {
        timeline.runtime_ms(origin_local + s as f64 / 16.0)
    });
    if let Some(stt) = &mut emitter.stt {
        stt.on_finish();
    }
    Ok(pipeline.stats())
}

#[cfg(feature = "mic-capture")]
fn run_mic(
    args: &Args,
    scorer: SileroVad,
    timeline: &Timeline,
    emitter: &mut Emitter,
) -> Result<aivtuber_adapters::speech_vad::PipelineStats, String> {
    use aivtuber_adapters::speech_vad::mic::start_capture;
    use std::sync::mpsc::{RecvTimeoutError, sync_channel};

    // About 2 s of 10 ms callbacks. A slower consumer drops chunks (a counted
    // gap) instead of growing memory.
    let (sender, receiver) = sync_channel(256);
    let capture = start_capture(args.input_device.as_deref(), sender)?;
    eprintln!(
        "speech-vad-sidecar: capturing device={:?} input_rate_hz={}",
        capture.device_name, capture.sample_rate_hz
    );
    let rate = capture.sample_rate_hz;
    let mut pipeline = VadPipeline::new(scorer, rate, args.hysteresis, args.ring_ms())?;
    let mut stream_clock = StreamClock::default();
    let mut input_samples = 0_u64;
    let mut seen_dropped = 0_u64;
    let mut edges = Vec::new();
    let deadline = args.duration.map(|d| Instant::now() + d);
    let mut next_stats = Instant::now() + args.stats_every;
    loop {
        if deadline.is_some_and(|d| Instant::now() >= d)
            || emitter.sink.is_closed()
            || capture.failed.load(Ordering::Relaxed)
            || (args.clock == ClockMode::Sync && timeline.parent_gone.load(Ordering::Relaxed))
        {
            break;
        }
        let chunk = match receiver.recv_timeout(Duration::from_millis(100)) {
            Ok(chunk) => chunk,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break,
        };
        let dropped = capture.dropped_chunks.load(Ordering::Relaxed);
        if dropped != seen_dropped {
            seen_dropped = dropped;
            edges.clear();
            pipeline.gap(&mut edges);
            // Map the forced end on the old clock, then restart the stream.
            emitter.emit(&edges, |s| {
                let local = stream_clock
                    .sample16_local_ms(s)
                    .ok_or(aivtuber_adapters::speech_vad::ClockError::NotSynced)?;
                timeline.runtime_ms(local)
            });
            if let Some(stt) = &mut emitter.stt {
                stt.on_gap();
            }
            stream_clock.reset();
            input_samples = 0;
        }
        let arrival_local = chunk
            .arrived
            .saturating_duration_since(timeline.started)
            .as_secs_f64()
            * 1000.0;
        input_samples += chunk.mono.len() as u64;
        stream_clock.observe_chunk(arrival_local, input_samples, rate);
        edges.clear();
        pipeline.push_input(&chunk.mono, &mut edges);
        emitter.step(&edges, pipeline.preroll(), |s| {
            let local = stream_clock
                .sample16_local_ms(s)
                .ok_or(aivtuber_adapters::speech_vad::ClockError::NotSynced)?;
            timeline.runtime_ms(local)
        });
        emitter.beat();
        if Instant::now() >= next_stats {
            next_stats = Instant::now() + args.stats_every;
            let sink = emitter.sink.stats();
            eprintln!(
                "speech-vad-sidecar: stats pipeline={:?} dropped_audio_chunks={dropped} lines_written={} dropped_queue_full={} preroll_len={}",
                pipeline.stats(),
                sink.lines_written,
                sink.dropped_queue_full,
                pipeline.preroll().len()
            );
        }
    }
    edges.clear();
    pipeline.finish(&mut edges);
    emitter.emit(&edges, |s| {
        let local = stream_clock
            .sample16_local_ms(s)
            .ok_or(aivtuber_adapters::speech_vad::ClockError::NotSynced)?;
        timeline.runtime_ms(local)
    });
    if let Some(stt) = &mut emitter.stt {
        stt.on_finish();
    }
    Ok(pipeline.stats())
}

#[cfg(not(feature = "mic-capture"))]
fn run_mic(
    _args: &Args,
    _scorer: SileroVad,
    _timeline: &Timeline,
    _emitter: &mut Emitter,
) -> Result<aivtuber_adapters::speech_vad::PipelineStats, String> {
    Err("microphone capture needs the mic-capture feature; use --input-wav".to_owned())
}
