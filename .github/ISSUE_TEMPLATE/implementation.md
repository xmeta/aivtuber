---
name: Implementation task
about: Create an agent-ready, testable implementation issue
title: ''
labels: 'enhancement'
assignees: ''
---

> Before implementation, assign one primary milestone and exactly one priority:P1, priority:P2, or priority:P3 label.
> Add status:agent-ready only after dependencies are satisfied and the acceptance/verification sections are actionable. Use status:blocked when a known blocker prevents implementation.
> Add status:design-ready when research/architecture/evaluation work may proceed before implementation; it does not imply code readiness.

## Goal

<!-- State the observable capability/outcome, not only the code to write. -->

## Context / evidence

<!-- Current behavior, baseline, relevant docs, reproduction, benchmark, or design evidence. -->

## Dependencies / ownership

<!-- Blocked by / depends on / related issues. Name the subsystem that owns the behavior. -->

## Reuse-first assessment

<!-- Before designing generic infrastructure, check docs/reuse-first.adoc, docs/implementation-accelerator.adoc, and maintained OSS/crates. Record candidate(s), license/maintenance/runtime fit, and a decision: adopt | wrap | sidecar | partial reuse | reference only | benchmark first | reject | custom. If custom, state why earlier reuse options do not fit. -->

## Scope

<!-- Smallest useful vertical slice. -->

## Acceptance criteria

- [ ]

## Verification

<!-- Required targeted tests, full checks, replay benchmark, soak, or manual smoke. -->

## Architecture / security constraints

<!-- Trust, authority, schema, persistence, network, replay, scheduler, privacy, provider boundaries. Write None only when genuinely not applicable. -->

## Expected outcome / guardrails

<!-- For uncertain/optimization work: expected direction, baseline if known, and what must not regress. Routine work may say Not applicable with a reason. -->

## Decision-changing evidence

<!-- What result would make us narrow, defer, redesign, or abandon this approach? -->

## Non-goals

<!-- Explicitly prevent scope creep. -->

## Agent notes

<!-- Optional: likely files/types to inspect. Avoid prescribing an implementation when the decision is still open. -->
