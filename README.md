# slo-data

Durable operational SLO calibration history (issue #71) — the bridge from
#58's continuous measurement to SLO baseline selection.

Written only by trusted jobs (push to main) via `scripts/slo-record.mjs`;
`pull_request` jobs never receive credentials for this branch.

## Layout

Two parallel trees, sharing one contract/series/run identity:

- `reports/<schema>/<catalog>/<series>/<run>.json` — the `slo-report`
  document. This is the tree `slo-baseline` pools.
- `benchmark-reports/<schema>/<catalog>/<series>/<run>.json` — the raw
  `replay-benchmark` mode report that the SLO report was evaluated from.

Each pair is verified as one artifact at record time: the SLO report's
`source.source_digest` must equal the canonical-form digest of the stored
replay report, and the revision and represented duration must match, so a
probe later measures exactly what the stored run measured.

Both are partitioned on two compatibility boundaries: the SLO report
contract (`<schema>`/`<catalog>` are `schema_version`/`catalog_version`, and
`slo-baseline` fails closed on a history spanning two) and the #58 workload
series (same helper the `benchmark-data` history uses). One leaf directory is
exactly one contract in exactly one series.

The trees are kept separate on purpose: the retrieval below pools
`reports/<...>/*.json` with a wildcard, and a replay report in that directory
would be refused as not an SLO report.

`<run>` is derived from the report's `source.run_id`. Re-recording a run id
replaces both of its artifacts, so a re-run of one workflow run is idempotent
while distinct runs accumulate.

## Ratio objectives

git fetch origin slo-data
git worktree add target/slo-history origin/slo-data

cargo run --locked -p aivtuber-app --bin slo-baseline -- \
  target/slo-history/reports/<schema>/<catalog>/<series>/*.json \
  --out target/aivtuber-slo/baseline-proposal.json

## Latency objectives

A latency target needs one more *measured* step: the conforming ratio at a
chosen `threshold_ms`. That ratio cannot be reconstructed from the stored
percentiles, so each stored run is re-probed from its stored replay report —
which is what `benchmark-reports/` keeps. Probe every run in the leaf at the
boundary chosen from `latency_calibration`, then pool the probed reports:

leaf=reports/<schema>/<catalog>/<series>
raw=benchmark-reports/<schema>/<catalog>/<series>
mkdir -p target/aivtuber-slo/probed

# One probed report per stored run, each with its own --run-id. The run ids
# are the `source.run_id` values in the sibling reports/ leaf.
for run in target/slo-history/$raw/*.json; do
  cargo run --locked -p aivtuber-app --bin slo-report -- \
    --report $run \
    --run-id <that-run's-source.run_id> \
    --probe-latency availability.event_to_first_audio_within_target=250 \
    --out target/aivtuber-slo/probed/$(basename $run)
done

cargo run --locked -p aivtuber-app --bin slo-baseline -- \
  target/aivtuber-slo/probed/*.json \
  --out target/aivtuber-slo/latency-baseline.json
