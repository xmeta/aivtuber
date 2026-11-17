//! Issue #79 (S2): crowd-level reaction routing.
//!
//! S1 turns a repeated-topic cluster into one crowd candidate. S2 decides what
//! that candidate *means* for the performer, with deterministic policy in code:
//!
//! * a **reaction family** (laughter, congratulation, GG, ...) maps to a
//!   code-owned [`CrowdReactionGroup`]; the composition root answers it with a
//!   cached or non-verbal reaction from that group and never generates a reply
//!   to the carrier's text;
//! * a **repeated question** gets one representative reply on the normal reply
//!   path, with a typed [`CrowdSummary`] in its context;
//! * everything else (a repeated remark that is not a question) is unchanged.
//!
//! A bounded [`CrowdWindow`] remembers recent crowd classes across batches on
//! the engine's logical clock. It lets a persisting class raise its intensity
//! and trend, and it holds a per-group cooldown so one celebration that spans
//! several batches yields one reaction, not one per batch.
//!
//! Load-bearing properties:
//!
//! 1. **No new absorption path.** Routing reads clusters the S1 guard and
//!    threshold already produced; it never groups anything itself.
//! 2. **No text, no actor ids.** The window retains a class, a batch id, a
//!    logical instant, a count and (for a question) a fixed-size hash. The
//!    summary handed to a reply context is closed vocabulary plus counts.
//! 3. **Logical time only.** Every instant is the batch's own flush instant.
//!    A clock that goes backwards cannot reopen a cooldown (arithmetic
//!    saturates).
//! 4. **Bounded.** At most `max_entries` ring entries, at most one cooldown
//!    stamp per group (a closed set of five).
//! 5. **Data, not authority.** Nothing here touches the operator or control
//!    plane, a capability, a permit or the content guard order.

use crate::audience::ReactionFamily;
use aivtuber_domain::{
    CROWD_CONTEXT_SCHEMA_VERSION, CrowdClass, CrowdSummary, CrowdTrend, MAX_CROWD_WINDOW_BATCHES,
};
use serde::Serialize;
use std::collections::{BTreeMap, VecDeque};

/// Intensity added per additional recent batch in which a class appeared.
const PERSISTENCE_INTENSITY_STEP_MILLI: u16 = 100;
/// Hard ceiling on retained window entries.
pub const MAX_CROWD_WINDOW_ENTRIES: usize = MAX_CROWD_WINDOW_BATCHES as usize;
/// Hard ceiling on the rolling window and cooldown (one hour of logical time).
pub const MAX_CROWD_SPAN_MS: u64 = 3_600_000;

/// Operator-facing configuration of crowd routing. Validated, never clamped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CrowdConfig {
    /// How far back a persisting class is remembered.
    pub window_ms: u64,
    /// Minimum logical time between two reactions of the same group.
    pub cooldown_ms: u64,
    /// Most window entries retained (oldest evicted).
    pub max_entries: usize,
}

impl Default for CrowdConfig {
    fn default() -> Self {
        Self {
            window_ms: 60_000,
            cooldown_ms: 20_000,
            max_entries: 32,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CrowdConfigError {
    pub field: &'static str,
}

impl std::fmt::Display for CrowdConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid crowd routing configuration: {}", self.field)
    }
}

impl std::error::Error for CrowdConfigError {}

impl CrowdConfig {
    pub fn validate(&self) -> Result<(), CrowdConfigError> {
        if self.window_ms == 0 || self.window_ms > MAX_CROWD_SPAN_MS {
            return Err(CrowdConfigError { field: "window_ms" });
        }
        if self.cooldown_ms > MAX_CROWD_SPAN_MS {
            return Err(CrowdConfigError {
                field: "cooldown_ms",
            });
        }
        if self.max_entries == 0 || self.max_entries > MAX_CROWD_WINDOW_ENTRIES {
            return Err(CrowdConfigError {
                field: "max_entries",
            });
        }
        Ok(())
    }
}

/// Code-owned grouping of reaction families for routing and cooldown. Several
/// families are one crowd moment (GG, praise, congratulation and a cheer are
/// all "celebration"), so a mixed-token burst shares one cooldown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CrowdReactionGroup {
    Celebration,
    Laughter,
    Surprise,
    Sympathy,
    Greeting,
}

impl CrowdReactionGroup {
    /// Stable token for asset-group names and records.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Celebration => "celebration",
            Self::Laughter => "laughter",
            Self::Surprise => "surprise",
            Self::Sympathy => "sympathy",
            Self::Greeting => "greeting",
        }
    }
}

/// The reviewed family-to-group table.
pub fn reaction_group(family: ReactionFamily) -> CrowdReactionGroup {
    match family {
        ReactionFamily::GoodGame
        | ReactionFamily::Praise
        | ReactionFamily::Congratulation
        | ReactionFamily::Cheer
        | ReactionFamily::Thanks => CrowdReactionGroup::Celebration,
        ReactionFamily::Laughter => CrowdReactionGroup::Laughter,
        ReactionFamily::Surprise => CrowdReactionGroup::Surprise,
        ReactionFamily::Sympathy => CrowdReactionGroup::Sympathy,
        ReactionFamily::Greeting => CrowdReactionGroup::Greeting,
    }
}

/// The domain class of a reaction family.
pub fn crowd_class(family: ReactionFamily) -> CrowdClass {
    match family {
        ReactionFamily::Laughter => CrowdClass::Laughter,
        ReactionFamily::GoodGame => CrowdClass::GoodGame,
        ReactionFamily::Praise => CrowdClass::Praise,
        ReactionFamily::Congratulation => CrowdClass::Congratulation,
        ReactionFamily::Greeting => CrowdClass::Greeting,
        ReactionFamily::Cheer => CrowdClass::Cheer,
        ReactionFamily::Surprise => CrowdClass::Surprise,
        ReactionFamily::Sympathy => CrowdClass::Sympathy,
        ReactionFamily::Thanks => CrowdClass::Thanks,
    }
}

/// What the composition root should do with a crowd carrier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum CrowdAction {
    /// Prefer a cached or non-verbal reaction of this group; never generate a
    /// reply to the carrier's text. When no fitting asset exists the carrier
    /// takes the ordinary reply path (today's behaviour).
    NonVerbal { group: CrowdReactionGroup },
    /// One representative reply on the normal reply path, with the summary in
    /// its context.
    RepresentativeReply,
}

/// The routing decision for one cluster of a batch. Counts and closed
/// vocabulary only; `signal` indexes the record's audience signals.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CrowdRouting {
    pub signal: usize,
    pub action: CrowdAction,
    pub summary: CrowdSummary,
}

/// What identifies "the same crowd" across batches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum CrowdKey {
    Group(CrowdReactionGroup),
    /// Hash of the folded remark; no text is retained.
    Question(u64),
}

#[derive(Debug, Clone, Copy)]
struct Entry {
    key: CrowdKey,
    batch_id: u64,
    at_ms: u64,
    count: u32,
}

/// What the window says about a class that was just observed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Persistence {
    /// Recent batches (this one included) in which the class appeared.
    pub window_batches: u32,
    pub trend: CrowdTrend,
    pub intensity_milli: u16,
}

/// Bounded rolling window plus per-group cooldown. Logical time only.
#[derive(Debug, Clone)]
pub(crate) struct CrowdWindow {
    config: CrowdConfig,
    ring: VecDeque<Entry>,
    cooldowns: BTreeMap<CrowdReactionGroup, u64>,
    evicted: u64,
}

impl CrowdWindow {
    pub(crate) fn new(config: CrowdConfig) -> Self {
        Self {
            config,
            ring: VecDeque::new(),
            cooldowns: BTreeMap::new(),
            evicted: 0,
        }
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.ring.len()
    }

    /// Entries dropped because the ring was full (not by age).
    pub(crate) fn evicted(&self) -> u64 {
        self.evicted
    }

    /// Record one cluster of batch `batch_id` and report how persistent its
    /// class is. `base_trend` and `base_intensity_milli` are the batch's own.
    pub(crate) fn observe(
        &mut self,
        key: CrowdKey,
        batch_id: u64,
        at_ms: u64,
        count: u32,
        base_trend: CrowdTrend,
        base_intensity_milli: u16,
    ) -> Persistence {
        let horizon = at_ms.saturating_sub(self.config.window_ms);
        while self.ring.front().is_some_and(|entry| entry.at_ms < horizon) {
            self.ring.pop_front();
        }
        let mut batches = std::collections::BTreeSet::new();
        let mut previous: Option<&Entry> = None;
        for entry in self
            .ring
            .iter()
            .filter(|entry| entry.key == key && entry.batch_id != batch_id)
        {
            batches.insert(entry.batch_id);
            previous = Some(entry);
        }
        let prior_batches = batches.len() as u32;
        let trend = match previous {
            Some(entry) => match count.cmp(&entry.count) {
                std::cmp::Ordering::Greater => CrowdTrend::Rising,
                std::cmp::Ordering::Less => CrowdTrend::Falling,
                std::cmp::Ordering::Equal => CrowdTrend::Steady,
            },
            None => base_trend,
        };
        let raised = base_intensity_milli
            .saturating_add(PERSISTENCE_INTENSITY_STEP_MILLI.saturating_mul(prior_batches as u16));
        let persistence = Persistence {
            window_batches: (prior_batches + 1).min(MAX_CROWD_WINDOW_BATCHES),
            trend,
            intensity_milli: raised.min(1000),
        };
        if self.ring.len() >= self.config.max_entries {
            self.ring.pop_front();
            self.evicted += 1;
        }
        self.ring.push_back(Entry {
            key,
            batch_id,
            at_ms,
            count,
        });
        persistence
    }

    /// Whether `group` reacted too recently to react again.
    pub(crate) fn cooling(&self, group: CrowdReactionGroup, at_ms: u64) -> bool {
        self.config.cooldown_ms > 0
            && self
                .cooldowns
                .get(&group)
                .is_some_and(|&last| at_ms < last.saturating_add(self.config.cooldown_ms))
    }

    pub(crate) fn stamp(&mut self, group: CrowdReactionGroup, at_ms: u64) {
        let slot = self.cooldowns.entry(group).or_insert(0);
        *slot = (*slot).max(at_ms);
    }
}

/// Build the summary for one cluster.
pub(crate) fn summary_for(
    class: CrowdClass,
    count: u32,
    unique_actors_estimate: u32,
    persistence: Persistence,
) -> CrowdSummary {
    CrowdSummary {
        schema_version: CROWD_CONTEXT_SCHEMA_VERSION.to_owned(),
        class,
        member_count: count,
        unique_actors_estimate: unique_actors_estimate.min(count).max(1),
        intensity_milli: persistence.intensity_milli,
        trend: persistence.trend,
        window_batches: persistence.window_batches,
    }
}

/// Map the S1 trend into the domain's.
pub(crate) fn domain_trend(trend: crate::audience::AudienceTrend) -> CrowdTrend {
    use crate::audience::AudienceTrend;
    match trend {
        AudienceTrend::Rising => CrowdTrend::Rising,
        AudienceTrend::Steady => CrowdTrend::Steady,
        AudienceTrend::Falling => CrowdTrend::Falling,
        AudienceTrend::Unknown => CrowdTrend::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(max_entries: usize) -> CrowdWindow {
        CrowdWindow::new(CrowdConfig {
            window_ms: 10_000,
            cooldown_ms: 5_000,
            max_entries,
        })
    }

    const CELEBRATION: CrowdKey = CrowdKey::Group(CrowdReactionGroup::Celebration);

    #[test]
    fn a_first_observation_keeps_the_batch_values() {
        let mut w = window(8);
        let p = w.observe(CELEBRATION, 1, 100, 30, CrowdTrend::Rising, 400);
        assert_eq!(
            p,
            Persistence {
                window_batches: 1,
                trend: CrowdTrend::Rising,
                intensity_milli: 400
            }
        );
    }

    #[test]
    fn a_persisting_class_raises_intensity_and_reads_the_trend_across_batches() {
        let mut w = window(8);
        w.observe(CELEBRATION, 1, 100, 30, CrowdTrend::Steady, 400);
        let second = w.observe(CELEBRATION, 2, 1_100, 50, CrowdTrend::Steady, 400);
        assert_eq!(second.window_batches, 2);
        assert_eq!(second.trend, CrowdTrend::Rising);
        assert_eq!(second.intensity_milli, 500);
        let third = w.observe(CELEBRATION, 3, 2_100, 10, CrowdTrend::Steady, 950);
        assert_eq!(third.window_batches, 3);
        assert_eq!(third.trend, CrowdTrend::Falling);
        assert_eq!(third.intensity_milli, 1000, "intensity is capped");
    }

    #[test]
    fn two_clusters_of_one_batch_do_not_count_as_persistence() {
        let mut w = window(8);
        w.observe(CELEBRATION, 1, 100, 30, CrowdTrend::Steady, 400);
        let again = w.observe(CELEBRATION, 1, 100, 20, CrowdTrend::Steady, 400);
        assert_eq!(again.window_batches, 1);
    }

    #[test]
    fn entries_expire_by_logical_time_and_the_ring_is_bounded() {
        let mut w = window(3);
        w.observe(CELEBRATION, 1, 0, 5, CrowdTrend::Steady, 100);
        let late = w.observe(CELEBRATION, 2, 20_000, 5, CrowdTrend::Steady, 100);
        assert_eq!(late.window_batches, 1, "the first batch left the window");
        for batch in 3..40 {
            w.observe(
                CrowdKey::Question(batch),
                batch,
                20_000,
                3,
                CrowdTrend::Unknown,
                100,
            );
        }
        assert_eq!(w.len(), 3);
        assert!(w.evicted() > 0);
    }

    #[test]
    fn a_question_hash_separates_different_questions() {
        let mut w = window(8);
        w.observe(CrowdKey::Question(1), 1, 0, 4, CrowdTrend::Steady, 100);
        let other = w.observe(CrowdKey::Question(2), 2, 10, 4, CrowdTrend::Steady, 100);
        assert_eq!(other.window_batches, 1);
        let same = w.observe(CrowdKey::Question(1), 3, 20, 4, CrowdTrend::Steady, 100);
        assert_eq!(same.window_batches, 2);
    }

    #[test]
    fn cooldown_is_per_group_and_a_regressing_clock_cannot_reopen_it() {
        let mut w = window(8);
        assert!(!w.cooling(CrowdReactionGroup::Celebration, 1_000));
        w.stamp(CrowdReactionGroup::Celebration, 1_000);
        assert!(w.cooling(CrowdReactionGroup::Celebration, 1_001));
        assert!(w.cooling(CrowdReactionGroup::Celebration, 0), "regression");
        assert!(!w.cooling(CrowdReactionGroup::Laughter, 1_001));
        assert!(!w.cooling(CrowdReactionGroup::Celebration, 6_000));
    }

    #[test]
    fn the_family_table_is_total_and_groups_celebrations() {
        for family in [
            ReactionFamily::GoodGame,
            ReactionFamily::Praise,
            ReactionFamily::Congratulation,
            ReactionFamily::Cheer,
            ReactionFamily::Thanks,
        ] {
            assert_eq!(reaction_group(family), CrowdReactionGroup::Celebration);
        }
        assert_eq!(
            reaction_group(ReactionFamily::Laughter),
            CrowdReactionGroup::Laughter
        );
    }

    #[test]
    fn config_validation_refuses_out_of_bounds_fields() {
        assert!(CrowdConfig::default().validate().is_ok());
        for bad in [
            CrowdConfig {
                window_ms: 0,
                ..CrowdConfig::default()
            },
            CrowdConfig {
                cooldown_ms: MAX_CROWD_SPAN_MS + 1,
                ..CrowdConfig::default()
            },
            CrowdConfig {
                max_entries: 0,
                ..CrowdConfig::default()
            },
            CrowdConfig {
                max_entries: MAX_CROWD_WINDOW_ENTRIES + 1,
                ..CrowdConfig::default()
            },
        ] {
            assert!(bad.validate().is_err());
        }
    }
}
