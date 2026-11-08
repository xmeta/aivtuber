import os, sys, math
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from floor_sim import *


def q(t):
    return math.ceil(t / (FT / 1000) - 1e-9) * (FT / 1000)


def exp_events(p, dur, T):
    """EXP-0001 analyze.py ev_fixed semantics: min-speech-filtered segments, oracle next-segment start."""
    segs = segments(p)
    ev = []
    for k, (a, b) in enumerate(segs):
        te = b * FT / 1000
        ns = segs[k + 1][0] * FT / 1000 if k + 1 < len(segs) else dur
        if ns - te >= T:
            ev.append(q(te + T))
    return ev


if __name__ == "__main__":
    for cond in ("clean", "noisy15"):
        R, L = load(cond)
        holds = [s for s in L if s["cls"] in ("B1", "B2")]
        for T in (300, 500, 800):
            ef = {s["id"] for s in holds if any(t < s["final_end"] for t in exp_events(R[s["id"]]["p"], s["dur"], T / 1000))}
            mine = {s["id"] for s in holds if any(t < int(round(s["final_end"] * 1000)) for t in replay_listen(R[s["id"]], Cfg(take_ms=T))["takes"])}
            print(cond, T, "EXP-semantics", len(ef), "causal-raw", len(mine), "only-in-EXP", len(ef - mine), "only-in-mine", len(mine - ef))
            if T == 800:
                for sid in sorted(ef - mine):
                    p = R[sid]["p"]
                    rr = raw_runs(p)
                    s = next(x for x in L if x["id"] == sid)
                    print("   ", sid, s["pause_ms"], "hold", s["hold"], "raw runs(ms):", [(a * 32, b * 32) for a, b in rr][:6])
