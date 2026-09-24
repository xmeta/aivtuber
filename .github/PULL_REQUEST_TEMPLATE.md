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

## Merge checklist

- [ ] The branch is based on current `main`.
- [ ] Generated/local-only files are not accidentally included.
- [ ] Dependency lockfile changes are intentional and reviewed.
- [ ] All required CI jobs are green.
- [ ] The PR is small enough to review as one testable change, or the reason it cannot be split is documented.
