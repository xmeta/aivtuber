<!-- Follow /AGENTS.md and docs/maintenance.adoc. The linked issue should define the primary milestone/priority and acceptance criteria. -->

## Linked issue

Closes #

## Summary

Describe the user-visible or architectural change and why this PR is the smallest useful GitHub Flow unit.

## Verification

List the commands/tests run locally and any manual smoke checks.

- [ ] `cargo fmt --all -- --check`
- [ ] `cargo clippy --locked --workspace --all-targets -- -D warnings`
- [ ] `cargo test --locked --workspace`
- [ ] `cargo build --locked --workspace`
- [ ] `bun install --frozen-lockfile`
- [ ] `bun run validate:toolchains`
- [ ] `bun run validate`
- [ ] `bun run validate:rust-parity`
- [ ] `bun test`

## Architecture / security impact

Describe any boundary, schema, dependency, privilege, network, secret, replay, scheduler, or persistence changes. Write "None" when not applicable.

## Risk and rollback

Describe the main failure mode and how to revert or disable this change safely.

## Expected / evidence

For uncertain, optimization, policy, threshold, dependency, or other evidence-sensitive changes, fill this section before interpreting the result. Routine deterministic fixes/docs may write `Not applicable — <reason>`.

**Mode:** Routine | Investigative | High-consequence/high-uncertainty | Not applicable

**Expected:** What observable behavior should improve or change?

**Baseline / evidence:** What is the current measured/reproduced state, and what evidence supports trying this direction?

**Guardrails:** What must not regress?

**Decision-changing evidence:** What result would make us narrow, redesign, defer, revert, or stop?

## Result / decision

For evidence-sensitive changes, complete this after verification/CI. Do not treat implementation completion itself as proof of improvement.

**Actual:** What was observed?

**Difference / interpretation:** How did the result differ from the expectation, including noise or missing evidence?

**Decision:** Keep | Expand | Narrow | Revert | Defer | Stop | Not applicable

If a confirmatory success criterion changed after results were visible, state the old criterion, new criterion, reason, and effective point; do not rewrite the original criterion as if it had always applied.

## Merge checklist

- [ ] The branch is based on current `main`.
- [ ] Generated/local-only files are not accidentally included.
- [ ] Dependency lockfile changes are intentional and reviewed.
- [ ] All required CI jobs are green.
- [ ] The PR is small enough to review as one testable change, or the reason it cannot be split is documented.
