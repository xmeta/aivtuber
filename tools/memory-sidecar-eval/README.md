# memory-sidecar-eval: Hindsight on the portable memory fixture (issue #105)

A standalone evaluation tool. It is not part of the Rust workspace, adds no
Rust or bun dependency, and CI does not run it: it needs a running Hindsight
deployment and a local LLM.

It measures the OSS memory sidecar [Hindsight](https://github.com/vectorize-io/hindsight)
(MIT, v1.2.0) on `examples/memory-long-range/cases.json` and emits a
`memory-long-range-eval/v1` report that `memory-eval-compare` judges against the
runtime's own report with the executable adoption rule (`docs/memory-adaptation.adoc` section 12).

This is a different measurement from `memory-sidecar-eval` (the Rust runner
over the `MemorySidecar` port, section 12.1). That runner keeps admission,
retention, supersession and expiry in the project's hands and retires claims on
the service. This tool does the opposite on purpose: it tells Hindsight every
claim and applies none of the runtime's rules, so Hindsight's own behaviour is
what is measured.

## What the adapter does

| Step | Rule |
|---|---|
| Retain | One `POST /memories` per claim, fixture order, one fresh bank per case, bank deleted afterwards. `document_id` = claim id, `timestamp` = the claim's event time, tag `actor:<viewer>` when the claim has an actor. `text_bytes` is tiled exactly as the Rust runner does. |
| Settle | After the last retain, wait until the bank reports no pending operation and no pending consolidation (observations are produced in the background). |
| Recall | One `POST /memories/recall` per case: query = the request topic (or the fixture's synthetic request text), `query_timestamp` = `now_ms`, `tags=[actor:<viewer>]` with `tags_match=any` (the viewer's claims plus unscoped ones), all fact types, `max_tokens=16384` so Hindsight's own token budget never cuts the ranking below the project's bounds, source facts included. |
| Attribution | A recalled fact maps to its `document_id`; a recalled observation maps to the `document_id`s of its `source_fact_ids`. Text is never compared. An item with no traceable claim is not delivered and not attributed: it is recorded in `refused_items` (`unknown`) and in the trace file. |
| Scope | An item whose claim belongs to another viewer is not delivered; it is recorded as `refused_items` `out_of_scope` (same contract as the Rust runner). The recall already filters by tag, so this only fires if Hindsight leaks. |
| Bounds | Hindsight's rank order, claim ids de-duplicated (first rank kept), then whole items packed by the same rule as `aivtuber_app::bounded_prefix` (item bound 12, per-item 4096 bytes skipped, 8192-byte request bound, packing stops at the first overflow). Text is never shortened. |

Not applied, by design: supersession, expiry, the retention entry bound, causal
edges, re-ranking. `entries_high_water` is therefore the number of claims the
case told Hindsight, which can exceed the case's `max_entries` (the retention
plateau case); the comparison does not score it.

`truncated_claims` counts delivered items whose text is not exactly the one
claim they carry. Hindsight stores extracted facts, not the text it was given,
so this counts every recalled item whose extracted text differs from the claim
text, and every observation that merges several claims.

## Reproduce

Prerequisites: Docker, Python 3.11+, Ollama on the host with `gpt-oss:20b` and
`nomic-embed-text`, and the Rust toolchain (for the two project CLIs).

```sh
# 1. Hindsight 1.2.0 (see docker/Dockerfile for why this is built, not pulled)
docker build -t hindsight-eval:1.2.0 tools/memory-sidecar-eval/docker
docker run -d --name hindsight-eval -p 8888:8888 \
  --add-host host.docker.internal:host-gateway \
  -e HINDSIGHT_API_LLM_PROVIDER=ollama \
  -e HINDSIGHT_API_LLM_BASE_URL=http://host.docker.internal:11434/v1 \
  -e HINDSIGHT_API_LLM_MODEL=gpt-oss:20b \
  -e HINDSIGHT_API_LLM_TEMPERATURE=0.0 \
  -e HINDSIGHT_API_EMBEDDINGS_PROVIDER=openai \
  -e HINDSIGHT_API_EMBEDDINGS_OPENAI_BASE_URL=http://host.docker.internal:11434/v1 \
  -e HINDSIGHT_API_EMBEDDINGS_OPENAI_MODEL=nomic-embed-text \
  -e HINDSIGHT_API_EMBEDDINGS_OPENAI_API_KEY=ollama \
  -e HINDSIGHT_API_EMBEDDINGS_QUERY_PREFIX="search_query: " \
  -e HINDSIGHT_API_EMBEDDINGS_PASSAGE_PREFIX="search_document: " \
  hindsight-eval:1.2.0

# 2. the runtime report and the adapter
cargo run --locked -p aivtuber-app --bin memory-long-range-eval -- \
  examples/memory-long-range/cases.json examples/memory-long-range/sidecar/hindsight/runtime-report.json
python -m pip install -r tools/memory-sidecar-eval/requirements.txt
python tools/memory-sidecar-eval/hindsight_eval.py \
  examples/memory-long-range/cases.json \
  examples/memory-long-range/sidecar/hindsight/runtime-report.json \
  --out examples/memory-long-range/sidecar/hindsight/run-1-report.json \
  --trace-out examples/memory-long-range/sidecar/hindsight/run-1-trace.json

# 3. the verdict
cargo run --locked -p aivtuber-app --bin memory-eval-compare -- \
  examples/memory-long-range/cases.json \
  examples/memory-long-range/sidecar/hindsight/runtime-report.json \
  examples/memory-long-range/sidecar/hindsight/run-1-report.json \
  --out examples/memory-long-range/sidecar/hindsight/run-1-verdict.json
```

Repeat step 2 and 3 for runs 2 and 3: an LLM extracts the facts, so each run is
kept and judged on its own and the runs are never averaged. The container needs
no API key and no cloud model; the `OPENAI_API_KEY` above is a placeholder the
Ollama endpoint ignores.

## Files

`docker/Dockerfile` the Hindsight 1.2.0 image. `hindsight_eval.py` the adapter.
`requirements.txt` the pinned Python dependency. Results and the manifest are
under `examples/memory-long-range/sidecar/hindsight/`.
