import os, sys
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from floor_sim import *

for cond in ("clean", "noisy15"):
    R, L = load(cond)
    B = [s for s in L if s["cls"] in ("B1", "B2")]
    for vm in (500, 400):
        cfg = Cfg(take_ms=800, bc_nv_ms=300, bc_voc_ms=vm)
        ov = []; n = 0
        for s in B:
            e = replay_listen(R[s["id"]], cfg)
            fe = int(round(s["final_end"] * 1000))
            runs = [a * FT for a, b in raw_runs(R[s["id"]]["p"])]
            for t, l in e["bcs"]:
                if l == 4 and t < fe:
                    n += 1
                    nxt = [x for x in runs if x > t - FT]
                    if nxt:
                        ov.append(t + 400 - nxt[0])
        print(cond, "vocal@", vm, "events on holds", n, "overlap with resumed user speech (400 ms asset) ms p10/p50/p90:", pctl(ov, .1), pctl(ov, .5), pctl(ov, .9), "max", max(ov))
