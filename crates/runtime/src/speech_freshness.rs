//! Projection freshness of played speech assets (issue #222 S2, ADR-0004 D-4).
//!
//! An asset whose recorded `compatibility.speech_projection` differs from the
//! projection the runtime would use now is *stale but playable*: its audio
//! still carries the meaning, so it is played, counted, and a bounded,
//! deduplicated re-synthesis request is queued so the next use is fresh.
//! A failed re-synthesis degrades silently (playback is never blocked by it).
//!
//! The operator can additionally force assets affected by a lexicon surface to
//! *must re-synthesize*: those are refused at play time until re-synthesized,
//! for when a wrong reading must stop immediately. Forcing is operator
//! configuration only; nothing in this module is reachable from chat, model
//! output or a provider.
//!
//! Everything here is deterministic and explicitly bounded.

use aivtuber_asset_store::PerformanceAsset;
use std::collections::{BTreeSet, VecDeque};

/// Outstanding (queued or in-flight) re-synthesis requests.
pub const MAX_RESYNTHESIS_PENDING: usize = 32;
/// Assets the operator may force to must-re-synthesize at once.
pub const MAX_FORCED_RESYNTHESIS: usize = 256;
/// Assets remembered as having failed re-synthesis (not retried until the
/// projection changes).
pub const MAX_RESYNTHESIS_FAILED: usize = 64;
/// Longest operator invalidation surface, in characters (the lexicon bound).
pub const MAX_INVALIDATION_SURFACE_CHARS: usize = 32;
/// Descriptors an invalidation scan reads at most.
pub const MAX_INVALIDATION_SCAN: usize = 4096;

/// What playing an asset should do about its projection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Freshness {
    Fresh,
    /// Play it; a re-synthesis was requested.
    StalePlayable,
    /// Do not play it until re-synthesized (operator-forced).
    MustResynthesize,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SpeechFreshnessMetrics {
    /// `projection_stale_played`: stale assets that were played anyway.
    pub projection_stale_played: u64,
    /// Plays refused because the operator forced the asset.
    pub forced_refused: u64,
    /// Re-synthesis requests accepted into the queue.
    pub resynthesis_requested: u64,
    /// Requests dropped because the outstanding set was full.
    pub resynthesis_dropped_full: u64,
    pub resynthesis_succeeded: u64,
    pub resynthesis_failed: u64,
    pub pending: usize,
    pub forced: usize,
}

#[derive(Debug, Default)]
pub struct SpeechFreshness {
    current: Option<String>,
    /// Queued, not yet taken.
    queue: VecDeque<String>,
    /// Queued or in flight: the dedupe set and the bound.
    outstanding: BTreeSet<String>,
    forced: BTreeSet<String>,
    failed: VecDeque<String>,
    failed_set: BTreeSet<String>,
    metrics: SpeechFreshnessMetrics,
}

impl SpeechFreshness {
    /// The projection identity the runtime uses now. `None` (no projection
    /// pipeline) disables staleness: nothing can be re-synthesized then.
    /// Changing it forgets earlier failures, so they are retried once.
    pub fn set_current(&mut self, current: Option<String>) {
        if self.current != current {
            self.failed.clear();
            self.failed_set.clear();
        }
        self.current = current;
    }

    pub fn current(&self) -> Option<&str> {
        self.current.as_deref()
    }

    pub fn metrics(&self) -> SpeechFreshnessMetrics {
        SpeechFreshnessMetrics {
            pending: self.outstanding.len(),
            forced: self.forced.len(),
            ..self.metrics
        }
    }

    /// Decide what to do with an asset about to be played.
    pub fn classify(&mut self, asset: &PerformanceAsset) -> Freshness {
        if self.forced.contains(&asset.id) {
            self.metrics.forced_refused += 1;
            self.request(&asset.id);
            return Freshness::MustResynthesize;
        }
        if self.current.is_some()
            && asset
                .compatibility
                .speech_projection_is_stale(self.current.as_deref())
        {
            self.metrics.projection_stale_played += 1;
            if !self.failed_set.contains(&asset.id) {
                self.request(&asset.id);
            }
            return Freshness::StalePlayable;
        }
        Freshness::Fresh
    }

    /// Queue a re-synthesis of `asset_id` unless one is outstanding or the
    /// bound is reached.
    pub fn request(&mut self, asset_id: &str) {
        if self.outstanding.contains(asset_id) {
            return;
        }
        if self.outstanding.len() >= MAX_RESYNTHESIS_PENDING {
            self.metrics.resynthesis_dropped_full += 1;
            return;
        }
        self.outstanding.insert(asset_id.to_owned());
        self.queue.push_back(asset_id.to_owned());
        self.metrics.resynthesis_requested += 1;
    }

    /// Operator action: mark `asset_id` must-re-synthesize (and request it).
    /// Returns false when the forced set is full.
    pub fn force(&mut self, asset_id: &str) -> bool {
        if !self.forced.contains(asset_id) && self.forced.len() >= MAX_FORCED_RESYNTHESIS {
            return false;
        }
        self.forced.insert(asset_id.to_owned());
        self.request(asset_id);
        true
    }

    /// Take the next request. It stays outstanding until [`Self::complete`] or
    /// [`Self::fail`].
    pub fn next_request(&mut self) -> Option<String> {
        self.queue.pop_front()
    }

    pub fn complete(&mut self, asset_id: &str) {
        self.outstanding.remove(asset_id);
        self.forced.remove(asset_id);
        self.forget_failure(asset_id);
        self.metrics.resynthesis_succeeded += 1;
    }

    /// A failed re-synthesis. A merely stale asset stays playable and is not
    /// retried until the projection changes; a forced asset stays refused and
    /// is requested again on its next use.
    pub fn fail(&mut self, asset_id: &str) {
        self.outstanding.remove(asset_id);
        self.metrics.resynthesis_failed += 1;
        if self.forced.contains(asset_id) || self.failed_set.contains(asset_id) {
            return;
        }
        self.failed.push_back(asset_id.to_owned());
        self.failed_set.insert(asset_id.to_owned());
        while self.failed.len() > MAX_RESYNTHESIS_FAILED {
            if let Some(old) = self.failed.pop_front() {
                self.failed_set.remove(&old);
            }
        }
    }

    /// The asset vanished (evicted or invalidated): drop every trace of it.
    pub fn forget(&mut self, asset_id: &str) {
        self.outstanding.remove(asset_id);
        self.queue.retain(|id| id != asset_id);
        self.forced.remove(asset_id);
        self.forget_failure(asset_id);
    }

    fn forget_failure(&mut self, asset_id: &str) {
        if self.failed_set.remove(asset_id) {
            self.failed.retain(|id| id != asset_id);
        }
    }
}

/// Whether a display text contains an operator surface (ASCII case-insensitive,
/// the lexicon's default matching).
pub fn text_contains_surface(text: &str, surface: &str) -> bool {
    !surface.is_empty()
        && text
            .to_ascii_lowercase()
            .contains(&surface.to_ascii_lowercase())
}

/// Validate an operator invalidation surface: 1..=32 characters, no control
/// characters, not blank.
pub fn valid_invalidation_surface(surface: &str) -> bool {
    let count = surface.chars().count();
    (1..=MAX_INVALIDATION_SURFACE_CHARS).contains(&count)
        && !surface.trim().is_empty()
        && !surface.chars().any(char::is_control)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aivtuber_asset_store::load_asset_file;
    use std::path::PathBuf;

    fn asset_with(projection: Option<&str>) -> PerformanceAsset {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../examples/assets/reaction-surprise-01.json");
        let mut asset = load_asset_file(path).expect("fixture asset");
        asset.compatibility.speech_projection = projection.map(str::to_owned);
        asset
    }

    const OLD: &str = "rules:1|lexicon:none";
    const NEW: &str =
        "rules:1|lexicon:a@1+0000000000000000000000000000000000000000000000000000000000000000";

    #[test]
    fn stale_assets_play_are_counted_and_requested_once() {
        let mut freshness = SpeechFreshness::default();
        freshness.set_current(Some(NEW.to_owned()));
        let stale = asset_with(Some(OLD));
        for _ in 0..3 {
            assert_eq!(freshness.classify(&stale), Freshness::StalePlayable);
        }
        let metrics = freshness.metrics();
        assert_eq!(metrics.projection_stale_played, 3);
        assert_eq!(metrics.resynthesis_requested, 1, "deduplicated");
        assert_eq!(freshness.next_request().as_deref(), Some(stale.id.as_str()));
        // Still outstanding while in flight.
        freshness.classify(&stale);
        assert_eq!(freshness.metrics().resynthesis_requested, 1);
        assert_eq!(freshness.next_request(), None);
        freshness.complete(&stale.id);
        assert_eq!(freshness.metrics().pending, 0);
    }

    #[test]
    fn fresh_unrecorded_and_unconfigured_assets_are_left_alone() {
        let mut freshness = SpeechFreshness::default();
        // No projection pipeline: nothing is ever stale.
        assert_eq!(freshness.classify(&asset_with(Some(OLD))), Freshness::Fresh);
        freshness.set_current(Some(NEW.to_owned()));
        assert_eq!(freshness.classify(&asset_with(Some(NEW))), Freshness::Fresh);
        // Unknown provenance (schema 0.1.0 or a recorded take) is not stale.
        assert_eq!(freshness.classify(&asset_with(None)), Freshness::Fresh);
        assert_eq!(freshness.metrics().projection_stale_played, 0);
        assert_eq!(freshness.next_request(), None);
    }

    #[test]
    fn requests_are_bounded_and_overflow_is_counted() {
        let mut freshness = SpeechFreshness::default();
        for index in 0..(MAX_RESYNTHESIS_PENDING + 5) {
            freshness.request(&format!("asset-{index}"));
        }
        let metrics = freshness.metrics();
        assert_eq!(metrics.pending, MAX_RESYNTHESIS_PENDING);
        assert_eq!(metrics.resynthesis_dropped_full, 5);
    }

    #[test]
    fn a_failed_resynthesis_leaves_a_stale_asset_playable_and_is_not_retried() {
        let mut freshness = SpeechFreshness::default();
        freshness.set_current(Some(NEW.to_owned()));
        let stale = asset_with(Some(OLD));
        freshness.classify(&stale);
        let id = freshness.next_request().unwrap();
        freshness.fail(&id);
        assert_eq!(freshness.classify(&stale), Freshness::StalePlayable);
        assert_eq!(freshness.next_request(), None, "no retry storm");
        assert_eq!(freshness.metrics().resynthesis_failed, 1);
        // A new projection forgets the failure.
        freshness.set_current(Some("rules:2|lexicon:none".to_owned()));
        freshness.classify(&stale);
        assert_eq!(freshness.next_request().as_deref(), Some(stale.id.as_str()));
    }

    #[test]
    fn forced_assets_are_refused_until_resynthesized() {
        let mut freshness = SpeechFreshness::default();
        freshness.set_current(Some(NEW.to_owned()));
        let asset = asset_with(Some(NEW));
        assert!(freshness.force(&asset.id));
        assert_eq!(freshness.classify(&asset), Freshness::MustResynthesize);
        assert_eq!(freshness.metrics().forced_refused, 1);
        let id = freshness.next_request().unwrap();
        freshness.fail(&id);
        // Failure does not release a forced asset; its next use asks again.
        assert_eq!(freshness.classify(&asset), Freshness::MustResynthesize);
        assert_eq!(freshness.next_request().as_deref(), Some(asset.id.as_str()));
        freshness.complete(&asset.id);
        assert_eq!(freshness.classify(&asset), Freshness::Fresh);
    }

    #[test]
    fn the_forced_set_is_bounded() {
        let mut freshness = SpeechFreshness::default();
        for index in 0..MAX_FORCED_RESYNTHESIS {
            assert!(freshness.force(&format!("a{index}")));
        }
        assert!(!freshness.force("overflow"));
        assert_eq!(freshness.metrics().forced, MAX_FORCED_RESYNTHESIS);
    }

    #[test]
    fn surface_matching_and_validation() {
        assert!(text_contains_surface("I like OpenAI a lot", "openai"));
        assert!(text_contains_surface("星乃ひかりです", "星乃"));
        assert!(!text_contains_surface("hello", ""));
        assert!(valid_invalidation_surface("OpenAI"));
        assert!(!valid_invalidation_surface(""));
        assert!(!valid_invalidation_surface("  "));
        assert!(!valid_invalidation_surface("a\nb"));
        assert!(!valid_invalidation_surface(&"x".repeat(33)));
    }
}
