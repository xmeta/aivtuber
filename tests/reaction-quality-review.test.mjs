import { describe, expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import { __testables } from "../scripts/reaction-quality-review.mjs";

const {
  expectedPresentation,
  validateSession,
  renderBlindPacket,
  normalizedChoice,
  aggregateSession,
  renderMarkdown,
} = __testables;

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const example = JSON.parse(
  readFileSync(
    join(
      root,
      "examples/evaluation/reaction-quality-review/development-example.json",
    ),
    "utf8",
  ),
);

function session() {
  return structuredClone(example);
}

describe("reaction-quality review blinding", () => {
  test("seeded presentation is deterministic and matches the checked-in fixture", () => {
    expect(expectedPresentation("dev-near-miss-hardware-advice", 151)).toEqual({
      a_role: "candidate",
      b_role: "base",
    });
    expect(expectedPresentation("dev-repetition-third-greeting", 151)).toEqual({
      a_role: "base",
      b_role: "candidate",
    });
    expect(
      expectedPresentation("dev-near-miss-hardware-advice", 151),
    ).toEqual(expectedPresentation("dev-near-miss-hardware-advice", 151));
  });

  test("reviewer packet strips policy identity and A/B role mapping", () => {
    const packet = renderBlindPacket(session());
    const serialized = JSON.stringify(packet);

    expect(serialized).not.toContain("stage2-routing-base");
    expect(serialized).not.toContain("stage2-routing-candidate");
    expect(serialized).not.toContain("base-example-sha");
    expect(serialized).not.toContain("candidate-example-sha");
    expect(serialized).not.toContain("a_role");
    expect(serialized).not.toContain("b_role");
    expect(serialized).not.toContain("route_class");

    expect(packet.cases[0].a.reaction_summary).toContain("causal question");
    expect(packet.cases[0].b.reaction_summary).toContain("generic troubleshooting");
  });

  test("tampered A/B presentation is rejected", () => {
    const doc = session();
    doc.cases[0].presentation = {
      a_role: "base",
      b_role: "candidate",
    };

    expect(() => validateSession(doc)).toThrow(
      "presentation does not match seeded randomization",
    );
  });
});

describe("reaction-quality outcome normalization", () => {
  test("A/B votes map back to base and candidate identities", () => {
    const candidateFirst = session().cases[0];
    expect(normalizedChoice(candidateFirst, "a_better")).toBe(
      "candidate_better",
    );
    expect(normalizedChoice(candidateFirst, "b_better")).toBe("base_better");

    const baseFirst = session().cases[1];
    expect(normalizedChoice(baseFirst, "a_better")).toBe("base_better");
    expect(normalizedChoice(baseFirst, "b_better")).toBe("candidate_better");

    expect(normalizedChoice(baseFirst, "tie")).toBe("tie");
    expect(normalizedChoice(baseFirst, "both_unacceptable")).toBe(
      "both_unacceptable",
    );
  });

  test("aggregation preserves reviewer disagreement instead of inventing consensus", () => {
    const summary = aggregateSession(session());

    expect(summary.totals).toEqual({
      base_better: 0,
      candidate_better: 5,
      tie: 1,
      both_unacceptable: 0,
    });
    expect(
      summary.cases.find(
        (testCase) => testCase.case_id === "dev-repetition-third-greeting",
      ).disagreement,
    ).toBe(true);
    expect(summary.product_improvement_claim_status).toBe(
      "quality_guardrail_passed",
    );
  });

  test("both-unacceptable votes remain a first-class outcome", () => {
    const doc = session();
    doc.cases[1].reviews[1].choice = "both_unacceptable";
    const summary = aggregateSession(doc);

    expect(summary.totals.both_unacceptable).toBe(1);
    expect(summary.totals.tie).toBe(0);
    expect(summary.cases[1].counts.both_unacceptable).toBe(1);
    expect(summary.cases[1].disagreement).toBe(true);
  });

  test("quality regression blocks an unqualified product-improvement claim", () => {
    const doc = session();
    doc.interpretation = {
      quality_guardrail: "regression",
      rationale: "Candidate quality regressed despite better proxy metrics.",
      decision: "revert",
    };

    const summary = aggregateSession(doc);
    expect(summary.quality_guardrail).toBe("regression");
    expect(summary.product_improvement_claim_status).toBe(
      "blocked_quality_regression",
    );
    expect(summary.decision).toBe("revert");
  });
});

describe("reaction-quality benchmark companion report", () => {
  test("renders quality evidence beside #58 metric deltas without scalar collapse", () => {
    const doc = session();
    doc.interpretation = {
      quality_guardrail: "regression",
      rationale: "Human review found a semantic-quality regression.",
      decision: "revert",
    };
    const summary = aggregateSession(doc);
    const gate = {
      verdict: "pass",
      base_commit: "base-example-sha",
      head_commit: "candidate-example-sha",
      metrics: [
        {
          name: "routing.llm_calls_per_100_events",
          base: 40,
          head: 25,
          delta_percent: 37.5,
          status: "improved",
        },
        {
          name: "cached.first_audio.p95_ms",
          base: 100,
          head: 90,
          delta_percent: 10,
          status: "improved",
        },
      ],
      invariants: [],
      warnings: [],
    };

    const markdown = renderMarkdown(summary, gate);
    expect(markdown).toContain("blocked_quality_regression");
    expect(markdown).toContain("routing.llm_calls_per_100_events");
    expect(markdown).toContain("cached.first_audio.p95_ms");
    expect(markdown).toContain("Benchmark verdict: **PASS**");
    expect(markdown).toContain(
      "Benchmark and human-review evidence are shown together but remain separate evidence classes.",
    );
    expect(markdown).toContain(
      "A quality regression blocks an unqualified product-improvement claim",
    );
    expect(markdown).not.toContain("Quality score");
  });
});
