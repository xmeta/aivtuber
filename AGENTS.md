# AGENTS.md

This file is the canonical machine-oriented development contract for the entire repository.
Human-facing architecture and policy documents remain authoritative for their subject areas; this file tells coding agents how to navigate and apply them.

## Mission

Build a low-latency AI VTuber runtime that prefers deterministic/reusable behavior, uses Jev-compatible reflex decisions for fast structured judgment, and invokes expensive generative reasoning only when necessary.

## Source-of-truth order

When instructions appear to conflict, use this order:

1. Security/trust rules in docs/security-threat-model.adoc.
2. Wire/domain rules in docs/domain-contract.adoc and repository JSON Schemas.
3. Architecture boundaries in docs/architecture.adoc.
4. Merge/dependency policy in docs/maintenance.adoc.
5. Task-specific GitHub issue/ADR and its acceptance criteria.
6. This file and docs/agent-development.adoc for development procedure.

Do not silently reinterpret a security or schema rule to make implementation easier.

## Start every task

1. Run git status --short --branch; do not overwrite unrelated local work.
2. Read the complete GitHub issue, its milestone, priority label, dependencies, and linked ADR/docs.
3. Fetch current main. Create one issue-sized branch from current origin/main; never develop directly on main.
4. Search the repository with tgrep, not another repository-content search command.
5. Read the smallest relevant authoritative docs before editing code.
6. Form a testable vertical-slice plan. Prefer the cheapest evidence that can invalidate the plan before a large implementation.

Do not open a normal PR for incomplete implementation. For interrupted work, push a checkpoint branch/commit and leave a precise handoff instead.

## Repository map

- crates/: production-target Rust implementation.
- crates/app: production composition root and lifecycle ownership.
- crates/domain: provider-neutral bounded/versioned domain contracts.
- crates/runtime: security enforcement and runtime boundaries.
- crates/reflex: semantic retrieval and Jev/System One decision path.
- crates/scheduler: deterministic performance scheduling/arbitration.
- crates/asset-store: validated Performance Asset storage/cache.
- crates/generative: generated-response pipeline and compilation.
- crates/adaptation: bounded memory/adaptation.
- crates/adapters: provider/platform adapters.
- crates/telemetry, crates/hardening: measurement and failure/soak tooling.
- src/: TypeScript reference/prototype/regression harness, not the production-core authority.
- schemas/: normative JSON wire schemas.
- examples/: fixtures, replay cases, benchmarks, security cases.
- docs/: architecture, security, runtime, metrics, ADRs, and maintenance policy.

A TypeScript-only change does not close a Rust production-core acceptance criterion unless the issue explicitly says otherwise.

## Non-negotiable architecture/security invariants

- Model/Jev/generated output is data, never runtime authority.
- Serialized authorization claims are audit/replay data, not authenticated capability.
- Privileged control requires the non-serializable capability minted by authenticated local ingress.
- Durable memory writes require their non-serializable permit; public chat cannot mint one.
- The normalizer assigns source class/trust/plane; untrusted payloads cannot self-escalate.
- Reflex/Thinking requests are bounded and typed; do not pass arbitrary raw history/state maps.
- Runtime queues, histories, caches, observations, and retained context must stay explicitly bounded.
- Operator emergency control must remain independent of saturated/failed content or model paths.
- Jev/LLM/TTS/provider failures degrade without freezing the deterministic performer.
- Jev or LLM must not run in the frame/motor loop.
- Replay-relevant decisions use monotonic/logical ordering, not wall-clock ordering.
- Provider-specific wire DTOs stay behind adapters.

If a change touches one of these boundaries, read docs/security-threat-model.adoc before coding.

## Repository search

Use tgrep for repository-content searches.

Recommended setup from the repository root:

~~~sh
tgrep index .
tgrep status .
~~~

Examples:

~~~sh
tgrep -n 'PerformanceIntent' .
tgrep -n -i -g 'crates/**/*.rs' 'authorization|permit|authority' .
tgrep -n -g 'docs/**/*.adoc' 'schema_version' .
tgrep -n -t rust 'struct ReflexRequest' .
~~~

tgrep uses -g/--glob; it does not use ripgrep's --include option.
Search with PATH . from the repository root so one root index is reused; narrow scope with -g/--glob or -t/--type rather than changing the search root.
The index is a snapshot: rebuild with tgrep index . after edits when you need searches to include changed files, or use --no-index for a one-off live scan.

## Schema/domain change order

schemas/ is the normative wire-format source of truth.

For an intentional wire change:

1. Update the relevant JSON Schema and positive/negative fixtures.
2. Update Rust domain types/validation in the same PR.
3. Update TypeScript/reference behavior when that contract is intentionally shared.
4. Run fixture -> Rust and Rust -> Schema parity.
5. Update replay fixtures/docs and schema version when the change is incompatible.

Do not create a second implicit wire contract in an adapter or prototype.

## Reuse-first rule

Before writing generic infrastructure, check docs/reuse-first.adoc and search maintained crates/reference implementations.

- Reuse generic mechanisms when they fit.
- Keep aivtuber-specific policy, authority, replay, determinism, and bounded-state semantics project-owned.
- Record adopt / wrap / partial reuse / reference only / benchmark first / reject with a short reason in the issue or PR.
- Do not add a dependency without checking license, maintenance status, transitive/runtime impact, security advisories, determinism/replay effects, and bounded-state behavior.
- Do not repeatedly revisit a rejected candidate unless new evidence changes the trade-off.

See #100 for dependency/supply-chain policy and docs/reuse-first.adoc for the current candidate matrix.

## Implementation style

- Prefer one issue-sized vertical slice per branch/PR.
- Keep deterministic policy in code; AI/provider output recommends high-level decisions.
- Reuse existing domain types, scheduler, security gates, telemetry vocabulary, and fixtures before adding parallel implementations.
- Preserve provider-neutral domain boundaries.
- Make fallback behavior typed and testable.
- Add regression tests with the bug/behavior change, not later.
- Avoid unrelated refactors while implementing an issue.
- Document assumptions/revisit conditions for uncertain or high-consequence changes.
- Keep comments focused on invariants/reasons rather than restating code.

## Validation strategy

Run targeted checks while iterating, then the full relevant gate before PR.

Typical targeted Rust loop:

~~~sh
cargo fmt --all -- --check
cargo clippy --locked -p <crate> --all-targets -- -D warnings
cargo test --locked -p <crate>
~~~

Schema/reference changes:

~~~sh
bun run validate:toolchains
bun run validate
bun run validate:rust-parity
bun test
~~~

Before a code PR, normally run the complete repository checks:

~~~sh
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace
cargo build --locked --workspace
bun install --frozen-lockfile
bun run validate:toolchains
bun run validate
bun run validate:rust-parity
bun test
~~~

Also run when relevant:

~~~sh
cargo run --locked -p aivtuber-app --bin replay-benchmark -- examples/benchmarks/replay-comparison.json target/aivtuber-benchmarks
cargo run --locked -p aivtuber-hardening --bin failure-soak -- target/hardening-soak.json 5000
~~~

Docs-only changes do not need expensive runtime benchmarks unless they alter benchmark fixtures/configuration. CI remains the final gate.

## Dependencies and generated files

- Feature work uses committed lockfiles and --locked / --frozen-lockfile.
- Do not refresh Cargo.lock or bun.lock implicitly.
- Isolate deliberate dependency upgrades when practical and follow docs/maintenance.adoc.
- Do not commit secrets, generated media, target/, node_modules/, .tgrep/, local logs, or diagnostic bundles.

## GitHub workflow

Development method is GitHub Flow.

- One primary issue per implementation branch.
- Assign/retain one primary delivery milestone and one priority:P1/P2/P3 label for implementation issues.
- Use milestone stage for delivery grouping; use priority label for ordering within/among stages.
- status:agent-ready means dependencies are satisfied and an agent may self-select the issue when asked to continue autonomously; status:blocked means do not start it until the named blocker is resolved.
- Link the issue from the PR and use Closes #... when the PR fully completes it.
- Required CI must be green on the final head commit before merge.
- Do not bypass branch protection or force-push main.
- Prefer conventional, scoped commits such as feat(reflex): ... (#74) or docs: ... (#95).

For evidence-sensitive work, follow the hypothesis/guardrail/benchmark process linked from the issue and docs/metrics.adoc.

## Finish / handoff

Before declaring work complete:

1. Re-read the issue acceptance criteria.
2. Check git diff for unrelated/generated/secret changes.
3. Run the required targeted/full validation.
4. Update docs/fixtures/ADRs when behavior or a contract changed.
5. Commit and push the issue branch.
6. Open a PR only when the implementation is coherent and complete.

If work must stop before completion, leave a handoff containing:

- issue and goal;
- branch and latest commit;
- current main base;
- completed changes;
- exact remaining work;
- verification already run and results;
- known failures/blockers;
- next recommended command/file to inspect.

See docs/agent-development.adoc for the detailed playbook.
