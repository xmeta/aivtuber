use super::*;

/// Target-free latency evidence pooled across a run history.
///
/// The artifact carries per-run percentiles, not raw samples, so a pooled
/// percentile cannot be reconstructed. A threshold chosen for a latency
/// objective has to hold for *every* run in the history, so the worst observed
/// percentile is the conservative evidence rather than the median run's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BaselineLatency {
    pub field: String,
    pub runs: u64,
    pub samples: u64,
    pub p95_ms: u64,
    pub p99_ms: u64,
    /// Worst observed maximum across contributing runs, or `None` when the
    /// history carried no per-run maximum (a #58 aggregate row). Rendered `-`
    /// rather than a fabricated zero.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_ms: Option<u64>,
}

/// Pooled calibration evidence for one indicator over a history of runs.
///
/// This is deliberately *not* a target. It publishes the measured evidence a
/// target is chosen from, in the `BaselineEvidence` shape a target entry has to
/// cite, so choosing a target is a documented product decision above the
/// measurement rather than a number the fixture happened to produce.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BaselineProposal {
    pub indicator: String,
    /// Window series pooled. Always `session`: stream-hour and rolling windows
    /// partition the same run, so pooling them would count every event once per
    /// window it appears in.
    pub window: String,
    /// Number of contributing runs that carried eligible evidence.
    pub runs: u64,
    /// Pooled session denominator (ratio objectives only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eligible: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conforming: Option<u64>,
    /// For a latency objective whose ratio was *measured* at a candidate
    /// boundary (the probe step), the threshold in ms the pooled ratio was
    /// computed at. A ratio is only comparable within one boundary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub threshold_ms: Option<u64>,
    /// Pooled measured ratio, ready to paste into a target entry as its
    /// `baseline`. The target ratio itself remains a product decision made
    /// *against* this value — a minimum conforming ratio, chosen never above
    /// the measured baseline — not equal to it by default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline: Option<BaselineEvidence>,
    /// Target-free latency percentiles; a `threshold_ms` is chosen from these.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latency: Option<BaselineLatency>,
}

/// Internal accumulator for pooled latency percentiles across contributing runs.
#[derive(Default)]
pub(super) struct LatencyPool {
    runs: u64,
    samples: u64,
    p95_ms: u64,
    p99_ms: u64,
    max_ms: Option<u64>,
}

/// Internal accumulator for one indicator's pooled denominators plus the
/// provenance of exactly the runs that produced them.
///
/// Provenance is tracked per indicator, not per history: a run with an empty
/// denominator for an indicator contributed no evidence for it, so listing it
/// would let one measured run satisfy the repeated-run floor.
#[derive(Default)]
pub(super) struct RatioPool {
    runs: u64,
    eligible: u64,
    conforming: u64,
    contributors: Vec<BaselineRun>,
    represented_ms: u64,
    without_duration: u64,
}

impl RatioPool {
    fn add(&mut self, run: BaselineRun, stream_duration_ms: Option<u64>, row: &SloIndicatorResult) {
        self.runs += 1;
        self.eligible = self.eligible.saturating_add(row.eligible);
        self.conforming = self.conforming.saturating_add(row.conforming);
        match stream_duration_ms {
            Some(ms) => self.represented_ms = self.represented_ms.saturating_add(ms),
            None => self.without_duration += 1,
        }
        self.contributors.push(run);
    }
}

/// Calibration evidence pooled from a history of SLO reports (#58 history).
///
/// This is the missing link the issue names: repeated measured runs, not a
/// single one, are what turn a measurement into a baseline. It reads the
/// per-run SLO reports the project already produces and pools them, so a target
/// can be chosen from evidence instead of invented.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BaselineProposalSet {
    pub schema_version: String,
    pub catalog_version: String,
    pub reports: u64,
    pub contributing_reports: u64,
    /// Input reports that repeated a run identity already in the history with
    /// identical content: the same artifact passed twice, or a byte-identical
    /// retry. They carry no second measurement. A repeat of a run identity with
    /// *different* content is refused instead of pooled.
    #[serde(default)]
    pub duplicate_reports: u64,
    pub excluded_experimental_reports: u64,
    /// The #58 compatibility series every contributing run belongs to:
    /// `mode|dataset_id|config_version|seed`. Pooled evidence is only valid
    /// within one series; the field names it so the boundary is auditable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub series: Option<String>,
    pub revisions: Vec<String>,
    pub datasets: Vec<String>,
    /// Identity of every contributing run, sorted by run id. The same manifest
    /// is embedded in each proposal's [`BaselineEvidence`], so it survives the
    /// copy into a target.
    #[serde(default)]
    pub contributing_runs: Vec<BaselineRun>,
    pub proposals: BTreeMap<String, BaselineProposal>,
    /// Objectives with no eligible evidence in any contributing run.
    pub not_measured: Vec<String>,
    pub limitations: Vec<String>,
}

/// Verify that a full report and an aggregate-history shell describe one
/// measured run before the pair collapses into the full report (the rule
/// [`BaselineProposalSet::from_reports`] applies to complementary artifacts).
///
/// The pair may pool as the full report only when everything the two
/// artifacts *share* agrees: the revision, compatibility series, and
/// represented duration they carry, and every percentile the shell holds.
/// Anything less and the two artifacts could be different measurements under
/// one identity, which fails closed exactly like any other conflicting repeat.
pub(super) fn collapse_into_full(full: &SloReport, shell: &SloReport) -> Result<(), SloError> {
    let run = shell.source.run_id.as_deref().unwrap_or("<none>");
    if full.source.git_commit != shell.source.git_commit {
        return Err(SloError::new(format!(
            "run {run:?} appears as a full report from revision {:?} and an aggregate shell \
             from revision {:?}; one run has one revision, so the two artifacts cannot describe \
             the same measurement",
            full.source.git_commit, shell.source.git_commit
        )));
    }
    if full.source.stream_duration_ms != shell.source.stream_duration_ms {
        return Err(SloError::new(format!(
            "run {run:?} appears as a full report and an aggregate shell that disagree on \
             stream_duration_ms ({:?} vs {:?}); the represented duration is part of what the run \
             measured, so the pair is not verified as one measurement",
            full.source.stream_duration_ms, shell.source.stream_duration_ms
        )));
    }
    if full.source.mode.as_str() != shell.source.mode.as_str()
        || full.source.dataset_id != shell.source.dataset_id
        || full.source.config_version != shell.source.config_version
        || full.source.seed != shell.source.seed
    {
        return Err(SloError::new(format!(
            "run {run:?} appears in more than one compatibility series across its full report \
             and aggregate shell; one run belongs to exactly one series"
        )));
    }
    for point in &shell.latency_calibration {
        let mate = full
            .latency_calibration
            .iter()
            .find(|candidate| {
                candidate.indicator == point.indicator
                    && candidate.window == point.window
                    && candidate.field == point.field
            })
            .ok_or_else(|| {
                SloError::new(format!(
                    "the aggregate shell of run {run:?} carries {} / {} percentiles the full \
                     report does not record, so the pair cannot be verified as one measurement",
                    point.indicator, point.window
                ))
            })?;
        if (mate.samples, mate.p50_ms, mate.p95_ms, mate.p99_ms)
            != (point.samples, point.p50_ms, point.p95_ms, point.p99_ms)
        {
            return Err(SloError::new(format!(
                "the aggregate shell and the full report of run {run:?} disagree on {} / {} \
                 percentiles (shell: p50={} p95={} p99={} over {} samples, full report: p50={} \
                 p95={} p99={} over {} samples); two artifacts of one run must measure the same \
                 numbers",
                point.indicator,
                point.window,
                point.p50_ms,
                point.p95_ms,
                point.p99_ms,
                point.samples,
                mate.p50_ms,
                mate.p95_ms,
                mate.p99_ms,
                mate.samples
            )));
        }
    }
    Ok(())
}

impl BaselineProposalSet {
    /// Pool a history of SLO reports into per-indicator calibration evidence.
    ///
    /// The rules a history must satisfy, each enforced rather than assumed:
    ///
    /// * **One compatibility series.** Reports from different
    ///   `mode + dataset_id + config_version + seed` series are the boundary
    ///   docs/performance-goals.adoc[#58] defines and are refused, not merged:
    ///   values written against a different workload/configuration are not
    ///   evidence about this one.
    /// * **One report contract.** A `schema_version` this build does not emit
    ///   fails closed, exactly like a target file for another contract.
    /// * **Distinct runs by identity, never by payload.** A run is identified
    ///   by [`SloSource::run_id`], the #58 `recording.run_id` semantics: two
    ///   independent deterministic runs of one revision produce byte-identical
    ///   reports and are two measurements, while a retry of one run is one
    ///   measurement however its numbers came out. A report with no run
    ///   identity is refused, because content cannot be used to infer one.
    ///   Repeated artifacts for one run identity collapse to that run when they
    ///   agree; when they *disagree* the history fails closed, because `run_id`
    ///   names the run but not which attempt is newer, and argument order is not
    ///   provenance. Fewer than [`MIN_BASELINE_RUNS`] distinct runs produce no
    ///   proposals at all, since one run calibrates nothing.
    /// * **Complementary views of one run collapse; conflicts never do.**
    ///   During the migration to durable full reports (#224) the same run can
    ///   arrive as both a full report and the percentile-only shell built
    ///   from its #58 aggregate row. The two collapse to the full report —
    ///   the run counts once, argument order irrelevant — only after their
    ///   shared identity (revision, series, and represented duration) and
    ///   every percentile the shell carries are verified to agree with it.
    ///   Two conflicting full reports,
    ///   two conflicting shells, or a pair failing that verification fail
    ///   closed exactly like any other conflicting repeat.
    /// * **Provenance per indicator, not per history.** A run whose denominator
    ///   is empty for one indicator contributed no evidence for it, so the
    ///   manifest and the coverage attached to that indicator's evidence name
    ///   exactly the runs that produced eligible observations for *it*. An
    ///   indicator pooled from fewer than [`MIN_BASELINE_RUNS`] such runs
    ///   publishes no [`BaselineEvidence`] at all, however many runs the
    ///   history contains — including a probed latency boundary, where only
    ///   runs with eligible samples at the candidate count. Publishing the
    ///   whole history's manifest there would let one measured run satisfy the
    ///   repeated-run floor the evidence exists to enforce.
    /// * **Exact time.** Represented time is summed exactly over an
    ///   indicator's contributing runs; a partial hour never rounds up to one.
    ///   Sub-hour evidence omits `stream_hours` instead of claiming an hour it
    ///   did not stand for.
    ///
    /// Only `active`-plane reports contribute: an experimental route must not
    /// move the active baseline any more than it may move the active SLO.
    pub fn from_reports(reports: &[SloReport]) -> Result<Self, SloError> {
        for report in reports {
            if report.schema_version != SLO_REPORT_SCHEMA_VERSION {
                return Err(SloError::new(format!(
                    "report for dataset {:?} declares schema {:?} but this build emits \
                     {:?}; pooling a report written for another contract fails closed",
                    report.source.dataset_id, report.schema_version, SLO_REPORT_SCHEMA_VERSION
                )));
            }
            if report.catalog_version != SLO_CATALOG_VERSION {
                return Err(SloError::new(format!(
                    "report for dataset {:?} was evaluated against catalog {:?} but this build \
                     implements {:?}; pooling values written against different indicator \
                     definitions is not comparable",
                    report.source.dataset_id, report.catalog_version, SLO_CATALOG_VERSION
                )));
            }
            match report.source.run_id.as_deref().map(str::trim) {
                Some(run_id) if !run_id.is_empty() => {}
                _ => {
                    return Err(SloError::new(format!(
                        "report for dataset {:?} carries no run identity, so it is a measurement \
                         but not calibration evidence; regenerate it with `slo-report --run-id \
                         <stable-id>` (#58 `recording.run_id` semantics: stable across retries of \
                         one run, distinct across distinct runs), because two independent runs can \
                         produce identical reports and content can never stand in for identity",
                        report.source.dataset_id
                    )));
                }
            }
        }

        // A series is #58's compatibility boundary. Everything pooled must sit
        // inside one, or the pooled value describes no workload at all.
        let series_key = |report: &SloReport| {
            compatibility_series_key(
                report.source.mode.as_str(),
                &report.source.dataset_id,
                &report.source.config_version,
                report.source.seed,
            )
        };
        let mut series: Option<String> = None;
        for report in reports {
            if !report.counts_toward_active_slo {
                continue;
            }
            let key = series_key(report);
            match &series {
                None => series = Some(key),
                Some(first) if *first == key => {}
                Some(first) => {
                    return Err(SloError::new(format!(
                        "the history spans more than one compatibility series ({first:?} vs \
                         {key:?}); a baseline is only valid within one \
                         mode+dataset_id+config_version+seed series, so pool one series at a time"
                    )));
                }
            }
        }

        // Distinct-run detection is by run identity, never by payload. Two
        // independent deterministic runs of one revision produce identical
        // reports, and a retry of one run can produce a different one, so
        // content decides neither question. Content equality is used only to
        // recognise an identical repeat of one run; a *conflicting* repeat is
        // refused, never resolved (see below).
        let canonical_json = |report: &SloReport| -> Result<String, SloError> {
            serde_json::to_string(report)
                .map_err(|error| SloError::new(format!("canonicalize report: {error}")))
        };
        // run identity -> the accepted artifact for it: its canonical payload,
        // whether it is an aggregate shell, and where it sits in `distinct`.
        struct Accepted {
            canonical: String,
            shell: bool,
            distinct: usize,
        }
        let mut accepted: BTreeMap<String, Accepted> = BTreeMap::new();
        let mut distinct: Vec<&SloReport> = Vec::new();
        let mut duplicate_reports = 0_u64;
        let mut collapsed_shells = 0_u64;
        let mut excluded_experimental = 0_u64;
        for report in reports {
            if !report.counts_toward_active_slo {
                excluded_experimental += 1;
                continue;
            }
            let run_id = report
                .source
                .run_id
                .as_deref()
                .expect("every report's run identity was required above")
                .trim()
                .to_owned();
            let canonical = canonical_json(report)?;
            let incoming_shell = report.is_aggregate_shell();
            match accepted.entry(run_id) {
                Entry::Occupied(mut occupied) => {
                    // The same run identity with identical content is one run
                    // measured twice: a double-passed file or a byte-identical
                    // retry, which carries no second measurement.
                    if occupied.get().canonical == canonical {
                        duplicate_reports += 1;
                        continue;
                    }
                    if occupied.get().shell == incoming_shell {
                        // Two attempts of one run disagree. `run_id` says they
                        // are the same logical run; it does not say which
                        // attempt is newer, and the order in which paths reach
                        // this function is not provenance. Choosing the last
                        // one would make durable calibration evidence a
                        // function of filesystem/glob/argv ordering, so
                        // pooling fails closed instead: the operator names the
                        // authoritative artifact explicitly.
                        return Err(SloError::new(format!(
                            "run identity {:?} appears twice with different measurements; the \
                             two artifacts are attempts of one run and carry no authoritative \
                             ordering, so choosing one would make the baseline depend on the \
                             order the files were passed — pool exactly one artifact per run \
                             identity, or give each attempt its own --run-id",
                            occupied.key()
                        )));
                    }
                    // One full report and one shell of the same run: two
                    // complementary views of a single measurement, not two
                    // attempts. They collapse to the full report only once
                    // everything they share is verified to agree; a mismatch
                    // fails closed like any other conflicting repeat.
                    let prior: &SloReport = distinct[occupied.get().distinct];
                    let (shell, full) = if occupied.get().shell {
                        (prior, report)
                    } else {
                        (report, prior)
                    };
                    collapse_into_full(full, shell)?;
                    if occupied.get().shell {
                        // The full report is the richer artifact: it becomes
                        // the accepted one, whichever order the two arrived in.
                        occupied.get_mut().canonical = canonical;
                        occupied.get_mut().shell = false;
                        distinct[occupied.get().distinct] = report;
                    }
                    collapsed_shells += 1;
                }
                Entry::Vacant(vacant) => {
                    vacant.insert(Accepted {
                        canonical,
                        shell: incoming_shell,
                        distinct: distinct.len(),
                    });
                    distinct.push(report);
                }
            }
        }

        // Identity of every contributing run, so the durable evidence can name
        // *which* runs formed a baseline rather than merely how many were seen.
        let mut run_manifest: Vec<BaselineRun> = distinct
            .iter()
            .map(|report| BaselineRun {
                run_id: report
                    .source
                    .run_id
                    .clone()
                    .expect("run identities were required above"),
                git_commit: report.source.git_commit.clone(),
            })
            .collect();
        run_manifest.sort_by(|left, right| left.run_id.cmp(&right.run_id));

        let mut limitations = vec![
            "baselines pool the `session` window only: stream-hour and rolling windows partition \
             the same run, so pooling them would count each event once per window"
                .to_owned(),
            "latency percentiles are the worst observed across contributing runs, not a pooled \
             percentile, because the artifact carries per-run percentiles rather than raw samples"
                .to_owned(),
        ];
        if duplicate_reports > 0 {
            limitations.push(format!(
                "{duplicate_reports} report(s) repeated a run identity already in the history with \
                 identical content and were not counted as further runs"
            ));
        }
        if collapsed_shells > 0 {
            limitations.push(format!(
                "{collapsed_shells} run(s) arrived as both a full report and an aggregate-history \
                 shell; the pair agreed on identity and percentiles, collapsed to the full \
                 report, and counted as one run"
            ));
        }
        if (distinct.len() as u64) < MIN_BASELINE_RUNS {
            limitations.push(format!(
                "only {} distinct active run(s) in the history; at least {MIN_BASELINE_RUNS} are \
                 required before any pooled value is calibration evidence, so none is published",
                distinct.len()
            ));
            return Ok(Self {
                schema_version: SLO_REPORT_SCHEMA_VERSION.to_owned(),
                catalog_version: SLO_CATALOG_VERSION.to_owned(),
                reports: reports.len() as u64,
                contributing_reports: distinct.len() as u64,
                duplicate_reports,
                excluded_experimental_reports: excluded_experimental,
                revisions: distinct
                    .iter()
                    .map(|report| report.source.git_commit.clone())
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect(),
                datasets: distinct
                    .iter()
                    .map(|report| report.source.dataset_id.clone())
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect(),
                series,
                contributing_runs: run_manifest,
                proposals: BTreeMap::new(),
                not_measured: Vec::new(),
                limitations,
            });
        }

        let catalog = catalog();
        let latency_ids: BTreeSet<&str> = catalog
            .iter()
            .filter(|indicator| indicator.latency_thresholded)
            .map(|indicator| indicator.id)
            .collect();

        let mut datasets: BTreeSet<&str> = BTreeSet::new();
        let mut revisions: BTreeSet<&str> = BTreeSet::new();
        let mut config_versions: BTreeSet<&str> = BTreeSet::new();
        let mut modes: BTreeSet<&str> = BTreeSet::new();
        let mut seeds: BTreeSet<u64> = BTreeSet::new();
        // Represented stream time, summed exactly. No per-run ceil: ten
        // two-second runs are twenty seconds of evidence, not ten hours.
        let mut represented_ms = 0_u64;
        let mut reports_without_duration = 0_u64;

        // indicator -> pooled denominators and the runs that produced them
        let mut pools: BTreeMap<&str, RatioPool> = BTreeMap::new();
        // (indicator, field) -> pooled percentile evidence
        let mut latency: BTreeMap<(&str, &str), LatencyPool> = BTreeMap::new();
        // Latency indicator -> the single candidate boundary every contributing
        // run was probed at. Ratios measured at different boundaries are not
        // poolable, so a mixed history is refused.
        let mut latency_thresholds: BTreeMap<&str, u64> = BTreeMap::new();

        for report in &distinct {
            datasets.insert(report.source.dataset_id.as_str());
            revisions.insert(report.source.git_commit.as_str());
            config_versions.insert(report.source.config_version.as_str());
            modes.insert(report.source.mode.as_str());
            seeds.insert(report.source.seed);
            match report.source.stream_duration_ms {
                Some(ms) => represented_ms = represented_ms.saturating_add(ms),
                None => reports_without_duration += 1,
            }
            // The identity of the run this row came from, so evidence can be
            // attributed to the run that actually produced it.
            let contributor = BaselineRun {
                run_id: report
                    .source
                    .run_id
                    .clone()
                    .expect("every report's run identity was required above"),
                git_commit: report.source.git_commit.clone(),
            };
            let stream_duration_ms = report.source.stream_duration_ms;

            for row in report
                .indicators
                .iter()
                .filter(|row| row.window == "session")
            {
                // Invariants are not probabilistic and never carry a ratio.
                if row.kind != ObjectiveKind::Slo || row.unit != IndicatorUnit::Ratio {
                    continue;
                }
                if latency_ids.contains(row.id.as_str()) {
                    // A latency objective's ratio depends on a candidate
                    // boundary. Without one (the target-free report) its
                    // evidence is the percentile; with one (the probe step)
                    // the measured ratio pools like any other. Ratios probed
                    // at two different boundaries are never pooled.
                    if let (Some(threshold_ms), true) = (row.threshold_ms, row.eligible > 0) {
                        match latency_thresholds.get(row.id.as_str()) {
                            Some(existing) if *existing != threshold_ms => {
                                return Err(SloError::new(format!(
                                    "latency indicator {:?} was probed at {} ms and {} ms in the \
                                     same history; ratios measured at different boundaries are not \
                                     poolable",
                                    row.id, existing, threshold_ms
                                )));
                            }
                            Some(_) => {}
                            None => {
                                latency_thresholds.insert(row.id.as_str(), threshold_ms);
                            }
                        }
                        pools.entry(row.id.as_str()).or_default().add(
                            contributor.clone(),
                            stream_duration_ms,
                            row,
                        );
                    }
                    continue;
                }
                if row.eligible == 0 {
                    continue;
                }
                pools.entry(row.id.as_str()).or_default().add(
                    contributor.clone(),
                    stream_duration_ms,
                    row,
                );
            }

            for point in report
                .latency_calibration
                .iter()
                .filter(|point| point.window == "session" && point.samples > 0)
            {
                let entry = latency
                    .entry((point.indicator.as_str(), point.field.as_str()))
                    .or_default();
                entry.runs += 1;
                entry.samples += point.samples;
                entry.p95_ms = entry.p95_ms.max(point.p95_ms);
                entry.p99_ms = entry.p99_ms.max(point.p99_ms);
                if let Some(max_ms) = point.max_ms {
                    entry.max_ms = Some(entry.max_ms.map_or(max_ms, |current| current.max(max_ms)));
                }
            }
        }

        // A boundary change restarts the series (docs/performance-goals.adoc),
        // so every identity field in the series key must be singular.
        debug_assert_eq!(modes.len(), 1);
        debug_assert_eq!(config_versions.len(), 1);
        debug_assert_eq!(seeds.len(), 1);
        let series = series.expect("distinct non-empty history has a series");
        // The evidence one indicator's pool is worth, if it is worth publishing at
        // all. It names the runs that actually contributed eligible
        // observations — not the whole history — because that is the set the
        // value was measured from and the set a reviewer has to be able to
        // re-measure. The same identities travel structurally in
        // [`BaselineEvidence::runs`] and [`BaselineEvidence::series`]; the prose
        // is for a reader. `datasets`/`config`/`mode`/`seed` stay series-wide
        // because `dataset_id` is part of the series key, so they are singular
        // for any subset of the contributing runs.
        let evidence_for = |pool: &RatioPool, boundary: Option<u64>| -> Option<BaselineEvidence> {
            // One run is a data point. An indicator measured in fewer than
            // `MIN_BASELINE_RUNS` runs is one, whatever else the history
            // measured, so it never becomes target-ready evidence.
            if pool.runs < MIN_BASELINE_RUNS {
                return None;
            }
            let revisions = pool
                .contributors
                .iter()
                .map(|run| run.git_commit.as_str())
                .collect::<BTreeSet<_>>();
            let mut source = format!(
                "slo-baseline: {} distinct session run(s) [{}]; series={series}; datasets={}; \
                 revisions={}; config={}; mode={}; seed={}",
                pool.runs,
                // `join_sorted` re-sorts, which is harmless: run ids are unique.
                join_sorted(pool.contributors.iter().map(|run| run.run_id.as_str())),
                join_sorted(datasets.iter()),
                join_sorted(revisions.iter()),
                join_sorted(config_versions.iter()),
                join_sorted(modes.iter()),
                join_sorted(seeds.iter()),
            );
            if let Some(threshold_ms) = boundary {
                source.push_str(&format!("; probe_threshold_ms={threshold_ms}"));
            }
            // Coverage is exactly the contributing runs' represented time: a
            // partial hour is evidence of a partial hour, and evidence
            // covering less than a full hour omits `stream_hours` rather than
            // claiming one.
            let whole_hours = pool.represented_ms / STREAM_HOUR_MS;
            Some(BaselineEvidence {
                value: pool.conforming as f64 / pool.eligible as f64,
                source,
                stream_hours: (whole_hours > 0).then_some(whole_hours),
                series: Some(series.clone()),
                runs: pool.contributors.clone(),
                eligible: Some(pool.eligible),
            })
        };
        // History-wide represented time, retained for the limitation below;
        // each indicator's own coverage comes from its contributing runs.
        let history_whole_hours = represented_ms / STREAM_HOUR_MS;
        let mut proposals: BTreeMap<String, BaselineProposal> = BTreeMap::new();
        for (id, pool) in &pools {
            proposals.insert(
                (*id).to_owned(),
                BaselineProposal {
                    indicator: (*id).to_owned(),
                    window: "session".to_owned(),
                    runs: pool.runs,
                    threshold_ms: None,
                    eligible: Some(pool.eligible),
                    conforming: Some(pool.conforming),
                    baseline: evidence_for(pool, None),
                    latency: None,
                },
            );
        }
        for ((id, field), pool) in &latency {
            let proposal = proposals
                .entry((*id).to_owned())
                .or_insert_with(|| BaselineProposal {
                    indicator: (*id).to_owned(),
                    window: "session".to_owned(),
                    runs: 0,
                    threshold_ms: None,
                    eligible: None,
                    conforming: None,
                    baseline: None,
                    latency: None,
                });
            proposal.runs = proposal.runs.max(pool.runs);
            // The probe step closes the calibration cycle: with a single
            // candidate boundary across the history, the measured ratio is a
            // valid `BaselineEvidence` and the first latency target can be
            // calibrated without inventing anything.
            if let Some(threshold_ms) = latency_thresholds.get(id) {
                proposal.threshold_ms = Some(*threshold_ms);
                if let Some(pool) = pools.get(id) {
                    // Only runs that had eligible latency samples at the
                    // candidate boundary count toward the calibration floor,
                    // exactly as for any other indicator.
                    proposal.eligible = Some(pool.eligible);
                    proposal.conforming = Some(pool.conforming);
                    proposal.baseline = evidence_for(pool, Some(*threshold_ms));
                }
            }
            proposal.latency = Some(BaselineLatency {
                field: (*field).to_owned(),
                runs: pool.runs,
                samples: pool.samples,
                p95_ms: pool.p95_ms,
                p99_ms: pool.p99_ms,
                max_ms: pool.max_ms,
            });
        }

        let not_measured: Vec<String> = catalog
            .iter()
            .filter(|indicator| {
                indicator.kind == ObjectiveKind::Slo
                    && indicator.evidence == EvidenceSource::Measured
            })
            .filter(|indicator| !proposals.contains_key(indicator.id))
            .map(|indicator| indicator.id.to_owned())
            .collect();

        if reports_without_duration > 0 {
            limitations.push(format!(
                "{reports_without_duration} contributing report(s) recorded no stream duration, so \
                 the represented time may be under-counted"
            ));
        }
        if excluded_experimental > 0 {
            limitations.push(format!(
                "{excluded_experimental} experimental-plane report(s) were excluded from the \
                 active baseline"
            ));
        }
        if history_whole_hours == 0 {
            limitations.push(format!(
                "the history represents {represented_ms} ms of logical stream time, less than one \
                 full stream hour, so `stream_hours` is omitted rather than rounded up"
            ));
        }
        // An indicator measured in the history but pooled from too few runs is
        // named, not silently dropped and not published as calibration
        // evidence: the repeated-run floor is about *this* indicator.
        let under_sampled = pools
            .iter()
            .filter(|(_, pool)| pool.runs < MIN_BASELINE_RUNS)
            .map(|(id, pool)| format!("{id} ({})", pool.runs))
            .collect::<Vec<_>>();
        if !under_sampled.is_empty() {
            limitations.push(format!(
                "{} indicator(s) were measured but pooled from fewer than {MIN_BASELINE_RUNS} \
                 contributing run(s), so no target-ready baseline is published for them: {}; a run \
                 with an empty denominator for an indicator produced no evidence for it",
                under_sampled.len(),
                under_sampled.join(", ")
            ));
        }
        // A pool that clears the repeated-run floor but not the indicator's
        // sample floor still publishes its evidence — with the denominator
        // attached — and `SloTargets::validate` refuses any target citing it.
        // The shortfall is named here so the operator sees it before copying.
        let under_floor = pools
            .iter()
            .filter(|(id, pool)| {
                pool.runs >= MIN_BASELINE_RUNS
                    && catalog
                        .iter()
                        .find(|indicator| indicator.id == **id)
                        .is_some_and(|indicator| pool.eligible < indicator.sample_floor)
            })
            .map(|(id, pool)| {
                let floor = catalog
                    .iter()
                    .find(|indicator| indicator.id == *id)
                    .map(|indicator| indicator.sample_floor)
                    .unwrap_or(0);
                format!("{id} ({}/{floor})", pool.eligible)
            })
            .collect::<Vec<_>>();
        if !under_floor.is_empty() {
            limitations.push(format!(
                "{} indicator(s) pooled below their sample floor, so validation refuses any \
                 target citing them rather than calibrating from evidence too thin to decide: \
                 {}",
                under_floor.len(),
                under_floor.join(", ")
            ));
        }
        let partial_duration = pools
            .iter()
            .filter(|(_, pool)| pool.without_duration > 0)
            .map(|(id, pool)| format!("{id} ({}/{})", pool.without_duration, pool.runs))
            .collect::<Vec<_>>();
        if !partial_duration.is_empty() {
            limitations.push(format!(
                "for some indicators a contributing run recorded no stream duration, so their \
                 coverage may be under-counted (run(s) missing a duration per indicator): {}",
                partial_duration.join(", ")
            ));
        }

        Ok(Self {
            schema_version: SLO_REPORT_SCHEMA_VERSION.to_owned(),
            catalog_version: SLO_CATALOG_VERSION.to_owned(),
            reports: reports.len() as u64,
            contributing_reports: distinct.len() as u64,
            duplicate_reports,
            excluded_experimental_reports: excluded_experimental,
            revisions: revisions.into_iter().map(ToOwned::to_owned).collect(),
            datasets: datasets.into_iter().map(ToOwned::to_owned).collect(),
            series: Some(series),
            contributing_runs: run_manifest,
            proposals,
            not_measured,
            limitations,
        })
    }

    pub fn to_json_pretty(&self) -> Result<Vec<u8>, SloError> {
        serde_json::to_vec_pretty(self)
            .map_err(|error| SloError::new(format!("serialize baseline proposal: {error}")))
    }

    pub fn markdown_summary(&self) -> String {
        let mut out = format!(
            "Operational SLO baseline proposal — {} report(s) pooled ({} contributing distinct, {} \
             duplicate, {} experimental excluded)\n\n",
            self.reports,
            self.contributing_reports,
            self.duplicate_reports,
            self.excluded_experimental_reports
        );
        if let Some(series) = &self.series {
            out.push_str(&format!("Series: `{series}`\n\n"));
        }
        if !self.contributing_runs.is_empty() {
            out.push_str(&format!(
                "Runs: {}\n\n",
                self.contributing_runs
                    .iter()
                    .map(|run| format!("`{}`@{}", run.run_id, run.git_commit))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        out.push_str(
            "| Indicator | Runs | Eligible | Conforming | Value | Threshold ms | Stream hours | Latency p95/p99/max |\n\
             |---|---|---|---|---|---|---|---|\n",
        );
        for proposal in self.proposals.values() {
            let value = proposal
                .baseline
                .as_ref()
                .map(|baseline| format!("{:.4}", baseline.value))
                .unwrap_or_else(|| "-".to_owned());
            let threshold = proposal
                .threshold_ms
                .map(|threshold| threshold.to_string())
                .unwrap_or_else(|| "-".to_owned());
            let stream_hours = proposal
                .baseline
                .as_ref()
                .and_then(|baseline| baseline.stream_hours)
                .map(|hours| hours.to_string())
                .unwrap_or_else(|| "-".to_owned());
            let latency = proposal
                .latency
                .as_ref()
                .map(|latency| {
                    let max = latency
                        .max_ms
                        .map(|value| value.to_string())
                        .unwrap_or_else(|| "-".to_owned());
                    format!("{} / {} / {max}", latency.p95_ms, latency.p99_ms)
                })
                .unwrap_or_else(|| "-".to_owned());
            out.push_str(&format!(
                "| {} | {} | {} | {} | {} | {} | {} | {} |\n",
                proposal.indicator,
                proposal.runs,
                proposal
                    .eligible
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "-".to_owned()),
                proposal
                    .conforming
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "-".to_owned()),
                value,
                threshold,
                stream_hours,
                latency,
            ));
        }
        if !self.not_measured.is_empty() {
            out.push_str(&format!(
                "\nNot measured in any contributing run: {}\n",
                self.not_measured.join("; ")
            ));
        }
        for limitation in &self.limitations {
            out.push_str(&format!("\n> {limitation}\n"));
        }
        out.push_str(
            "\nThis is measured evidence, not a target. A target ratio is a product decision \
             chosen against the value shown — never above the measured conforming ratio — and \
             the chosen `baseline` block is copied from here.\n",
        );
        out
    }
}

/// Join a collection into a comma-separated list for provenance text.
pub(super) fn join_sorted<I>(values: I) -> String
where
    I: IntoIterator,
    I::Item: ToString,
{
    let mut items: Vec<String> = values.into_iter().map(|value| value.to_string()).collect();
    items.sort_unstable();
    items.dedup();
    items.join(",")
}

pub(super) fn status_for(
    indicator: &SloIndicator,
    measurement: &SloMeasurement,
    target: Option<&SloTarget>,
) -> SloStatus {
    if indicator.kind == ObjectiveKind::Invariant {
        // A zero-tolerance invariant needs no configured target: the tolerated
        // value is zero by definition.
        return match measurement.value {
            Some(value) if value > 0.0 => SloStatus::Missed,
            Some(_) => SloStatus::Met,
            None => SloStatus::NoData,
        };
    }
    if measurement.eligible == 0 {
        return SloStatus::NoData;
    }
    let Some(target) = target else {
        return SloStatus::Uncalibrated;
    };
    // A target alone is not a calibrated objective: if the classifier could not
    // score any sample, the objective is still unresolved. This is defensive —
    // `validate` already refuses a thresholdless latency target — but the rule
    // that an unscoreable objective is never a breach is enforced here rather
    // than inferred from catalog metadata elsewhere.
    if measurement.unscored > 0 {
        return SloStatus::Uncalibrated;
    }
    if measurement.eligible < indicator.sample_floor {
        return SloStatus::InsufficientSamples;
    }
    match measurement.value {
        Some(value) if value >= target.target => SloStatus::Met,
        Some(_) => SloStatus::Missed,
        // `value` is `None` only when nothing was eligible, already handled
        // above; treating it as a miss would make "unmeasured" mean "failed".
        None => SloStatus::Uncalibrated,
    }
}

/// Build the error budget for one probabilistic objective.
///
/// Invariants are excluded by construction: a stale post-stop dispatch or an
/// unauthorized privileged action has no allowance to spend.
pub(super) fn error_budget_for(
    indicator: &SloIndicator,
    window: &SloWindow,
    measurement: &SloMeasurement,
    target: Option<&SloTarget>,
    status: SloStatus,
) -> Option<ErrorBudget> {
    if indicator.kind == ObjectiveKind::Invariant || !status.is_decided() {
        return None;
    }
    let target = target?;
    let allowed_misses = ((1.0 - target.target) * measurement.eligible as f64).floor() as u64;
    let observed_misses = measurement.misses();
    Some(ErrorBudget {
        indicator: indicator.id.to_owned(),
        window: window.label.clone(),
        window_kind: window.kind,
        target: target.target,
        allowed_misses,
        observed_misses,
        remaining_misses: allowed_misses as i64 - observed_misses as i64,
        exhausted: observed_misses > allowed_misses,
    })
}

/// Misses that belong in an after-action review (#62).
///
/// A single noisy window is not an incident; a run of them is. Zero-tolerance
/// invariants bypass the persistence threshold because there is no acceptable
/// streak to wait for.
pub(super) fn aar_candidates(
    indicators: &[SloIndicatorResult],
    threshold: u64,
) -> Vec<AarCandidate> {
    let mut candidates = Vec::new();
    let mut series_keys: Vec<(&str, WindowKind)> = indicators
        .iter()
        .map(|result| (result.id.as_str(), result.window_kind))
        .collect();
    series_keys.sort_by(|a, b| a.0.cmp(b.0).then_with(|| a.1.as_str().cmp(b.1.as_str())));
    series_keys.dedup();

    for (id, window_kind) in series_keys {
        // Persistence is a property of one window *series*, not of the whole
        // indicator row. A session aggregate, an hourly bucket and a rolling
        // bucket are different observations of the same runtime, and letting
        // their misses flow into one counter would let a two-hour run
        // manufacture a three-hour streak. Rolling windows are keyed by their
        // span, so a 3h and a 6h window never share a streak either.
        let series: Vec<&SloIndicatorResult> = indicators
            .iter()
            .filter(|result| result.id == id && result.window_kind == window_kind)
            .collect();
        let windows = series.len() as u64;
        let mut run = 0_u64;
        let mut total = 0_u64;
        for result in series {
            if result.status == SloStatus::Missed {
                run += 1;
                total += 1;
            } else {
                run = 0;
            }
            let zero_tolerance = result.kind == ObjectiveKind::Invariant;
            if run == 0 || (!zero_tolerance && run < threshold) {
                continue;
            }
            let rationale = if zero_tolerance {
                format!(
                    "zero-tolerance invariant missed in {} of {} {} window(s); no error budget applies",
                    total,
                    windows,
                    window_kind.as_str(),
                )
            } else {
                format!(
                    "missed {} consecutive {} window(s) ({} of {} missed in total)",
                    run,
                    window_kind.as_str(),
                    total,
                    windows
                )
            };
            candidates.push(AarCandidate {
                indicator: id.to_owned(),
                window_kind,
                windows_missed: total,
                consecutive_miss_windows: run,
                recommended_action: result
                    .actions
                    .first()
                    .copied()
                    .unwrap_or(SloAction::OpenOptimizationIssue),
                rationale,
            });
        }
    }
    candidates
}

/// Build the reporting windows for one report.
///
/// Bucketing is over the logical stream offset recorded at admission. An
/// artifact that carries no offsets degrades to a single session window and
/// says so, rather than inventing hour boundaries from the event index.
pub(super) fn build_windows(
    report: &BenchmarkReport,
    config: &SloEvaluationConfig,
) -> Vec<SloWindow> {
    let events = &report.events;
    let observed_end = events
        .iter()
        .filter_map(|event| event.stream_offset_ms)
        .max()
        .map(|offset| offset.saturating_add(1))
        .unwrap_or(0);
    let mut windows = vec![SloWindow {
        kind: WindowKind::Session,
        label: "session".to_owned(),
        start_offset_ms: 0,
        end_offset_ms: report.metadata.stream_duration_ms.unwrap_or(observed_end),
        admitted_events: events.len() as u64,
    }];

    // Every observation needs an offset, or the buckets would silently drop
    // events and the per-hour denominators would disagree with the session.
    if events.is_empty() || events.iter().any(|event| event.stream_offset_ms.is_none()) {
        return windows;
    }
    // The represented stream is at least as long as the workload declares, even
    // if its last event arrived earlier. Bucketing on the last observation
    // alone would quietly truncate the tail of a long run and under-report how
    // many stream hours it actually stood for.
    let represented_ms = observed_end.max(report.metadata.stream_duration_ms.unwrap_or(0));
    if represented_ms <= STREAM_HOUR_MS {
        return windows;
    }
    let max_hour = represented_ms.saturating_sub(1) / STREAM_HOUR_MS;

    for hour in 0..=max_hour {
        let start = hour * STREAM_HOUR_MS;
        windows.push(SloWindow {
            kind: WindowKind::StreamHour,
            label: format!("stream-hour-{hour}"),
            start_offset_ms: start,
            end_offset_ms: start + STREAM_HOUR_MS,
            admitted_events: admitted_between(events, start, start + STREAM_HOUR_MS),
        });
    }

    let hours = max_hour + 1;
    let span = u64::from(config.rolling_window_hours.max(1));
    let mut start_hour = 0;
    while start_hour + span <= hours {
        let start = start_hour * STREAM_HOUR_MS;
        let end = (start_hour + span) * STREAM_HOUR_MS;
        windows.push(SloWindow {
            kind: WindowKind::RollingStreamHours {
                window_hours: config.rolling_window_hours,
            },
            label: format!("rolling-{span}h-from-hour-{start_hour}"),
            start_offset_ms: start,
            end_offset_ms: end,
            admitted_events: admitted_between(events, start, end),
        });
        start_hour += 1;
    }

    windows
}

pub(super) fn admitted_between(events: &[EventObservation], start: u64, end: u64) -> u64 {
    events
        .iter()
        .filter(|event| {
            event
                .stream_offset_ms
                .is_some_and(|offset| offset >= start && offset < end)
        })
        .count() as u64
}

pub(super) fn window_events<'a>(
    events: &'a [EventObservation],
    window: &SloWindow,
) -> Vec<&'a EventObservation> {
    if window.kind == WindowKind::Session {
        return events.iter().collect();
    }
    events
        .iter()
        .filter(|event| {
            event.stream_offset_ms.is_some_and(|offset| {
                offset >= window.start_offset_ms && offset < window.end_offset_ms
            })
        })
        .collect()
}

pub(super) fn latency_calibration_for(
    indicator: &str,
    events: &[EventObservation],
    windows: &[SloWindow],
) -> Vec<LatencyCalibration> {
    let (field, audio): (&str, bool) = if indicator.contains("audio") {
        ("event_to_first_audio_ms", true)
    } else {
        ("event_to_first_visible_reaction_ms", false)
    };
    windows
        .iter()
        .map(|window| {
            let mut values: Vec<u64> = window_events(events, window)
                .into_iter()
                .filter_map(|event| {
                    if audio {
                        event.event_to_first_audio_ms
                    } else {
                        event.event_to_first_visible_reaction_ms
                    }
                })
                .collect();
            values.sort_unstable();
            LatencyCalibration {
                indicator: indicator.to_owned(),
                window: window.label.clone(),
                field: field.to_owned(),
                samples: values.len() as u64,
                p50_ms: nearest_rank(&values, 50),
                p95_ms: nearest_rank(&values, 95),
                p99_ms: nearest_rank(&values, 99),
                max_ms: Some(values.last().copied().unwrap_or(0)),
            }
        })
        .collect()
}

pub(super) fn nearest_rank(sorted: &[u64], percentile: usize) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let rank = (percentile * sorted.len()).div_ceil(100).max(1);
    sorted[rank - 1]
}

pub(super) fn limitations(report: &BenchmarkReport, windows: &[SloWindow]) -> Vec<String> {
    let mut limitations = vec![
        "denominators are retained observations, not offered events: telemetry retention is \
         bounded, so a run that evicted observations reports the retained set"
            .to_owned(),
    ];
    if windows
        .iter()
        .all(|window| window.kind == WindowKind::Session)
    {
        let has_offsets = !report.events.is_empty()
            && report
                .events
                .iter()
                .all(|event| event.stream_offset_ms.is_some());
        limitations.push(if has_offsets {
            format!(
                "this run represents {} ms of logical stream time, less than one stream hour, so \
                 no stream-hour or rolling window exists to report",
                report
                    .events
                    .iter()
                    .filter_map(|event| event.stream_offset_ms)
                    .max()
                    .unwrap_or(0)
            )
        } else {
            "this artifact carries no per-event logical stream offsets, so only the session \
             window is available; stream-hour and rolling windows need the offset recorded at \
             admission"
                .to_owned()
        });
    }
    if report.summary.events != report.events.len() {
        limitations.push(format!(
            "the summary counted {} events while the report retains {}; the retained set was \
             used for every denominator",
            report.summary.events,
            report.events.len()
        ));
    }
    limitations
}
