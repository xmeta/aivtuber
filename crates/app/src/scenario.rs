//! Issue #70: versioned, seeded, deterministic stream workload scenarios.
//!
//! A fixed replay fixture is useful for comparing two revisions but it is a
//! single temporal pattern, and real streams are phased and bursty. This module
//! is the shared definition of those patterns so the benchmark, soak, retention
//! and quality suites stop inventing their own unrealistic workloads.
//!
//! Three properties are load-bearing:
//!
//! 1. **Reproducibility.** The trace is a pure function of the scenario identity,
//!    its `scenario_version`, its `seed` and its phases. Nothing reads the wall
//!    clock: `logical_start` is a declared constant, so a trace generated on a
//!    laptop in a year reproduces byte for byte.
//! 2. **A version bump reshuffles the trace.** The version is mixed into the
//!    per-phase seed, so an incompatible scenario change cannot silently reuse
//!    a previous comparison baseline. `scenario_dataset_id` carries it into the
//!    #58 benchmark result, whose gate already refuses cross-`dataset_id`
//!    comparison.
//! 3. **Honest provenance.** `provenance` and `provenance_note` are required,
//!    so a synthetic design assumption cannot be presented as a measured
//!    production fact.
//!
//! `operator.command` is deliberately not synthesizable: a privileged event
//! would require fabricating an authorization claim. Operator and failure
//! overlays belong to the #40 fault slice.

use crate::{AppError, PayloadIntentEmbedding, QueryEmbeddingProvider};
use aivtuber_domain::{
    EVENT_SCHEMA_VERSION, EventEnvelope, EventKind, SecurityPlane, SourceClass, TrustLevel,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;

/// Version of the scenario document contract.
pub const SCENARIO_SCHEMA_VERSION: &str = "1";

/// Embedding width of the starter semantic space (#55), which the generated
/// query embeddings are expressed in.
pub const SCENARIO_EMBEDDING_DIMENSION: usize = 3;

/// Upper bound on events per minute.
///
/// Also the reason timestamps stay strictly increasing: one event per minute
/// keeps an event count no larger than its phase's millisecond duration, so
/// every event can own a distinct millisecond.
pub const MAX_EVENTS_PER_MINUTE: f64 = 60_000.0;

/// Highest event rate the runtime can actually *play*, in events per minute.
///
/// Measured against the starter pack, not guessed. Sweeping the shipped runtime
/// through cached playback:
/// * chat-only completes at 15 events/minute over two minutes, and 20 trips the
///   per-asset cooldown (`Rejection::Cooldown`);
/// * mixing any higher-priority kind - `game.event` or `chat.donation` -
///   completes at 10 and fails at 15 with `Rejection::Priority`, because the
///   scheduler will not preempt a playing item for a higher-ranked one;
/// * `timer.tick` and `stream.event` do not reach the generative replay's event
///   accounting at all, at any rate.
///
/// 10 is therefore the ceiling for a mixed workload, and this slice keeps its
/// scenarios at or below it. A scenario denser than this is refused here rather
/// than discovered when a benchmark job aborts, because "the workload was
/// unrepresentable" is a far worse finding than "the scenario is too dense".
///
/// This is a property of the starter pack and the scheduler, not of the scenario
/// format. Raising it is a runtime change with a measured justification, and
/// this constant is where that gets recorded.
pub const MAX_PLAYABLE_EVENTS_PER_MINUTE: f64 = 10.0;

/// Refuse to materialise an unreasonably large trace. Logical-time scenarios are
/// meant to be cheap to generate; a typo should fail, not exhaust memory.
pub const MAX_TRACE_EVENTS: u64 = 1_000_000;

/// Where a scenario's parameters came from.
///
/// The issue is explicit that synthetic distributions must not be presented as
/// measured production facts, so the distinction is part of the contract rather
/// than a comment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScenarioProvenance {
    /// A design assumption made for this repository.
    SyntheticDesignAssumption,
    /// Derived from approved aggregate observations, never raw transcripts.
    MeasuredAggregate,
    /// Deliberately extreme values for robustness work.
    StressOnly,
}

impl ScenarioProvenance {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SyntheticDesignAssumption => "synthetic_design_assumption",
            Self::MeasuredAggregate => "measured_aggregate",
            Self::StressOnly => "stress_only",
        }
    }
}

/// The workload family a scenario represents.
///
/// Declared rather than inferred so coverage is checkable: the validator refuses
/// a scenario whose parameters contradict its class, which stops a scenario
/// labelled `high_cardinality` from quietly shipping three actors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScenarioClass {
    LowTraffic,
    NormalMixed,
    Burst,
    DonationBurst,
    EventHeavy,
    IdleToBurst,
    HighSemanticReuse,
    HighGenerativeMiss,
    HighCardinality,
    // `asset_churn` and `repeated_cancellation` are deliberately absent until
    // the #40 fault overlays can express them; a class label with nothing to
    // validate it against would be decoration.
}

impl ScenarioClass {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::LowTraffic => "low_traffic",
            Self::NormalMixed => "normal_mixed",
            Self::Burst => "burst",
            Self::DonationBurst => "donation_burst",
            Self::EventHeavy => "event_heavy",
            Self::IdleToBurst => "idle_to_burst",
            Self::HighSemanticReuse => "high_semantic_reuse",
            Self::HighGenerativeMiss => "high_generative_miss",
            Self::HighCardinality => "high_cardinality",
        }
    }
}

/// How close a generated event sits to the curated semantic index.
///
/// The distribution matters to #69 and #79: a workload of pure hits measures
/// the cache path, and a workload of pure misses measures the generative path,
/// and neither number means anything for a real mix without the other two.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SemanticBand {
    Hit,
    NearMiss,
    Miss,
}

impl SemanticBand {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Hit => "hit",
            Self::NearMiss => "near_miss",
            Self::Miss => "miss",
        }
    }

    /// Variant groups the starter pack actually contains.
    ///
    /// `payload.intent` is used *directly* as a variant group by cached playback
    /// (`CachedPlayer::select_variant_with_tier`), so an invented intent such as
    /// `reaction.agree.qualified` fails the whole run with "variant_group:...
    /// was not found" rather than degrading. A scenario therefore cannot name
    /// an asset that does not exist; it can only choose among real ones.
    const fn axis_intents() -> &'static [&'static str] {
        &["reaction.agree", "reaction.surprise", "filler.thinking"]
    }

    /// Signal retained by this band, as a share of the axis embedding.
    ///
    /// The band is expressed through the *embedding*, not through the intent,
    /// because the intent has to stay a real variant group. A hit is the axis
    /// itself, a near-miss keeps most of the signal but is no longer a clean
    /// match, and a miss carries none of it.
    ///
    /// Limitation worth stating: whether a near-miss actually flips reuse
    /// depends on the retriever's ranking, so a 65%-signal near-miss is a weaker
    /// match rather than a guaranteed wrong one. A retriever with graded
    /// distance thresholds would let the bands be separated exactly, and this is
    /// where that would change.
    const fn signal(self) -> f64 {
        match self {
            Self::Hit => 1.0,
            Self::NearMiss => 0.65,
            Self::Miss => 0.0,
        }
    }

    /// Project an axis embedding onto this band's strength.
    fn project(self, axis: &[f32]) -> Vec<f32> {
        let dimension = axis.len();
        if dimension == 0 {
            return Vec::new();
        }
        let signal = self.signal();
        let spread = (1.0 - signal) / dimension as f64;
        axis.iter()
            .map(|value| (*value as f64) * signal + spread)
            .map(|value| value as f32)
            .collect()
    }
}

/// Priority buckets, so a scenario states intent rather than raw floats.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PriorityBucket {
    Low,
    Normal,
    High,
    Critical,
}

impl PriorityBucket {
    pub const fn priority_hint(self) -> f64 {
        match self {
            Self::Low => 0.2,
            Self::Normal => 0.5,
            Self::High => 0.8,
            Self::Critical => 1.0,
        }
    }
}

/// One logical-time phase of a stream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScenarioPhase {
    /// Phase name, unique within the scenario, for readable assertions.
    pub name: String,
    pub duration_ms: u64,
    /// Mean events per minute across this phase. `0` is a genuine idle phase.
    pub events_per_minute: f64,
    /// Weight per synthesizable event kind.
    pub kind_mix: BTreeMap<EventKind, f64>,
    /// Weight per semantic band.
    pub semantic_mix: BTreeMap<SemanticBand, f64>,
    /// Weight per priority bucket.
    pub priority_mix: BTreeMap<PriorityBucket, f64>,
    /// Size of the viewer pool. Repeated-viewer behaviour is drawn from it.
    pub distinct_actors: u32,
    /// Size of the topic pool, which drives semantic reuse.
    pub distinct_topics: u32,
    /// Chance an event reuses the previous event's viewer, modelling a viewer
    /// who keeps talking instead of a fresh arrival each time.
    pub repeat_viewer_probability: f64,
}

impl ScenarioPhase {
    /// Events this phase contributes.
    ///
    /// Rounded so a rate is a mean rather than a promise, and bounded by
    /// [`MAX_EVENTS_PER_MINUTE`] so the count never exceeds the phase duration
    /// in milliseconds, which is what keeps timestamps strictly increasing.
    pub fn event_count(&self) -> Result<u64, AppError> {
        let raw = self.events_per_minute * self.duration_ms as f64 / 60_000.0;
        let count = raw.round();
        if count > self.duration_ms as f64 {
            return Err(AppError::Routing(format!(
                "scenario phase {:?} would place {count} events in {} ms; at most one event per millisecond is representable",
                self.name, self.duration_ms
            )));
        }
        Ok(count as u64)
    }

    fn validate(&self, index: usize) -> Result<(), AppError> {
        let label = format!("phase {index} ({})", self.name);
        if self.name.trim().is_empty() {
            return Err(AppError::Routing(format!(
                "scenario {label} must have a non-empty name"
            )));
        }
        if self.duration_ms == 0 {
            return Err(AppError::Routing(format!(
                "scenario {label} must have a positive duration_ms"
            )));
        }
        if !self.events_per_minute.is_finite()
            || self.events_per_minute < 0.0
            || self.events_per_minute > MAX_EVENTS_PER_MINUTE
        {
            return Err(AppError::Routing(format!(
                "scenario {label} events_per_minute must be finite and within 0..={MAX_EVENTS_PER_MINUTE}, got {}",
                self.events_per_minute
            )));
        }
        if self.events_per_minute > MAX_PLAYABLE_EVENTS_PER_MINUTE {
            return Err(AppError::Routing(format!(
                "scenario {label} runs at {} events/minute, above the {MAX_PLAYABLE_EVENTS_PER_MINUTE} events/minute the starter pack can play; cached playback rejects the overlap instead of queueing it, so a denser workload aborts the run rather than producing a measurement. Raise the runtime limit with evidence, or model the burst as a ramp",
                self.events_per_minute
            )));
        }
        if !(0.0..=1.0).contains(&self.repeat_viewer_probability) {
            return Err(AppError::Routing(format!(
                "scenario {label} repeat_viewer_probability must be within 0..=1, got {}",
                self.repeat_viewer_probability
            )));
        }
        if self.distinct_actors == 0 {
            return Err(AppError::Routing(format!(
                "scenario {label} must have at least one distinct actor"
            )));
        }
        if self.distinct_topics == 0 {
            return Err(AppError::Routing(format!(
                "scenario {label} must have at least one distinct topic"
            )));
        }
        validate_weights(&format!("scenario {label} kind_mix"), &self.kind_mix)?;
        validate_weights(
            &format!("scenario {label} semantic_mix"),
            &self.semantic_mix,
        )?;
        validate_weights(
            &format!("scenario {label} priority_mix"),
            &self.priority_mix,
        )?;
        if self
            .kind_mix
            .get(&EventKind::OperatorCommand)
            .is_some_and(|weight| *weight > 0.0)
        {
            return Err(AppError::Routing(format!(
                "scenario {label} weights operator.command, but a synthetic privileged event would have to fabricate its authorization; operator and failure overlays belong to the #40 fault slice"
            )));
        }
        self.event_count()?;
        Ok(())
    }
}

fn validate_weights<T: Ord + std::fmt::Debug>(
    label: &str,
    weights: &BTreeMap<T, f64>,
) -> Result<(), AppError> {
    if weights.is_empty() {
        return Err(AppError::Routing(format!("{label} must not be empty")));
    }
    let mut total = 0.0;
    for (key, weight) in weights {
        if !weight.is_finite() || *weight <= 0.0 {
            return Err(AppError::Routing(format!(
                "{label} weight for {key:?} must be finite and positive, got {weight}"
            )));
        }
        total += weight;
    }
    if !total.is_finite() || total <= 0.0 {
        return Err(AppError::Routing(format!(
            "{label} weights must sum above zero"
        )));
    }
    Ok(())
}

/// A versioned stream workload definition.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StreamScenario {
    pub schema_version: String,
    pub scenario_id: String,
    /// Bumping this starts a new comparison baseline; see the module docs.
    pub scenario_version: u32,
    pub scenario_class: ScenarioClass,
    pub provenance: ScenarioProvenance,
    /// Required so a scenario always states what its parameters are.
    pub provenance_note: String,
    pub seed: u64,
    /// Logical start instant. A constant, never the wall clock.
    pub logical_start: String,
    pub stream_duration_ms: u64,
    pub phases: Vec<ScenarioPhase>,
}

impl StreamScenario {
    /// Dataset identity for the #58 benchmark result.
    ///
    /// Carries the scenario version so `benchmark_gate` refuses to compare a
    /// new scenario against an old baseline without any extra machinery.
    pub fn dataset_id(&self) -> String {
        format!("scenario-{}-v{}", self.scenario_id, self.scenario_version)
    }

    pub fn phase_count(&self) -> usize {
        self.phases.len()
    }

    /// Total events the scenario will generate, without generating them.
    pub fn total_event_count(&self) -> Result<u64, AppError> {
        let mut total = 0u64;
        for phase in &self.phases {
            total = total
                .checked_add(phase.event_count()?)
                .ok_or_else(|| AppError::Routing("scenario event count overflowed".to_owned()))?;
        }
        Ok(total)
    }

    pub fn validate(&self) -> Result<(), AppError> {
        if self.schema_version != SCENARIO_SCHEMA_VERSION {
            return Err(AppError::Routing(format!(
                "unsupported scenario schema_version {:?}; this build reads {SCENARIO_SCHEMA_VERSION:?}. An incompatible scenario version starts a new comparison baseline, it does not silently reinterpret",
                self.schema_version
            )));
        }
        if !is_slug(&self.scenario_id) {
            return Err(AppError::Routing(format!(
                "scenario_id {:?} must be a lowercase slug",
                self.scenario_id
            )));
        }
        if self.scenario_version == 0 {
            return Err(AppError::Routing(
                "scenario_version must start at 1".to_owned(),
            ));
        }
        if self.provenance_note.trim().is_empty() {
            return Err(AppError::Routing(format!(
                "scenario {:?} must state provenance_note: whether its parameters are a synthetic design assumption, derived from approved aggregates, or stress-only",
                self.scenario_id
            )));
        }
        if self.stream_duration_ms == 0 {
            return Err(AppError::Routing(
                "stream_duration_ms must be positive".to_owned(),
            ));
        }
        if self.phases.is_empty() {
            return Err(AppError::Routing(format!(
                "scenario {:?} must declare at least one phase",
                self.scenario_id
            )));
        }

        let mut seen_names = BTreeMap::new();
        let mut duration_total = 0u64;
        for (index, phase) in self.phases.iter().enumerate() {
            phase.validate(index)?;
            if seen_names.insert(phase.name.as_str(), index).is_some() {
                return Err(AppError::Routing(format!(
                    "scenario {:?} reuses phase name {:?}",
                    self.scenario_id, phase.name
                )));
            }
            duration_total = duration_total
                .checked_add(phase.duration_ms)
                .ok_or_else(|| AppError::Routing("phase durations overflowed".to_owned()))?;
        }
        if duration_total != self.stream_duration_ms {
            return Err(AppError::Routing(format!(
                "scenario {:?} phase durations sum to {duration_total} ms but stream_duration_ms is {} ms; an unrepresented or overlapping tail would make the trace unaccountable",
                self.scenario_id, self.stream_duration_ms
            )));
        }

        let total = self.total_event_count()?;
        if total == 0 {
            return Err(AppError::Routing(format!(
                "scenario {:?} generates no events, which would benchmark nothing",
                self.scenario_id
            )));
        }
        if total > MAX_TRACE_EVENTS {
            return Err(AppError::Routing(format!(
                "scenario {:?} would generate {total} events, above the {MAX_TRACE_EVENTS} ceiling",
                self.scenario_id
            )));
        }
        parse_logical_start(&self.logical_start).map(|_| ())?;
        Ok(())
    }
}

fn is_slug(value: &str) -> bool {
    let bytes = value.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 64
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-')
        && !bytes[0].is_ascii_digit()
        && !bytes[0..1].eq(b"-")
        && !bytes[bytes.len() - 1..].eq(b"-")
}

/// One generated event, plus the query embedding the benchmark feeds it.
///
/// The embedding comes from the same provider production uses, so a scenario
/// benchmark and a live run resolve the same retrieval axis.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScenarioEvent {
    pub event: EventEnvelope,
    pub query_embedding: Vec<f32>,
}

/// Deterministic counter-based PRNG (SplitMix64).
///
/// Written out rather than pulled in so the generated trace depends only on this
/// repository's source: a dependency upgrade must not be able to reshuffle a
/// recorded workload under an unchanged scenario.
#[derive(Debug, Clone)]
struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in `[0, bound)`. `bound == 0` yields 0.
    fn below(&mut self, bound: u64) -> u64 {
        if bound == 0 {
            return 0;
        }
        // Lemire's multiply-shift rejection bound: cheap, unbiased enough for
        // workload synthesis, and free of floating point.
        let mut candidate = self.next_u64();
        let threshold = bound.wrapping_neg() % bound;
        while candidate < threshold {
            candidate = self.next_u64();
        }
        candidate % bound
    }

    /// Uniform in `[0, 1)`.
    fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01B3);
    }
    hash
}

/// Per-phase seed.
///
/// Includes the scenario version, which is what makes an incompatible scenario
/// change produce a different trace and therefore a new baseline.
fn phase_seed(scenario: &StreamScenario, phase_index: usize) -> u64 {
    let mut mixed = scenario.seed ^ fnv1a64(scenario.scenario_id.as_bytes());
    mixed ^= u64::from(scenario.scenario_version).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    mixed ^= (phase_index as u64 + 1).wrapping_mul(0xD1B5_4A32_D192_ED03);
    SplitMix64::new(mixed).next_u64()
}

fn pick_weighted<T: Ord + Copy>(rng: &mut SplitMix64, weights: &BTreeMap<T, f64>) -> T {
    let total: f64 = weights.values().sum();
    let mut threshold = rng.unit() * total;
    // BTreeMap iteration is sorted, so the walk is order-independent of how the
    // document happened to be written.
    let mut last = None;
    for (key, weight) in weights {
        threshold -= weight;
        last = Some(*key);
        if threshold <= 0.0 {
            return *key;
        }
    }
    last.expect("validate rejects an empty mix")
}

/// Generate the ordered event trace for a scenario.
///
/// The result is ordered by `observed_at` with contiguous `sequence` values
/// starting at 1, which is the property every consumer of a scenario relies on.
pub fn generate_scenario_trace(scenario: &StreamScenario) -> Result<Vec<ScenarioEvent>, AppError> {
    scenario.validate()?;
    let start_ms = parse_logical_start(&scenario.logical_start)?;
    let mut provider = PayloadIntentEmbedding::new(SCENARIO_EMBEDDING_DIMENSION);

    let mut trace: Vec<ScenarioEvent> = Vec::new();
    let mut sequence = 0u64;
    let mut phase_start_ms = 0u64;

    for (index, phase) in scenario.phases.iter().enumerate() {
        let count = phase.event_count()?;
        let mut rng = SplitMix64::new(phase_seed(scenario, index));
        let mut previous_actor: Option<u32> = None;

        // Integer base offsets, at least one millisecond apart because the rate
        // ceiling keeps `count <= duration_ms`.
        let bases: Vec<u64> = (0..count)
            .map(|ordinal| (ordinal as u128 * phase.duration_ms as u128 / count as u128) as u64)
            .collect();

        for (ordinal, base) in bases.iter().enumerate() {
            let kind = pick_weighted(&mut rng, &phase.kind_mix);
            let band = pick_weighted(&mut rng, &phase.semantic_mix);
            let priority = pick_weighted(&mut rng, &phase.priority_mix);

            // Jitter stays inside this event's own slot so it can never collide
            // with the next event's base.
            let slot = bases
                .get(ordinal + 1)
                .map(|next| next.saturating_sub(*base))
                .unwrap_or(phase.duration_ms.saturating_sub(*base))
                .max(1);
            let offset_ms = base + rng.below(slot);
            sequence += 1;

            let actor_index = match previous_actor {
                Some(previous) if rng.unit() < phase.repeat_viewer_probability => previous,
                _ => {
                    let drawn = rng.below(u64::from(phase.distinct_actors));
                    previous_actor = Some(drawn as u32);
                    drawn as u32
                }
            };
            let topic_index = rng.below(u64::from(phase.distinct_topics));
            let intents = SemanticBand::axis_intents();
            let intent = intents[rng.below(intents.len() as u64) as usize];

            let mut payload = BTreeMap::new();
            // Synthetic labels only. No scenario may carry viewer prose: a
            // checked-in workload must never require a real transcript.
            payload.insert(
                "intent".to_owned(),
                serde_json::Value::String(intent.to_owned()),
            );
            payload.insert(
                "semantic_band".to_owned(),
                serde_json::Value::String(band.as_str().to_owned()),
            );
            payload.insert(
                "topic_id".to_owned(),
                serde_json::Value::String(format!("topic-{topic_index:04}")),
            );
            payload.insert(
                "scenario_phase".to_owned(),
                serde_json::Value::String(phase.name.clone()),
            );

            let event = EventEnvelope {
                schema_version: EVENT_SCHEMA_VERSION.to_owned(),
                event_id: format!("{}:{sequence:06}", scenario.scenario_id),
                correlation_id: format!("{}-stream", scenario.scenario_id),
                sequence,
                observed_at: format_rfc3339(start_ms + phase_start_ms + offset_ms),
                source: "scenario".to_owned(),
                source_class: source_class_for(kind),
                plane: plane_for(kind),
                trust_level: trust_for(kind),
                kind,
                actor_id: Some(format!("viewer:{actor_index:04}")),
                priority_hint: Some(priority.priority_hint()),
                authorization: None,
                payload,
            };

            let query_embedding = band.project(&provider.embedding(&event)?);
            trace.push(ScenarioEvent {
                event,
                query_embedding,
            });
        }

        phase_start_ms += phase.duration_ms;
    }

    Ok(trace)
}

/// Render the trace in the #58 benchmark dataset shape.
///
/// The generator and the benchmark therefore share one workload definition
/// rather than one definition plus a copy of it.
pub fn scenario_to_benchmark_fixture(
    scenario: &StreamScenario,
) -> Result<serde_json::Value, AppError> {
    let trace = generate_scenario_trace(scenario)?;
    let events: Vec<serde_json::Value> = trace
        .iter()
        .map(|entry| {
            serde_json::json!({
                "event": entry.event,
                "query_embedding": entry.query_embedding,
            })
        })
        .collect();
    Ok(serde_json::json!({
        "dataset_id": scenario.dataset_id(),
        "seed": scenario.seed,
        "stream_duration_ms": scenario.stream_duration_ms,
        "events": events,
    }))
}

fn source_class_for(kind: EventKind) -> SourceClass {
    match kind {
        EventKind::ChatMessage => SourceClass::PublicChat,
        EventKind::ChatDonation => SourceClass::Donation,
        EventKind::SpeechInput => SourceClass::Speech,
        EventKind::GameEvent => SourceClass::Game,
        EventKind::StreamEvent => SourceClass::Stream,
        EventKind::TimerTick => SourceClass::Timer,
        EventKind::OperatorCommand => SourceClass::Operator,
        EventKind::SystemHealth => SourceClass::System,
    }
}

fn plane_for(kind: EventKind) -> SecurityPlane {
    match kind {
        EventKind::StreamEvent | EventKind::TimerTick | EventKind::SystemHealth => {
            SecurityPlane::System
        }
        EventKind::OperatorCommand => SecurityPlane::Control,
        _ => SecurityPlane::Content,
    }
}

fn trust_for(kind: EventKind) -> TrustLevel {
    match kind {
        EventKind::ChatMessage | EventKind::ChatDonation | EventKind::SpeechInput => {
            TrustLevel::Untrusted
        }
        EventKind::GameEvent | EventKind::StreamEvent => TrustLevel::SemiTrusted,
        EventKind::TimerTick | EventKind::SystemHealth => TrustLevel::Trusted,
        EventKind::OperatorCommand => TrustLevel::Trusted,
    }
}

/// Days from 1970-01-01 for a proleptic Gregorian date (Howard Hinnant's
/// algorithm). Used instead of a date dependency so trace generation adds no
/// supply-chain surface for what is arithmetic.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let shifted_month = if month > 2 { month - 3 } else { month + 9 };
    let day_of_year = (153 * shifted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let shifted = days + 719_468;
    let era = if shifted >= 0 {
        shifted
    } else {
        shifted - 146_096
    } / 146_097;
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    let year = if month <= 2 { year + 1 } else { year };
    (year, month, day)
}

/// Strict `YYYY-MM-DDTHH:MM:SSZ` parsing.
///
/// Strict on purpose: a scenario's start is part of its identity, and silently
/// accepting a zone offset or fractional seconds would make two documents with
/// the same declared instant generate different traces.
fn parse_logical_start(value: &str) -> Result<u64, AppError> {
    let invalid = || {
        AppError::Routing(format!(
            "logical_start {value:?} must be UTC as YYYY-MM-DDTHH:MM:SSZ"
        ))
    };
    let bytes = value.as_bytes();
    if bytes.len() != 20 || bytes[4] != b'-' || bytes[7] != b'-' || bytes[10] != b'T' {
        return Err(invalid());
    }
    if bytes[13] != b':' || bytes[16] != b':' || bytes[19] != b'Z' {
        return Err(invalid());
    }
    let number = |range: std::ops::Range<usize>| -> Result<i64, AppError> {
        let slice = value.get(range).ok_or_else(invalid)?;
        if !slice.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(invalid());
        }
        slice.parse::<i64>().map_err(|_| invalid())
    };
    let year = number(0..4)?;
    let month = number(5..7)?;
    let day = number(8..10)?;
    let hour = number(11..13)?;
    let minute = number(14..16)?;
    let second = number(17..19)?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return Err(invalid());
    }
    if hour > 23 || minute > 59 || second > 59 {
        return Err(invalid());
    }
    let days = days_from_civil(year, month, day);
    let seconds = days
        .checked_mul(86_400)
        .and_then(|value| value.checked_add(hour * 3600 + minute * 60 + second))
        .ok_or_else(invalid)?;
    u64::try_from(seconds * 1000).map_err(|_| invalid())
}

/// Format an instant as RFC 3339 with millisecond precision.
///
/// Milliseconds are not decoration: event slots are allocated one millisecond
/// apart, so rounding to whole seconds would give a burst of events the same
/// `observed_at` string and leave a consumer unable to tell them apart or to
/// detect a reordering. Fixed-width fractional digits keep the strings
/// lexicographically ordered as well as chronologically ordered.
fn format_rfc3339(epoch_ms: u64) -> String {
    let seconds = (epoch_ms / 1000) as i64;
    let millis = epoch_ms % 1000;
    let days = seconds.div_euclid(86_400);
    let time_of_day = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{millis:03}Z",
        time_of_day / 3600,
        (time_of_day % 3600) / 60,
        time_of_day % 60
    )
}

impl fmt::Display for ScenarioClass {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}
