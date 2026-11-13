//! Issue #78 (S3): retrieval-based local score for batched chat.
//!
//! The burst engine orders candidates by a caller-supplied local score. S2
//! shipped a text heuristic; this module augments it with the *existing*
//! semantic-retrieval path ([`SemanticIndex::search`], the index the Reflex
//! pipeline queries) instead of adding a second retrieval implementation. A
//! comment's retrieval component is the cosine similarity of its best match in
//! the curated Performance Asset index: how well the reviewed library can
//! already answer it.
//!
//! Properties (tested):
//!
//! * **Deterministic and replay-stable.** The embedding is a pure function of
//!   the bounded comment text, the search is the index's deterministic
//!   Top-K (similarity descending, then asset id), the similarity is rounded
//!   to an integer milli value, and the lookup budget runs on the caller's
//!   monotonic `now_ms`, never a wall clock.
//! * **Bounded per batch.** At most `max_lookups_per_window` searches per
//!   logical window of `window_ms`, and the index itself is capped at
//!   [`MAX_SCORE_INDEX_ASSETS`] assets, so the work per batch window has a hard
//!   ceiling. Beyond it a comment keeps its heuristic score.
//! * **Falls back to the current score.** No embedding, a search error or an
//!   exhausted budget returns the heuristic unchanged (counted by reason).
//! * **Cannot buy anything.** A heuristic score of zero (spam) stays zero, the
//!   blend keeps the heuristic dominant (3:1), and the result is clamped to
//!   `0..=1000`. The score only orders untrusted comments among themselves:
//!   Top-K, the actor cap, the cooldown and every gate are applied afterwards.
//!
//! No model runs here and no viewer text leaves the process.

use crate::audience::{ReactionFamily, reaction_family};
use crate::retrieval::SemanticIndex;
use aivtuber_domain::payload_intent_axis;

/// Hard ceiling on the assets of the scoring index (bounds per-lookup work).
pub const MAX_SCORE_INDEX_ASSETS: usize = 4_096;
/// Hard ceiling on lookups per window.
pub const MAX_SCORE_LOOKUPS_PER_WINDOW: usize = 1_024;
/// Hard ceiling on the lookup budget window.
pub const MAX_SCORE_WINDOW_MS: u64 = 5_000;

/// Weight of the heuristic in the blend (retrieval has `4 - this`).
const HEURISTIC_WEIGHT: u32 = 3;
const BLEND_DENOMINATOR: u32 = 4;

/// Maps a bounded comment text to a query vector in the index's embedding
/// space. Implementations must be pure functions of the text.
pub trait ChatTextEmbedder: Send {
    /// `None` when the text has no usable embedding; the caller falls back.
    fn embed(&mut self, text: &str) -> Option<Vec<f32>>;
}

/// Embedder for the starter-semantic model space (the three-axis
/// `reaction.agree` / `reaction.surprise` / `filler.thinking` vocabulary
/// [`payload_intent_axis`] already defines for the live retrieval path). A
/// question is a thinking prompt, a surprise reaction is a surprise, any other
/// reviewed reaction family (see `reaction_family`) is an agreement-style
/// reaction, and everything else spreads over every axis exactly as an unknown
/// intent does on the live path.
#[derive(Debug, Clone, Copy)]
pub struct ReactionAxisEmbedder {
    dimension: usize,
}

impl ReactionAxisEmbedder {
    pub fn new(dimension: usize) -> Self {
        Self { dimension }
    }
}

impl ChatTextEmbedder for ReactionAxisEmbedder {
    fn embed(&mut self, text: &str) -> Option<Vec<f32>> {
        let intent = if text.contains('?') || text.contains('？') {
            "filler.thinking"
        } else {
            match reaction_family(text) {
                Some(ReactionFamily::Surprise) => "reaction.surprise",
                Some(_) => "reaction.agree",
                None => "",
            }
        };
        let vector = payload_intent_axis(intent, self.dimension);
        (vector.len() == self.dimension && !vector.is_empty()).then_some(vector)
    }
}

/// Operator-facing configuration. Validated, never clamped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetrievalScoreConfig {
    /// Searches allowed per logical window.
    pub max_lookups_per_window: usize,
    /// Logical length of the lookup budget window.
    pub window_ms: u64,
}

impl Default for RetrievalScoreConfig {
    fn default() -> Self {
        Self {
            max_lookups_per_window: 64,
            window_ms: 1_500,
        }
    }
}

/// Refusal raised for an out-of-bounds configuration or index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetrievalScoreConfigError {
    pub field: &'static str,
}

impl std::fmt::Display for RetrievalScoreConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid retrieval score configuration: {}", self.field)
    }
}

impl std::error::Error for RetrievalScoreConfigError {}

impl RetrievalScoreConfig {
    pub fn validate(&self) -> Result<(), RetrievalScoreConfigError> {
        if self.max_lookups_per_window == 0
            || self.max_lookups_per_window > MAX_SCORE_LOOKUPS_PER_WINDOW
        {
            return Err(RetrievalScoreConfigError {
                field: "max_lookups_per_window",
            });
        }
        if self.window_ms == 0 || self.window_ms > MAX_SCORE_WINDOW_MS {
            return Err(RetrievalScoreConfigError { field: "window_ms" });
        }
        Ok(())
    }
}

/// Observable counters. `retrieval_scored + fallback_total() == scored`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RetrievalScoreMetrics {
    pub scored: u64,
    pub retrieval_scored: u64,
    /// Heuristic score returned because the text had no embedding.
    pub fallback_no_embedding: u64,
    /// ... because the search failed (dimension mismatch, bad vector, ...).
    pub fallback_search_error: u64,
    /// ... because the window's lookup budget was spent.
    pub fallback_budget: u64,
    /// ... because the heuristic was zero (spam is never revived).
    pub fallback_zero_heuristic: u64,
}

impl RetrievalScoreMetrics {
    pub fn fallback_total(&self) -> u64 {
        self.fallback_no_embedding
            + self.fallback_search_error
            + self.fallback_budget
            + self.fallback_zero_heuristic
    }
}

/// Retrieval-augmented local scorer over an existing [`SemanticIndex`].
pub struct RetrievalScorer {
    index: SemanticIndex,
    embedder: Box<dyn ChatTextEmbedder>,
    config: RetrievalScoreConfig,
    window_start_ms: u64,
    lookups_in_window: usize,
    metrics: RetrievalScoreMetrics,
}

impl std::fmt::Debug for RetrievalScorer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RetrievalScorer")
            .field("config", &self.config)
            .field("metrics", &self.metrics)
            .finish_non_exhaustive()
    }
}

impl RetrievalScorer {
    pub fn new(
        index: SemanticIndex,
        embedder: Box<dyn ChatTextEmbedder>,
        config: RetrievalScoreConfig,
    ) -> Result<Self, RetrievalScoreConfigError> {
        config.validate()?;
        if index.asset_count() > MAX_SCORE_INDEX_ASSETS {
            return Err(RetrievalScoreConfigError { field: "index" });
        }
        Ok(Self {
            index,
            embedder,
            config,
            window_start_ms: 0,
            lookups_in_window: 0,
            metrics: RetrievalScoreMetrics::default(),
        })
    }

    pub fn metrics(&self) -> RetrievalScoreMetrics {
        self.metrics
    }

    /// Blend the heuristic with the retrieval similarity of `text`. `now_ms` is
    /// the caller's monotonic instant (a regression is treated as the same
    /// window). Returns a value in `0..=1000`; on any fallback, `heuristic_milli`
    /// itself.
    pub fn score(&mut self, text: &str, heuristic_milli: u16, now_ms: u64) -> u16 {
        let heuristic_milli = heuristic_milli.min(1000);
        self.metrics.scored += 1;
        if heuristic_milli == 0 {
            self.metrics.fallback_zero_heuristic += 1;
            return 0;
        }
        if now_ms.saturating_sub(self.window_start_ms) >= self.config.window_ms {
            self.window_start_ms = now_ms;
            self.lookups_in_window = 0;
        }
        if self.lookups_in_window >= self.config.max_lookups_per_window {
            self.metrics.fallback_budget += 1;
            return heuristic_milli;
        }
        self.lookups_in_window += 1;
        let Some(query) = self.embedder.embed(text) else {
            self.metrics.fallback_no_embedding += 1;
            return heuristic_milli;
        };
        let similarity = match self.index.search(&query, 1) {
            Ok(result) => result
                .candidates
                .first()
                .map(|candidate| candidate.similarity),
            Err(_) => None,
        };
        let Some(similarity) = similarity.filter(|value| value.is_finite()) else {
            self.metrics.fallback_search_error += 1;
            return heuristic_milli;
        };
        let retrieval_milli = (similarity.clamp(0.0, 1.0) * 1000.0).round() as u32;
        self.metrics.retrieval_scored += 1;
        let blended = (HEURISTIC_WEIGHT * u32::from(heuristic_milli)
            + (BLEND_DENOMINATOR - HEURISTIC_WEIGHT) * retrieval_milli)
            / BLEND_DENOMINATOR;
        blended.min(1000) as u16
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::retrieval::{IndexedAsset, RetrievalMetadata, SimilarityMetric};

    fn index() -> SemanticIndex {
        SemanticIndex::new(
            RetrievalMetadata {
                retriever_version: "test".to_owned(),
                embedding_model: "starter-semantic".to_owned(),
                index_version: "1".to_owned(),
                asset_compiler_version: "1".to_owned(),
                similarity_metric: SimilarityMetric::Cosine,
                tie_break_rule: "similarity_desc_then_asset_id_asc".to_owned(),
            },
            vec![
                IndexedAsset {
                    asset_id: "agree".to_owned(),
                    asset_identity: None,
                    embedding: vec![1.0, 0.0, 0.0],
                },
                IndexedAsset {
                    asset_id: "surprise".to_owned(),
                    asset_identity: None,
                    embedding: vec![0.0, 1.0, 0.0],
                },
                IndexedAsset {
                    asset_id: "thinking".to_owned(),
                    asset_identity: None,
                    embedding: vec![0.0, 0.0, 1.0],
                },
            ],
        )
        .expect("index")
    }

    fn scorer(config: RetrievalScoreConfig) -> RetrievalScorer {
        RetrievalScorer::new(index(), Box::new(ReactionAxisEmbedder::new(3)), config).unwrap()
    }

    struct Failing;
    impl ChatTextEmbedder for Failing {
        fn embed(&mut self, _: &str) -> Option<Vec<f32>> {
            None
        }
    }

    struct WrongDimension;
    impl ChatTextEmbedder for WrongDimension {
        fn embed(&mut self, _: &str) -> Option<Vec<f32>> {
            Some(vec![1.0; 7])
        }
    }

    #[test]
    fn matched_library_text_outscores_unmatched_text_at_equal_heuristic() {
        let mut scorer = scorer(RetrievalScoreConfig::default());
        let matched = scorer.score("wow!", 500, 0);
        let unmatched = scorer.score("the lighting here is odd", 500, 0);
        assert!(matched > unmatched, "{matched} vs {unmatched}");
        assert!(matched <= 1000);
        assert_eq!(scorer.metrics().retrieval_scored, 2);
    }

    #[test]
    fn deterministic_across_instances_and_calls() {
        let texts = ["wow!", "how did you do that?", "草", "plain remark", "gg"];
        let run = || {
            let mut scorer = scorer(RetrievalScoreConfig::default());
            texts
                .iter()
                .enumerate()
                .map(|(i, t)| scorer.score(t, 400, i as u64 * 10))
                .collect::<Vec<_>>()
        };
        assert_eq!(run(), run());
    }

    #[test]
    fn spam_stays_zero_and_heuristic_dominates() {
        let mut scorer = scorer(RetrievalScoreConfig::default());
        assert_eq!(scorer.score("wow!", 0, 0), 0);
        assert_eq!(scorer.metrics().fallback_zero_heuristic, 1);
        // Even a perfect library match cannot lift a 100 heuristic past
        // (3*100 + 1000) / 4.
        assert!(scorer.score("wow!", 100, 0) <= 325);
        assert!(scorer.score("wow!", 5000, 0) <= 1000);
    }

    #[test]
    fn falls_back_to_heuristic_when_retrieval_is_unavailable() {
        let mut none =
            RetrievalScorer::new(index(), Box::new(Failing), RetrievalScoreConfig::default())
                .unwrap();
        assert_eq!(none.score("hello", 321, 0), 321);
        assert_eq!(none.metrics().fallback_no_embedding, 1);

        let mut wrong = RetrievalScorer::new(
            index(),
            Box::new(WrongDimension),
            RetrievalScoreConfig::default(),
        )
        .unwrap();
        assert_eq!(wrong.score("hello", 321, 0), 321);
        assert_eq!(wrong.metrics().fallback_search_error, 1);
    }

    #[test]
    fn lookup_budget_is_bounded_per_logical_window() {
        let mut scorer = scorer(RetrievalScoreConfig {
            max_lookups_per_window: 2,
            window_ms: 100,
        });
        let _ = scorer.score("wow!", 500, 0);
        let _ = scorer.score("wow!", 500, 10);
        // Third lookup in the window is not searched.
        assert_eq!(scorer.score("wow!", 500, 20), 500);
        assert_eq!(scorer.metrics().fallback_budget, 1);
        // A regressing instant does not reopen the window.
        assert_eq!(scorer.score("wow!", 500, 5), 500);
        // The next logical window does.
        let _ = scorer.score("wow!", 500, 120);
        assert_eq!(scorer.metrics().retrieval_scored, 3);
    }

    #[test]
    fn config_bounds_are_validated() {
        for bad in [
            RetrievalScoreConfig {
                max_lookups_per_window: 0,
                ..Default::default()
            },
            RetrievalScoreConfig {
                max_lookups_per_window: MAX_SCORE_LOOKUPS_PER_WINDOW + 1,
                ..Default::default()
            },
            RetrievalScoreConfig {
                window_ms: 0,
                ..Default::default()
            },
            RetrievalScoreConfig {
                window_ms: MAX_SCORE_WINDOW_MS + 1,
                ..Default::default()
            },
        ] {
            assert!(bad.validate().is_err());
        }
    }
}
