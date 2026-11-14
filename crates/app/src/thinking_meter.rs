//! Thinking usage and optional spend cap (#367).
//!
//! [`ThinkingMeter`] is a small bounded accumulator of the provider-reported
//! usage of every Thinking reply in this process: token counts and cost in
//! micro-USD, all `u64` and saturating. [`MeteredThinking`] wraps the real
//! engine so the buffered, phrased and streamed paths are all counted at the
//! one place every call passes.
//!
//! * Missing usage is *unknown*, never zero: a reply without a usage object
//!   only bumps `turns_unknown_usage`, and a total cost exists only once some
//!   reply reported one.
//! * The optional cap (`AIVTUBER_THINKING_COST_CAP_USD`) is the only way cost
//!   influences behaviour, and it is a deterministic rule on the recorded
//!   totals: once the reported session cost reaches the cap, every later
//!   Thinking call is refused with `EngineErrorKind::QuotaExhausted`, which
//!   the pipeline already maps to the deterministic fallback (wire reason
//!   `unavailable`). Turns whose cost is unknown do not count toward the cap,
//!   and a call already in flight when the cap is reached still completes, so
//!   the total can overshoot by what was in flight. The trip never lifts
//!   before restart: totals only grow.
//! * Totals are audit/operator data. They are not in the replay identity.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use aivtuber_domain::{
    BackendIdentity, EngineError, EngineErrorKind, EngineFuture, GeneratedReply, ReplyDeltaSink,
    ThinkingEngine, ThinkingRequest, ThinkingUsage,
};
use serde::{Deserialize, Serialize};

/// Environment variable naming the optional session spend cap, in decimal USD.
pub const THINKING_COST_CAP_VAR: &str = "AIVTUBER_THINKING_COST_CAP_USD";
/// Upper bound accepted for the cap (USD): a typo like `1e9` is a startup
/// error, not a cap that can never trip.
pub const MAX_THINKING_COST_CAP_USD: u64 = 10_000;
const MICRO_PER_USD: u64 = 1_000_000;

/// Parses the cap: a positive decimal number of USD with at most six
/// fractional digits and at most [`MAX_THINKING_COST_CAP_USD`]; returns
/// micro-USD. Plain digits only: no sign, exponent, separators or whitespace
/// inside the number.
pub fn parse_cost_cap_usd(raw: &str) -> Result<u64, String> {
    let invalid = |why: &str| format!("{THINKING_COST_CAP_VAR}: {why} (got {raw:?})");
    let text = raw.trim();
    let (whole, fraction) = match text.split_once('.') {
        Some((whole, fraction)) => (whole, fraction),
        None => (text, ""),
    };
    let digits = |part: &str| part.bytes().all(|b| b.is_ascii_digit());
    if whole.is_empty() || !digits(whole) || !digits(fraction) {
        return Err(invalid(
            "expected a decimal number of USD such as 5 or 2.50",
        ));
    }
    if text.contains('.') && fraction.is_empty() {
        return Err(invalid("expected digits after the decimal point"));
    }
    if fraction.len() > 6 {
        return Err(invalid("at most 6 decimal places"));
    }
    // Bounded before parsing so a long digit string cannot overflow.
    if whole.len() > 5 {
        return Err(invalid(&format!(
            "must be at most {MAX_THINKING_COST_CAP_USD}"
        )));
    }
    let whole_usd: u64 = whole.parse().map_err(|_| invalid("not a usable number"))?;
    if whole_usd > MAX_THINKING_COST_CAP_USD {
        return Err(invalid(&format!(
            "must be at most {MAX_THINKING_COST_CAP_USD}"
        )));
    }
    let mut padded = fraction.to_owned();
    while padded.len() < 6 {
        padded.push('0');
    }
    let fraction_micro: u64 = padded.parse().map_err(|_| invalid("not a usable number"))?;
    let micro = whole_usd * MICRO_PER_USD + fraction_micro;
    if micro == 0 {
        return Err(invalid("must be greater than 0"));
    }
    if micro > MAX_THINKING_COST_CAP_USD * MICRO_PER_USD {
        return Err(invalid(&format!(
            "must be at most {MAX_THINKING_COST_CAP_USD}"
        )));
    }
    Ok(micro)
}

/// What the operator surfaces show. Text-free and bounded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThinkingUsageStatus {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    /// Summed provider-reported cost in micro-USD, or `None` when no reply has
    /// reported one (unknown, not zero).
    pub cost_micro_usd: Option<u64>,
    /// Replies that carried a usage object.
    pub turns_with_usage: u64,
    /// Replies with no usage object at all.
    pub turns_unknown_usage: u64,
    /// Replies that reported a cost; the rest are cost-unknown.
    pub turns_cost_reported: u64,
    /// The configured cap in micro-USD, when there is one.
    pub cap_micro_usd: Option<u64>,
    /// True once the reported cost reached the cap: Thinking is suspended.
    pub cap_tripped: bool,
    /// Thinking calls refused because the cap had tripped.
    pub suspended_calls: u64,
}

#[derive(Debug, Default)]
struct State {
    prompt_tokens: u64,
    completion_tokens: u64,
    cost_micro_usd: u64,
    turns_with_usage: u64,
    turns_unknown_usage: u64,
    turns_cost_reported: u64,
    suspended_calls: u64,
    cap_micro_usd: Option<u64>,
}

impl State {
    fn tripped(&self) -> bool {
        self.cap_micro_usd
            .is_some_and(|cap| self.turns_cost_reported > 0 && self.cost_micro_usd >= cap)
    }
}

/// Shared handle to the session totals. Cloning shares the same totals.
#[derive(Debug, Clone, Default)]
pub struct ThinkingMeter {
    state: Arc<Mutex<State>>,
}

impl ThinkingMeter {
    /// `cap_micro_usd` of `None` means no cap: the totals are display only.
    pub fn new(cap_micro_usd: Option<u64>) -> Self {
        Self {
            state: Arc::new(Mutex::new(State {
                cap_micro_usd,
                ..State::default()
            })),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Record one successful Thinking reply. `None` is unknown usage.
    pub fn record(&self, usage: Option<ThinkingUsage>) {
        let mut state = self.lock();
        let was_tripped = state.tripped();
        match usage {
            None => state.turns_unknown_usage = state.turns_unknown_usage.saturating_add(1),
            Some(usage) => {
                state.turns_with_usage = state.turns_with_usage.saturating_add(1);
                state.prompt_tokens = state
                    .prompt_tokens
                    .saturating_add(u64::from(usage.prompt_tokens));
                state.completion_tokens = state
                    .completion_tokens
                    .saturating_add(u64::from(usage.completion_tokens));
                if let Some(cost) = usage.cost_micro_usd {
                    state.turns_cost_reported = state.turns_cost_reported.saturating_add(1);
                    state.cost_micro_usd = state.cost_micro_usd.saturating_add(cost);
                }
            }
        }
        if !was_tripped && state.tripped() {
            eprintln!(
                "thinking cost cap reached ({} micro-USD of {:?}): Thinking is suspended until restart; replies use the deterministic fallback",
                state.cost_micro_usd, state.cap_micro_usd
            );
        }
    }

    /// Whether the cap has tripped (always false without a cap).
    pub fn is_suspended(&self) -> bool {
        self.lock().tripped()
    }

    fn note_suspended(&self) {
        let mut state = self.lock();
        state.suspended_calls = state.suspended_calls.saturating_add(1);
    }

    pub fn status(&self) -> ThinkingUsageStatus {
        let state = self.lock();
        ThinkingUsageStatus {
            prompt_tokens: state.prompt_tokens,
            completion_tokens: state.completion_tokens,
            cost_micro_usd: (state.turns_cost_reported > 0).then_some(state.cost_micro_usd),
            turns_with_usage: state.turns_with_usage,
            turns_unknown_usage: state.turns_unknown_usage,
            turns_cost_reported: state.turns_cost_reported,
            cap_micro_usd: state.cap_micro_usd,
            cap_tripped: state.tripped(),
            suspended_calls: state.suspended_calls,
        }
    }
}

/// Counts every reply the wrapped engine returns and refuses calls once the
/// cap has tripped. The refusal is an ordinary typed engine error, so the
/// pipeline takes its existing deterministic fallback.
pub struct MeteredThinking {
    inner: Arc<dyn ThinkingEngine>,
    meter: ThinkingMeter,
}

impl MeteredThinking {
    pub fn new(inner: Arc<dyn ThinkingEngine>, meter: ThinkingMeter) -> Self {
        Self { inner, meter }
    }

    fn suspended_error(&self) -> EngineError {
        self.meter.note_suspended();
        EngineError::new(
            EngineErrorKind::QuotaExhausted,
            "thinking suspended: session cost cap reached",
        )
    }

    fn metered<'a>(
        &self,
        call: EngineFuture<'a, GeneratedReply>,
    ) -> EngineFuture<'a, GeneratedReply> {
        let meter = self.meter.clone();
        Box::pin(async move {
            let reply = call.await?;
            meter.record(reply.usage);
            Ok(reply)
        })
    }
}

impl ThinkingEngine for MeteredThinking {
    fn generate<'a>(&'a self, request: &'a ThinkingRequest) -> EngineFuture<'a, GeneratedReply> {
        if self.meter.is_suspended() {
            let error = self.suspended_error();
            return Box::pin(async move { Err(error) });
        }
        self.metered(self.inner.generate(request))
    }

    fn streams(&self) -> bool {
        self.inner.streams()
    }

    fn generate_streaming(
        &self,
        request: &ThinkingRequest,
        timeout: Duration,
        sink: &mut dyn ReplyDeltaSink,
    ) -> Option<Result<GeneratedReply, EngineError>> {
        if self.meter.is_suspended() {
            return Some(Err(self.suspended_error()));
        }
        let result = self.inner.generate_streaming(request, timeout, sink)?;
        if let Ok(reply) = &result {
            self.meter.record(reply.usage);
        }
        Some(result)
    }

    fn generate_with_timeout<'a>(
        &'a self,
        request: &'a ThinkingRequest,
        timeout: Duration,
    ) -> EngineFuture<'a, GeneratedReply> {
        if self.meter.is_suspended() {
            let error = self.suspended_error();
            return Box::pin(async move { Err(error) });
        }
        self.metered(self.inner.generate_with_timeout(request, timeout))
    }

    fn identity(&self) -> BackendIdentity {
        self.inner.identity()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aivtuber_domain::{
        DeltaFlow, EventEnvelope, PrivacyClass, ReflexContext, RetrievalSnapshot,
    };
    use std::future::Future;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::task::{Context, Poll, Waker};

    fn usage(prompt: u32, completion: u32, cost: Option<u64>) -> ThinkingUsage {
        ThinkingUsage {
            prompt_tokens: prompt,
            completion_tokens: completion,
            cost_micro_usd: cost,
        }
    }

    fn ready<T>(future: impl Future<Output = T>) -> T {
        let mut future = Box::pin(future);
        match future
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
        {
            Poll::Ready(value) => value,
            Poll::Pending => panic!("test future must be ready"),
        }
    }

    #[test]
    fn accumulates_and_distinguishes_unknown_from_zero() {
        let meter = ThinkingMeter::new(None);
        assert_eq!(
            meter.status().cost_micro_usd,
            None,
            "nothing yet is unknown"
        );

        meter.record(None);
        let status = meter.status();
        assert_eq!(status.turns_unknown_usage, 1);
        assert_eq!(status.cost_micro_usd, None, "no usage is unknown, not 0");

        meter.record(Some(usage(100, 20, None)));
        let status = meter.status();
        assert_eq!((status.prompt_tokens, status.completion_tokens), (100, 20));
        assert_eq!(status.turns_with_usage, 1);
        assert_eq!(
            status.cost_micro_usd, None,
            "tokens without cost stay unknown"
        );

        meter.record(Some(usage(10, 5, Some(1_500))));
        meter.record(Some(usage(1, 1, Some(0))));
        let status = meter.status();
        assert_eq!((status.prompt_tokens, status.completion_tokens), (111, 26));
        assert_eq!(
            status.cost_micro_usd,
            Some(1_500),
            "a reported 0 is a known 0"
        );
        assert_eq!(status.turns_cost_reported, 2);
        assert_eq!(status.turns_with_usage, 3);
        assert!(!status.cap_tripped);
    }

    #[test]
    fn totals_saturate() {
        let meter = ThinkingMeter::new(None);
        for _ in 0..3 {
            meter.record(Some(usage(u32::MAX, u32::MAX, Some(u64::MAX - 1))));
        }
        let status = meter.status();
        assert_eq!(status.prompt_tokens, 3 * u64::from(u32::MAX));
        assert_eq!(status.cost_micro_usd, Some(u64::MAX));
        meter.state.lock().unwrap().prompt_tokens = u64::MAX;
        meter.record(Some(usage(5, 5, None)));
        assert_eq!(meter.status().prompt_tokens, u64::MAX);
    }

    #[test]
    fn cap_trips_at_reported_cost_and_stays_tripped() {
        let meter = ThinkingMeter::new(Some(2_000_000));
        meter.record(Some(usage(1, 1, Some(1_999_999))));
        assert!(!meter.is_suspended());
        meter.record(None);
        meter.record(Some(usage(1, 1, None)));
        assert!(
            !meter.is_suspended(),
            "unknown cost never counts toward the cap"
        );
        meter.record(Some(usage(1, 1, Some(1))));
        assert!(meter.is_suspended(), "reaching the cap exactly trips it");
        let status = meter.status();
        assert!(status.cap_tripped);
        assert_eq!(status.cap_micro_usd, Some(2_000_000));
        meter.record(Some(usage(1, 1, Some(0))));
        assert!(meter.is_suspended());
    }

    #[test]
    fn no_cap_never_suspends() {
        let meter = ThinkingMeter::new(None);
        meter.record(Some(usage(1, 1, Some(u64::MAX))));
        assert!(!meter.is_suspended());
        assert_eq!(meter.status().cap_micro_usd, None);
    }

    #[test]
    fn cap_parsing() {
        assert_eq!(parse_cost_cap_usd("5"), Ok(5_000_000));
        assert_eq!(parse_cost_cap_usd(" 2.50 "), Ok(2_500_000));
        assert_eq!(parse_cost_cap_usd("0.000001"), Ok(1));
        assert_eq!(parse_cost_cap_usd("0.5"), Ok(500_000));
        assert_eq!(parse_cost_cap_usd("10000"), Ok(10_000_000_000));
        assert_eq!(parse_cost_cap_usd("10000.00"), Ok(10_000_000_000));
        for bad in [
            "",
            "0",
            "0.0",
            "0.000000",
            "-1",
            "+1",
            "abc",
            "1e3",
            "1,5",
            ".5",
            "5.",
            "1.0000001",
            "10000.000001",
            "10001",
            "99999999999999999999999",
            "1 2",
            "NaN",
            "inf",
            "--1",
        ] {
            assert!(parse_cost_cap_usd(bad).is_err(), "{bad:?} must be rejected");
        }
    }

    struct Scripted {
        replies: Mutex<Vec<Option<ThinkingUsage>>>,
        calls: AtomicU64,
        stream: bool,
    }

    impl Scripted {
        fn new(replies: Vec<Option<ThinkingUsage>>, stream: bool) -> Self {
            Self {
                replies: Mutex::new(replies),
                calls: AtomicU64::new(0),
                stream,
            }
        }

        fn next(&self) -> GeneratedReply {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let usage = self.replies.lock().unwrap().remove(0);
            GeneratedReply {
                text: "hello".to_owned(),
                usage,
            }
        }
    }

    impl ThinkingEngine for Scripted {
        fn generate<'a>(&'a self, _: &'a ThinkingRequest) -> EngineFuture<'a, GeneratedReply> {
            let reply = self.next();
            Box::pin(async move { Ok(reply) })
        }

        fn streams(&self) -> bool {
            self.stream
        }

        fn generate_streaming(
            &self,
            _: &ThinkingRequest,
            _: Duration,
            sink: &mut dyn ReplyDeltaSink,
        ) -> Option<Result<GeneratedReply, EngineError>> {
            if !self.stream {
                return None;
            }
            let reply = self.next();
            assert!(matches!(sink.push("hel"), DeltaFlow::Continue));
            Some(Ok(reply))
        }

        fn identity(&self) -> BackendIdentity {
            BackendIdentity {
                name: "scripted".to_owned(),
                model_alias: None,
                model_version: None,
            }
        }
    }

    struct Sink;
    impl ReplyDeltaSink for Sink {
        fn push(&mut self, _: &str) -> DeltaFlow {
            DeltaFlow::Continue
        }
    }

    fn request() -> ThinkingRequest {
        let event: EventEnvelope =
            serde_json::from_str(include_str!("../../../examples/events/chat-message.json"))
                .expect("chat fixture");
        ThinkingRequest::from_event(
            &event,
            "hello",
            PrivacyClass::Pseudonymous,
            ReflexContext::default(),
            RetrievalSnapshot::default(),
            Vec::new(),
        )
    }

    #[test]
    fn buffered_and_streamed_replies_are_recorded_and_cap_suspends() {
        let inner = Arc::new(Scripted::new(
            vec![
                Some(usage(10, 2, Some(600_000))),
                None,
                Some(usage(4, 1, Some(500_000))),
            ],
            true,
        ));
        let meter = ThinkingMeter::new(Some(1_000_000));
        let engine = MeteredThinking::new(inner.clone(), meter.clone());
        let request = request();

        assert!(engine.streams());
        ready(engine.generate(&request)).expect("first");
        engine
            .generate_streaming(&request, Duration::from_secs(1), &mut Sink)
            .expect("streams")
            .expect("second");
        assert!(!meter.is_suspended());
        ready(engine.generate_with_timeout(&request, Duration::from_secs(1))).expect("third");
        let status = meter.status();
        assert_eq!((status.prompt_tokens, status.completion_tokens), (14, 3));
        assert_eq!(status.turns_unknown_usage, 1);
        assert_eq!(status.cost_micro_usd, Some(1_100_000));
        assert!(status.cap_tripped);

        // Suspended: typed refusal on every path, the provider is not called.
        let error = ready(engine.generate(&request)).expect_err("buffered refused");
        assert_eq!(error.kind, EngineErrorKind::QuotaExhausted);
        let error = ready(engine.generate_with_timeout(&request, Duration::from_secs(1)))
            .expect_err("timeout path refused");
        assert_eq!(error.kind, EngineErrorKind::QuotaExhausted);
        let error = engine
            .generate_streaming(&request, Duration::from_secs(1), &mut Sink)
            .expect("still a stream answer")
            .expect_err("stream refused");
        assert_eq!(error.kind, EngineErrorKind::QuotaExhausted);
        assert_eq!(
            inner.calls.load(Ordering::SeqCst),
            3,
            "no provider call after the trip"
        );
        assert_eq!(meter.status().suspended_calls, 3);
    }

    #[test]
    fn a_non_streaming_engine_still_answers_none() {
        let inner = Arc::new(Scripted::new(vec![], false));
        let engine = MeteredThinking::new(inner, ThinkingMeter::new(None));
        assert!(!engine.streams());
        assert!(
            engine
                .generate_streaming(&request(), Duration::from_secs(1), &mut Sink)
                .is_none()
        );
    }
}
