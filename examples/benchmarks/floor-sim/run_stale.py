import os, sys, json, random
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from floor_sim import *

STALL_MS = 600
K = 5


def main(cond):
    R, L = load(cond)
    B = [s for s in L if s["cls"] in ("B1", "B2")]
    ends = [s for s in L if s["cls"] in ("A", "B1", "B2")]
    rng = random.Random(20261108)
    naive = Cfg(take_ms=800)
    guard = Cfg(take_ms=800, max_obs_age_ms=160)
    res = {}
    for name, cfg in (("no stall (reference)", naive), ("stall, no freshness rule", naive), ("stall, freshness rule (age<=160 ms)", guard)):
        false = n = 0; lags = []
        for s in B:
            fe = int(round(s["final_end"] * 1000)); h0, h1 = [int(round(x * 1000)) for x in s["hold"][0]]
            for k in range(K):
                n += 1
                stall = None
                if name.startswith("stall"):
                    a = rng.randint(h0, h1); stall = (a, a + STALL_MS)
                e = replay_listen(R[s["id"]], cfg, False, stall)
                false += any(t < fe for t in e["takes"])
        # lag at true ends with a stall starting at a random time within 800 ms after the end
        for s in ends:
            fe = int(round(s["final_end"] * 1000))
            for k in range(2):
                stall = None
                if name.startswith("stall"):
                    a = fe + rng.randint(0, 800); stall = (a, a + STALL_MS)
                e = replay_listen(R[s["id"]], cfg, False, stall)
                late = [t for t in e["takes"] if t >= fe]
                if late: lags.append(late[0] - fe)
        res[name] = dict(false=(false, n), lag_p50=pctl(lags, .5), lag_p90=pctl(lags, .9), lag_p99=pctl(lags, .99), lag_n=len(lags))
        print(f"{name}: false TakeTurn on holds {pct(false, n)}; lag at true end p50/p90/p99 {pctl(lags,.5):.0f}/{pctl(lags,.9):.0f}/{pctl(lags,.99):.0f} ms (n={len(lags)})")
    os.makedirs(os.path.join(os.environ["TURN_BENCH_DIR"], "out", "floor-sim"), exist_ok=True)
    json.dump(res, open(os.path.join(os.environ["TURN_BENCH_DIR"], "out", "floor-sim", "stale_%s.json" % cond), "w"), indent=1)


if __name__ == "__main__":
    print("=== stale observation", sys.argv[1])
    main(sys.argv[1])
