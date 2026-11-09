//! Cuts VAD speech into STT segments inside the speech sidecar (issue #92, S3b).
//!
//! Raw VAD edges are not utterances: Silero drops below its end threshold on
//! every breath, so transcribing between raw edges would shred sentences. The
//! feeder owns the STT-side policy EXP-0002 section 9 assigned to S3b:
//!
//! * A segment is opened only once the speech span reaches `min_speech`
//!   (a cough or click never reaches the worker) and then starts
//!   `preroll` before the first start edge, recovering the onset the VAD
//!   reported late.
//! * An end edge starts a `tail` of silence. A start edge inside the tail
//!   resumes the same segment; the tail running out closes it.
//! * A segment is split at `max_segment` so the worker's hard cap is a
//!   backstop, not the normal path.
//!
//! Everything is positions on the 16 kHz stream. The caller maps a position to
//! runtime time (`map`), so the worker needs no clock. Audio comes from the
//! bounded [`PreRoll`] ring; nothing here keeps audio itself.
//!
//! Backpressure: the worker pipe is bounded and never blocks the VAD. A frame
//! the sink refuses damages the open segment (its remaining audio is not sent
//! and its END carries the skip flag, so spliced audio is never decoded); a
//! START the sink refuses skips the segment. Both are counted.

use crate::speech_stt::protocol::{encode_audio_chunks, encode_end, encode_start};
use crate::speech_vad::{ClockError, PreRoll, RawEdge};
use aivtuber_domain::SpeechEdge;

/// Where encoded frames go. `offer` never blocks.
pub trait FrameSink {
    /// The worker has loaded its model and is alive. Segments are skipped
    /// (not queued) until then, so model load time cannot back up audio.
    fn ready(&self) -> bool;
    /// Queue one encoded frame. `false`: the queue is full or the worker is
    /// gone; the frame was not queued.
    fn offer(&mut self, frame: Vec<u8>) -> bool;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FeederConfig {
    /// Audio recovered before the first start edge (at least 300 ms by
    /// default: EXP-0001 onset lag p90 was 112-144 ms).
    pub preroll_samples: u64,
    /// Silence after an end edge before the segment closes.
    pub tail_samples: u64,
    /// Shortest speech span worth transcribing.
    pub min_speech_samples: u64,
    /// Split point; keep below the worker's hard cap.
    pub max_segment_samples: u64,
}

impl FeederConfig {
    pub fn from_ms(preroll_ms: u64, tail_ms: u64, min_speech_ms: u64, max_segment_ms: u64) -> Self {
        let samples = |ms: u64| ms * 16;
        Self {
            preroll_samples: samples(preroll_ms),
            tail_samples: samples(tail_ms),
            min_speech_samples: samples(min_speech_ms),
            max_segment_samples: samples(max_segment_ms),
        }
    }

    /// Ring capacity (in ms) the pipeline's [`PreRoll`] needs so that a
    /// segment opened at `min_speech` can still reach back `preroll`.
    pub fn ring_ms(&self) -> u64 {
        (self.preroll_samples + self.min_speech_samples) / 16 + 300
    }

    pub fn validate(&self) -> Result<(), String> {
        let ms = |s: u64| s / 16;
        if ms(self.preroll_samples) < 300 {
            return Err("stt preroll must be at least 300 ms".to_owned());
        }
        if self.ring_ms() > crate::speech_vad::MAX_PREROLL_MS {
            return Err(
                "stt preroll plus min speech must leave room in the 2000 ms ring".to_owned(),
            );
        }
        if !(100..=3_000).contains(&ms(self.tail_samples)) {
            return Err("stt tail must be 100..=3000 ms".to_owned());
        }
        if ms(self.min_speech_samples) > 2_000 {
            return Err("stt min speech must be at most 2000 ms".to_owned());
        }
        if !(2_000..=29_000).contains(&ms(self.max_segment_samples)) {
            return Err("stt max segment must be 2000..=29000 ms".to_owned());
        }
        Ok(())
    }
}

impl Default for FeederConfig {
    fn default() -> Self {
        // 500 ms tail: below the 800 ms silence the floor fallback waits for
        // an irreversible turn-take, above a normal breath. A measured
        // synthetic default; to be re-measured on real conversation.
        Self::from_ms(300, 500, 250, 25_000)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FeederStats {
    pub segments_opened: u64,
    pub segments_closed: u64,
    /// Speech that never reached the minimum span.
    pub segments_dropped_short: u64,
    /// START not sent: worker not ready, queue full, or END still pending.
    pub segments_skipped_busy: u64,
    /// A frame was refused mid-segment; the segment closes as skipped.
    pub segments_damaged: u64,
    pub segments_split_at_cap: u64,
    pub clock_errors: u64,
    /// The ring no longer held the whole pre-roll window.
    pub preroll_short: u64,
}

#[derive(Debug)]
struct Active {
    first_start: u64,
    /// Set once the START frame went out.
    opened: Option<Opened>,
    /// Position of the end edge that began the current tail.
    closing_since: Option<u64>,
}

#[derive(Debug)]
struct Opened {
    id: u64,
    first_sample: u64,
    sent_end: u64,
    damaged: bool,
}

pub struct SegmentFeeder<S: FrameSink> {
    config: FeederConfig,
    sink: S,
    active: Option<Active>,
    next_segment_id: u64,
    pending_end: Option<Vec<u8>>,
    stats: FeederStats,
}

impl<S: FrameSink> SegmentFeeder<S> {
    pub fn new(config: FeederConfig, sink: S) -> Result<Self, String> {
        config.validate()?;
        Ok(Self {
            config,
            sink,
            active: None,
            next_segment_id: 0,
            pending_end: None,
            stats: FeederStats::default(),
        })
    }

    pub fn stats(&self) -> FeederStats {
        self.stats
    }

    pub fn sink(&self) -> &S {
        &self.sink
    }

    pub fn sink_mut(&mut self) -> &mut S {
        &mut self.sink
    }

    pub fn into_sink(self) -> S {
        self.sink
    }

    pub fn is_segment_open(&self) -> bool {
        self.active.as_ref().is_some_and(|a| a.opened.is_some())
    }

    /// Call after every `VadPipeline::push_input` with the edges it produced
    /// and the pipeline's ring. `map` converts a stream position to runtime
    /// milliseconds.
    pub fn on_step(
        &mut self,
        edges: &[RawEdge],
        ring: &PreRoll,
        map: impl Fn(u64) -> Result<u64, ClockError>,
    ) {
        self.retry_pending_end();
        for edge in edges {
            match edge.edge {
                SpeechEdge::Start => match &mut self.active {
                    Some(active) => active.closing_since = None,
                    None => {
                        self.active = Some(Active {
                            first_start: edge.sample16,
                            opened: None,
                            closing_since: None,
                        });
                    }
                },
                SpeechEdge::End => {
                    if let Some(active) = &mut self.active
                        && active.closing_since.is_none()
                    {
                        active.closing_since = Some(edge.sample16);
                    }
                }
            }
        }
        let end = ring.end_position();
        self.maybe_open(ring, &map);
        self.top_up(ring);
        self.maybe_close(end);
        self.maybe_split(ring, &map);
    }

    /// The capture dropped audio and the pipeline restarted its stream
    /// (positions restart at zero): close whatever is open now.
    pub fn on_gap(&mut self) {
        self.close_now();
    }

    /// End of input: close whatever is open now, without waiting for a tail.
    pub fn on_finish(&mut self) {
        self.close_now();
    }

    fn close_now(&mut self) {
        self.retry_pending_end();
        if let Some(active) = self.active.take()
            && let Some(opened) = active.opened
        {
            self.send_end(&opened);
        }
    }

    fn retry_pending_end(&mut self) {
        if let Some(frame) = self.pending_end.take()
            && !self.sink.offer(frame.clone())
        {
            self.pending_end = Some(frame);
        }
    }

    fn speech_span(&self, active: &Active, end: u64) -> u64 {
        active
            .closing_since
            .unwrap_or(end)
            .saturating_sub(active.first_start)
    }

    fn maybe_open(&mut self, ring: &PreRoll, map: &impl Fn(u64) -> Result<u64, ClockError>) {
        let end = ring.end_position();
        let Some(active) = &self.active else { return };
        if active.opened.is_some() || self.speech_span(active, end) < self.config.min_speech_samples
        {
            return;
        }
        let wanted = active
            .first_start
            .saturating_sub(self.config.preroll_samples);
        let (from, samples) = ring.since(wanted);
        if from > wanted {
            self.stats.preroll_short += 1;
        }
        if self.pending_end.is_some() || !self.sink.ready() {
            // Not skipping forever: the segment is retried on later steps
            // only while speech continues, so a long utterance that began
            // before the worker was ready starts as soon as it is.
            return;
        }
        let Ok(start_ms) = map(from) else {
            self.stats.clock_errors += 1;
            return;
        };
        let id = self.next_segment_id;
        if !self.sink.offer(encode_start(id, start_ms)) {
            self.stats.segments_skipped_busy += 1;
            self.active = None;
            return;
        }
        self.next_segment_id += 1;
        self.stats.segments_opened += 1;
        let mut opened = Opened {
            id,
            first_sample: from,
            sent_end: end,
            damaged: false,
        };
        self.send_audio(&mut opened, samples);
        if let Some(active) = &mut self.active {
            active.opened = Some(opened);
        }
    }

    /// Send audio the open segment has not received yet.
    fn top_up(&mut self, ring: &PreRoll) {
        let Some(mut active) = self.active.take() else {
            return;
        };
        if let Some(opened) = &mut active.opened {
            let end = ring.end_position();
            if opened.sent_end < end {
                let (_, samples) = ring.since(opened.sent_end);
                opened.sent_end = end;
                self.send_audio(opened, samples);
            }
        }
        self.active = Some(active);
    }

    fn send_audio(&mut self, opened: &mut Opened, samples: Vec<f32>) {
        if opened.damaged || samples.is_empty() {
            return;
        }
        for frame in encode_audio_chunks(&samples) {
            if !self.sink.offer(frame) {
                opened.damaged = true;
                self.stats.segments_damaged += 1;
                return;
            }
        }
    }

    fn maybe_close(&mut self, end: u64) {
        let Some(active) = &self.active else { return };
        let Some(since) = active.closing_since else {
            return;
        };
        if end < since + self.config.tail_samples {
            return;
        }
        let span = self.speech_span(active, end);
        let active = self.active.take().expect("checked above");
        match active.opened {
            Some(opened) => self.send_end(&opened),
            // Long enough, but the worker was never ready or reachable.
            None if span >= self.config.min_speech_samples => {
                self.stats.segments_skipped_busy += 1;
            }
            None => self.stats.segments_dropped_short += 1,
        }
    }

    fn maybe_split(&mut self, ring: &PreRoll, map: &impl Fn(u64) -> Result<u64, ClockError>) {
        let end = ring.end_position();
        let Some(active) = &self.active else { return };
        let Some(opened) = &active.opened else { return };
        if end.saturating_sub(opened.first_sample) < self.config.max_segment_samples {
            return;
        }
        self.stats.segments_split_at_cap += 1;
        let speaking = active.closing_since.is_none();
        let active = self.active.take().expect("checked above");
        if let Some(opened) = active.opened {
            self.send_end(&opened);
        }
        if speaking {
            // Continue the utterance in a new segment with no pre-roll
            // overlap: the cut is mid-speech, nothing was clipped.
            self.active = Some(Active {
                first_start: end.saturating_sub(self.config.min_speech_samples),
                opened: None,
                closing_since: None,
            });
            self.maybe_open(ring, map);
        }
    }

    fn send_end(&mut self, opened: &Opened) {
        self.stats.segments_closed += 1;
        let frame = encode_end(opened.id, opened.damaged);
        if !self.sink.offer(frame.clone()) {
            self.pending_end = Some(frame);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::speech_stt::protocol::{Frame, FrameReader};

    struct Recorder {
        ready: bool,
        /// Offers beyond this many are refused.
        accept: usize,
        frames: Vec<u8>,
        offers: usize,
    }

    impl FrameSink for Recorder {
        fn ready(&self) -> bool {
            self.ready
        }
        fn offer(&mut self, frame: Vec<u8>) -> bool {
            if self.offers >= self.accept {
                return false;
            }
            self.offers += 1;
            self.frames.extend(frame);
            true
        }
    }

    fn recorder() -> Recorder {
        Recorder {
            ready: true,
            accept: usize::MAX,
            frames: Vec::new(),
            offers: 0,
        }
    }

    fn parse(rec: &Recorder) -> Vec<Frame> {
        let mut reader = FrameReader::new(rec.frames.as_slice());
        let mut out = Vec::new();
        while let Some(f) = reader.next_frame().unwrap() {
            out.push(f);
        }
        out
    }

    fn edge(edge: SpeechEdge, at: u64) -> RawEdge {
        RawEdge {
            edge,
            sample16: at,
            confidence: None,
        }
    }

    /// Feeds `total` samples in 160-sample steps, delivering each edge once a
    /// frame (512 samples) past its position, like the VAD. Ring holds 2 s.
    fn drive(
        config: FeederConfig,
        total: u64,
        edges: &[(SpeechEdge, u64)],
    ) -> (FeederStats, Recorder) {
        let mut feeder = SegmentFeeder::new(config, recorder()).unwrap();
        let mut ring = PreRoll::new(2_000).unwrap();
        let mut next = 0;
        let mut pos = 0;
        while pos < total {
            ring.push(&[0.1; 160]);
            pos += 160;
            let mut due = Vec::new();
            while next < edges.len() && edges[next].1 + 512 <= pos {
                due.push(edge(edges[next].0, edges[next].1));
                next += 1;
            }
            feeder.on_step(&due, &ring, |p| Ok(1_000 + p / 16));
        }
        feeder.on_finish();
        (feeder.stats(), feeder.into_sink())
    }

    fn cfg() -> FeederConfig {
        FeederConfig::from_ms(300, 500, 250, 25_000)
    }

    #[test]
    fn a_segment_starts_with_preroll_and_closes_after_the_tail() {
        let (stats, rec) = drive(
            cfg(),
            16_000 * 6,
            &[(SpeechEdge::Start, 16_000), (SpeechEdge::End, 48_000)],
        );
        let frames = parse(&rec);
        let Frame::Start {
            segment_id,
            start_ms,
        } = frames[0]
        else {
            panic!("{:?}", frames[0]);
        };
        assert_eq!(segment_id, 0);
        // 300 ms of pre-roll before the start edge at 1.0 s: position 11_200
        // maps to 1_000 + 700 ms.
        assert_eq!(start_ms, 1_000 + 11_200 / 16);
        let audio: usize = frames
            .iter()
            .map(|f| if let Frame::Audio(s) = f { s.len() } else { 0 })
            .sum();
        // From 11_200 through the end edge (48_000) plus the 500 ms tail
        // (8_000), delivered in 160-sample steps.
        assert!(
            (36_800 + 8_000..=36_800 + 8_000 + 1_000).contains(&audio),
            "{audio}"
        );
        assert_eq!(
            frames.last(),
            Some(&Frame::End {
                segment_id: 0,
                skip_decode: false
            })
        );
        assert_eq!((stats.segments_opened, stats.segments_closed), (1, 1));
    }

    #[test]
    fn a_blip_shorter_than_the_minimum_never_reaches_the_worker() {
        let (stats, rec) = drive(
            cfg(),
            16_000 * 3,
            &[(SpeechEdge::Start, 16_000), (SpeechEdge::End, 16_512)],
        );
        assert!(rec.frames.is_empty());
        assert_eq!(stats.segments_dropped_short, 1);
        assert_eq!(stats.segments_opened, 0);
    }

    #[test]
    fn a_pause_shorter_than_the_tail_stays_one_segment() {
        let (stats, rec) = drive(
            cfg(),
            16_000 * 7,
            &[
                (SpeechEdge::Start, 16_000),
                (SpeechEdge::End, 32_000),
                // Resumes 250 ms later: inside the 500 ms tail.
                (SpeechEdge::Start, 36_000),
                (SpeechEdge::End, 56_000),
            ],
        );
        let frames = parse(&rec);
        let starts = frames
            .iter()
            .filter(|f| matches!(f, Frame::Start { .. }))
            .count();
        let ends = frames
            .iter()
            .filter(|f| matches!(f, Frame::End { .. }))
            .count();
        assert_eq!((starts, ends), (1, 1));
        assert_eq!(stats.segments_opened, 1);
    }

    #[test]
    fn a_pause_longer_than_the_tail_makes_two_segments_with_increasing_ids() {
        let (_, rec) = drive(
            cfg(),
            16_000 * 8,
            &[
                (SpeechEdge::Start, 16_000),
                (SpeechEdge::End, 32_000),
                (SpeechEdge::Start, 56_000),
                (SpeechEdge::End, 72_000),
            ],
        );
        let ids: Vec<u64> = parse(&rec)
            .iter()
            .filter_map(|f| match f {
                Frame::Start { segment_id, .. } => Some(*segment_id),
                _ => None,
            })
            .collect();
        assert_eq!(ids, [0, 1]);
    }

    #[test]
    fn an_unready_worker_defers_the_segment_and_a_long_utterance_starts_when_ready() {
        let mut rec = recorder();
        rec.ready = false;
        let mut feeder = SegmentFeeder::new(cfg(), rec).unwrap();
        let mut ring = PreRoll::new(2_000).unwrap();
        ring.push(&[0.1; 16_000]);
        feeder.on_step(&[edge(SpeechEdge::Start, 100)], &ring, |p| Ok(p / 16));
        assert!(!feeder.is_segment_open());
        feeder.sink_mut().ready = true;
        ring.push(&[0.1; 160]);
        feeder.on_step(&[], &ring, |p| Ok(p / 16));
        assert!(feeder.is_segment_open());
        assert_eq!(feeder.stats().segments_opened, 1);
    }

    #[test]
    fn a_segment_that_ends_before_the_worker_is_ready_is_skipped_and_counted() {
        let mut rec = recorder();
        rec.ready = false;
        let mut feeder = SegmentFeeder::new(cfg(), rec).unwrap();
        let mut ring = PreRoll::new(2_000).unwrap();
        ring.push(&[0.1; 40_000]);
        feeder.on_step(
            &[edge(SpeechEdge::Start, 100), edge(SpeechEdge::End, 20_000)],
            &ring,
            |p| Ok(p / 16),
        );
        let stats = feeder.stats();
        assert_eq!((stats.segments_skipped_busy, stats.segments_opened), (1, 0));
    }

    #[test]
    fn a_refused_audio_frame_damages_the_segment_and_its_end_says_skip() {
        let mut rec = recorder();
        // START and the first audio frame pass; the next audio frame is refused.
        rec.accept = 2;
        let mut feeder = SegmentFeeder::new(cfg(), rec).unwrap();
        let mut ring = PreRoll::new(2_000).unwrap();
        ring.push(&[0.1; 12_000]);
        feeder.on_step(&[edge(SpeechEdge::Start, 4_000)], &ring, |p| Ok(p / 16));
        ring.push(&[0.1; 160]);
        feeder.on_step(&[], &ring, |p| Ok(p / 16));
        assert_eq!(feeder.stats().segments_damaged, 1);
        // Closing while the sink still refuses parks the END; it is retried.
        feeder.on_finish();
        feeder.sink_mut().accept = usize::MAX;
        feeder.on_step(&[], &ring, |p| Ok(p / 16));
        let frames = parse(feeder.sink());
        assert!(
            matches!(
                frames.last(),
                Some(Frame::End {
                    skip_decode: true,
                    ..
                })
            ),
            "{frames:?}"
        );
        // No audio was sent after the refusal.
        let audio = frames
            .iter()
            .filter(|f| matches!(f, Frame::Audio(_)))
            .count();
        assert_eq!(audio, 1);
    }

    #[test]
    fn a_refused_start_skips_the_segment() {
        let mut rec = recorder();
        rec.accept = 0;
        let mut feeder = SegmentFeeder::new(cfg(), rec).unwrap();
        let mut ring = PreRoll::new(2_000).unwrap();
        ring.push(&[0.1; 12_000]);
        feeder.on_step(&[edge(SpeechEdge::Start, 4_000)], &ring, |p| Ok(p / 16));
        assert_eq!(feeder.stats().segments_skipped_busy, 1);
        assert!(!feeder.is_segment_open());
    }

    #[test]
    fn a_long_utterance_is_split_below_the_workers_cap() {
        let config = FeederConfig::from_ms(300, 500, 250, 3_000);
        let (_, rec) = drive(config, 16_000 * 8, &[(SpeechEdge::Start, 1_600)]);
        let frames = parse(&rec);
        let starts = frames
            .iter()
            .filter(|f| matches!(f, Frame::Start { .. }))
            .count();
        assert!(starts >= 2, "split into {starts} segments");
        let mut audio = 0;
        for f in &frames {
            match f {
                Frame::Audio(s) => audio += s.len(),
                Frame::End { .. } => {
                    assert!(audio <= 3_000 * 16 + 1_000, "{audio}");
                    audio = 0;
                }
                Frame::Start { .. } => audio = 0,
            }
        }
    }

    #[test]
    fn a_gap_or_finish_closes_an_open_segment_without_a_tail() {
        let mut feeder = SegmentFeeder::new(cfg(), recorder()).unwrap();
        let mut ring = PreRoll::new(2_000).unwrap();
        ring.push(&[0.1; 12_000]);
        feeder.on_step(&[edge(SpeechEdge::Start, 4_000)], &ring, |p| Ok(p / 16));
        assert!(feeder.is_segment_open());
        feeder.on_gap();
        assert!(!feeder.is_segment_open());
        assert!(matches!(
            parse(feeder.sink()).last(),
            Some(Frame::End { .. })
        ));
    }

    #[test]
    fn configuration_bounds_are_enforced() {
        assert!(cfg().validate().is_ok());
        assert!(
            FeederConfig::from_ms(100, 500, 250, 25_000)
                .validate()
                .is_err()
        );
        assert!(
            FeederConfig::from_ms(300, 50, 250, 25_000)
                .validate()
                .is_err()
        );
        assert!(
            FeederConfig::from_ms(300, 500, 250, 30_000)
                .validate()
                .is_err()
        );
        assert!(
            FeederConfig::from_ms(1_900, 500, 250, 25_000)
                .validate()
                .is_err()
        );
    }
}
