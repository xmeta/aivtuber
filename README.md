# benchmark-data

Durable benchmark history for the continuous benchmark loop (issue #58).

- `data/<suite>.<series>.jsonl` — one `schemas/benchmark-result.schema.json` document per line;
  deduped on (benchmark_suite, mode, git.commit, recording.run_id), last write wins.
  Retries of one workflow run are idempotent; distinct runs of the same revision
  accumulate so runner variance can be calibrated (docs/performance-goals.adoc).
- `<series>` encodes the comparator's compatibility boundary (mode + dataset_id +
  config_version + seed) plus a short stable hash: results from different datasets,
  configs, or seeds are never rendered into the same trend table.
- `trends/<suite>.<series>.md` — generated trend tables; do not edit by hand.
- `recording.run_id` / `recording.recorded_at` (+ diagnostic `attempt`) are appended by
  the recorder when a trusted job stores the result; benchmark producers never emit them.
  run_id is stable across re-runs of one workflow run (GitHub run_id), so a re-run
  replaces its earlier row; attempt never participates in identity.
- Written only by trusted jobs (push to main, scheduled benchmark);
  pull_request jobs never receive credentials for this branch.
- Commit identity and dataset/config metadata are preserved verbatim so
  after-action review (#62) can distinguish trends from one-run anomalies.
