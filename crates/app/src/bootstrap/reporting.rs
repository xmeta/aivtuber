use aivtuber_app::{
    AdapterHealth, GenerationFallbackCounts, RawIngressMetrics, RawIngressMetricsSnapshot,
};
use std::time::Instant;

/// One line per newly counted generation fallback, naming the typed reason
/// (`timeout`, `unavailable`, `policy_override`, ...) from the closed
/// fallback vocabulary. Bounded by construction: it prints only when a counter
/// moved, never carries provider text or error messages, and the totals are
/// the process-lifetime counters (issue #288).
pub(crate) fn report_generation_fallbacks(
    current: &GenerationFallbackCounts,
    previous: &mut GenerationFallbackCounts,
) {
    if current == previous {
        return;
    }
    for (reason, count) in &current.by_reason {
        if previous.by_reason.get(reason) != Some(count) {
            eprintln!(
                "generation_fallback: reason={reason} count={count} visible_total={} unseen_total={}",
                current.visible, current.silent
            );
        }
    }
    previous.clone_from(current);
}

/// Drain the bounded chat-burst records and deferred-reply reports and log a
/// counters line when anything was accounted. Event ids and counts only: what
/// a viewer wrote is never logged.
pub(crate) fn report_chat_burst<R: aivtuber_app::RoutePlanner>(
    app: &mut aivtuber_app::ProductionApp<R>,
    last_accounted: &mut u64,
    last_crowd: &mut [u64; 7],
) {
    for report in app.take_deferred_reply_reports(16) {
        eprintln!(
            "chat_burst: deferred reply event_id={} at_ms={} dispatched={} playback={}",
            report.event_id,
            report.at_ms,
            report.dispatched,
            report.asset_id.as_deref().unwrap_or("none")
        );
    }
    // The terminal and signal rings are bounded; draining them keeps them from
    // dropping. A signal is logged as ids and counts only.
    let _ = app.take_chat_burst_records(usize::MAX);
    for signal in app.take_audience_signals(16) {
        eprintln!(
            "chat_burst: audience signal batch={} index={} class={:?} method={:?} count={} unique_actors={} intensity_milli={} trend={:?} carrier={}",
            signal.batch_id,
            signal.index,
            signal.class,
            signal.method,
            signal.count,
            signal.unique_actors_estimate,
            signal.intensity_milli,
            signal.trend,
            signal
                .representative_event_ids
                .first()
                .map_or("", String::as_str)
        );
    }
    for (class, members) in app.take_crowd_routes(16) {
        eprintln!("chat_burst: crowd routed class={class:?} members={members}");
    }
    let Some(metrics) = app.chat_burst_metrics() else {
        return;
    };
    // #79: one line that separates why a wave did or did not become a crowd
    // reaction. Cumulative session counters, printed whenever any of them
    // moves; nothing is counted here.
    let crowd = metrics.crowd_diagnostic();
    if crowd != *last_crowd {
        *last_crowd = crowd;
        eprintln!(
            "chat_burst: crowd quiet_passthrough={} batched={} below_threshold={} routed={} voiced={} unvoiced={} cooldown_suppressed={}",
            crowd[0], crowd[1], crowd[2], crowd[3], crowd[4], crowd[5], crowd[6]
        );
    }
    let accounted = metrics.accounted();
    if accounted == *last_accounted {
        return;
    }
    *last_accounted = accounted;
    eprintln!(
        "chat_burst: admitted={} dispatched={} held={} deferred_pending={} deferred_offered={} expired={} superseded={} erased={} operator_cancelled={} revalidation_refused={} evicted={} actor_capped={} not_selected={} shed={} invariant_violations={}",
        metrics.admitted,
        metrics.dispatched(),
        metrics.held,
        metrics.deferred.pending,
        metrics.deferred.offered,
        metrics
            .deferred
            .count(aivtuber_scheduler::DeferredOutcome::Expired),
        metrics
            .deferred
            .count(aivtuber_scheduler::DeferredOutcome::Superseded),
        metrics
            .deferred
            .count(aivtuber_scheduler::DeferredOutcome::Erased),
        metrics
            .deferred
            .count(aivtuber_scheduler::DeferredOutcome::OperatorCancelled),
        metrics
            .deferred
            .count(aivtuber_scheduler::DeferredOutcome::RevalidationRefused),
        metrics
            .deferred
            .count(aivtuber_scheduler::DeferredOutcome::EvictedForCapacity),
        metrics
            .deferred
            .count(aivtuber_scheduler::DeferredOutcome::RejectedActorCap),
        metrics.terminal[2] + metrics.terminal[3] + metrics.terminal[4],
        metrics.terminal[5],
        metrics.invariant_violations
            + metrics.burst_invariant_violations
            + metrics.deferred.invariant_violations
    );
    if metrics.audience_signals > 0 || metrics.audience_guarded > 0 {
        eprintln!(
            "chat_burst: audience signals={} represented={} guarded={} signals_dropped={}",
            metrics.audience_signals,
            metrics.audience_members_represented,
            metrics.audience_guarded,
            metrics.audience_signals_dropped
        );
    }
}

pub(crate) fn elapsed_ms(started: Instant) -> u64 {
    started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64
}

pub(crate) fn report_raw_ingress_metrics(
    metrics: &RawIngressMetrics,
    previous: &mut RawIngressMetricsSnapshot,
    final_report: bool,
) {
    let current = metrics.snapshot();
    let drops_changed = current.dropped_queue_full != previous.dropped_queue_full
        || current.dropped_oversize != previous.dropped_oversize
        || current.read_errors != previous.read_errors;
    if drops_changed || final_report {
        eprintln!(
            "raw_ingress_metrics: enqueued={} dropped_queue_full={} dropped_oversize={} read_errors={} max_retained_record_bytes={}",
            current.enqueued,
            current.dropped_queue_full,
            current.dropped_oversize,
            current.read_errors,
            current.max_retained_record_bytes,
        );
    }
    *previous = current;
}

pub(crate) fn report_degraded(avatar_output: &str, health: &AdapterHealth) {
    if let Some(error) = &health.avatar_error {
        eprintln!("{avatar_output} degraded: {error}");
    }
    if let Some(error) = &health.stream_error {
        eprintln!("OBS degraded: {error}");
    }
}
