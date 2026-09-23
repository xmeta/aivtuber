import { describe, expect, test } from "bun:test";

import { SourceRateLimiter } from "../src/rate-limit.js";

function fakeClock() {
  let t = 0;
  return { now: () => t, advance: (ms: number) => { t += ms; } };
}

describe("SourceRateLimiter", () => {
  test("admits within burst then drops on flood", () => {
    const clock = fakeClock();
    const limiter = new SourceRateLimiter(
      { ratePerSecond: 5, burst: 5, maxPayloadBytes: 2048 },
      clock.now,
    );
    const results: string[] = [];
    for (let i = 0; i < 8; i++) results.push(limiter.admit("chat:viewer1", { text: "hi" }));
    expect(results.slice(0, 5)).toEqual(["admitted", "admitted", "admitted", "admitted", "admitted"]);
    expect(results.slice(5)).toEqual(["dropped_rate", "dropped_rate", "dropped_rate"]);
  });

  test("refills over time", () => {
    const clock = fakeClock();
    const limiter = new SourceRateLimiter(
      { ratePerSecond: 1, burst: 1, maxPayloadBytes: 2048 },
      clock.now,
    );
    expect(limiter.admit("chat:a", {})).toBe("admitted");
    expect(limiter.admit("chat:a", {})).toBe("dropped_rate");
    clock.advance(1000);
    expect(limiter.admit("chat:a", {})).toBe("admitted");
  });

  test("tracks sources independently", () => {
    const clock = fakeClock();
    const limiter = new SourceRateLimiter(
      { ratePerSecond: 1, burst: 1, maxPayloadBytes: 2048 },
      clock.now,
    );
    expect(limiter.admit("chat:a", {})).toBe("admitted");
    // A different source still has its own burst.
    expect(limiter.admit("chat:b", {})).toBe("admitted");
    expect(limiter.admit("chat:a", {})).toBe("dropped_rate");
  });

  test("rejects oversized payloads", () => {
    const clock = fakeClock();
    const limiter = new SourceRateLimiter(
      { ratePerSecond: 100, burst: 100, maxPayloadBytes: 128 },
      clock.now,
    );
    const big = { text: "A".repeat(200) };
    expect(limiter.admit("chat:a", big)).toBe("dropped_size");
  });

  test("control plane bypasses content flood and stays bounded", () => {
    const clock = fakeClock();
    const limiter = new SourceRateLimiter(
      { ratePerSecond: 0, burst: 0, maxPayloadBytes: 64 },
      clock.now,
    );
    // Saturate the content source completely.
    expect(limiter.admit("chat:flooder", {})).toBe("dropped_rate");

    // Operator stop still gets through on the control queue.
    for (let i = 0; i < 64; i++) {
      expect(limiter.admit("operator", {}, { controlPlane: true })).toBe("admitted");
      limiter.releaseControl();
    }
    expect(limiter.admit("operator", {}, { controlPlane: true })).toBe("admitted");

    // Depth cap without releases eventually drops rather than growing unbounded.
    let dropped = false;
    for (let i = 0; i < 64; i++) {
      if (limiter.admit("operator", {}, { controlPlane: true }) === "dropped_rate") {
        dropped = true;
        break;
      }
    }
    expect(dropped).toBe(true);
  });
});
