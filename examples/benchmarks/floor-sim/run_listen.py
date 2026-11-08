import os, sys, json, math
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from floor_sim import *
from repro import exp_events

VOC_MS = 400   # assumed duration of a short cached vocal backchannel (assumption, not measured)
P_LIST = (300, 600, 1000, 1500)  # assumed LLM+TTS preparation time to first audio (assumption, not measured)
SCR = os.path.join(os.environ["TURN_BENCH_DIR"], "out", "floor-sim")
os.makedirs(SCR, exist_ok=True)


def evaluate(cond, name, cfg, use_st=False):
    R, L = load(cond)
    B = [s for s in L if s["cls"] in ("B1", "B2")]
    A = [s for s in L if s["cls"] == "A"]
    C = [s for s in L if s["cls"] == "C"]
    ends = A + B
    E = {s["id"]: replay_listen(R[s["id"]], cfg, use_st) for s in ends + C}
    o = dict(name=name, n_hold=len(B), n_end=len(ends))
    # --- TakeTurn
    fh = fa = 0; lags = []; none = 0
    exp_fh = 0
    for s in ends:
        fe = int(round(s["final_end"] * 1000)); t = E[s["id"]]["takes"]
        early = any(x < fe for x in t)
        if s in B: fh += early
        else: fa += early
        late = [x for x in t if x >= fe]
        if late: lags.append(late[0] - fe)
        else: none += 1
    o.update(false_hold=(fh, len(B)), false_A=(fa, len(A)), lag_p50=pctl(lags, .5), lag_p90=pctl(lags, .9), lag_none=none)
    if cfg.take_ms < INF:
        eh = sum(1 for s in B if any(x < s["final_end"] for x in exp_events(R[s["id"]]["p"], s["dur"], cfg.take_ms / 1000)))
        o["false_hold_exp_semantics"] = (eh, len(B))
    o["C_fired"] = sum(1 for s in C if E[s["id"]]["takes"] or E[s["id"]]["bcs"])
    # --- backchannels
    nv_h = voc_h = col_h = 0           # holds with >=1
    nv_a = voc_a = col_a = 0           # A streams with >=1 inside natural pause
    nv_e = voc_e = 0                   # true ends (t >= fe, before take)
    ev_nv_in = ev_voc_in = ev_col = 0  # event counts inside
    for s in ends:
        fe = int(round(s["final_end"] * 1000)); e = E[s["id"]]
        runs = [a * FT for a, b in raw_runs(R[s["id"]]["p"])]
        ins = [(t, l) for t, l in e["bcs"] if t < fe]
        end = [(t, l) for t, l in e["bcs"] if t >= fe]
        def collides(t):
            nxt = [x for x in runs if x > t - FT]
            return bool(nxt) and nxt[0] < t + VOC_MS
        nvi = [t for t, l in ins if l == 3]; voi = [t for t, l in ins if l == 4]
        coi = [t for t in voi if collides(t)]
        ev_nv_in += len(nvi); ev_voc_in += len(voi); ev_col += len(coi)
        if s in B:
            nv_h += bool(nvi); voc_h += bool(voi); col_h += bool(coi)
        else:
            nv_a += bool(nvi); voc_a += bool(voi); col_a += bool(coi)
        nv_e += any(l == 3 for t, l in end); voc_e += any(l == 4 for t, l in end)
    o["bc"] = dict(hold_nv=(nv_h, len(B)), hold_voc=(voc_h, len(B)), hold_voc_collide=(col_h, len(B)),
                   A_nv=(nv_a, len(A)), A_voc=(voc_a, len(A)), A_voc_collide=(col_a, len(A)),
                   end_nv=(nv_e, len(ends)), end_voc=(voc_e, len(ends)),
                   ev_voc_in=ev_voc_in, ev_voc_collide=ev_col)
    # --- preparation
    started = canc = comm = 0; lead = []; aud = {P: [] for P in P_LIST}; base = {P: [] for P in P_LIST}
    held_prep = 0; ends_prepped = 0
    for s in ends:
        fe = int(round(s["final_end"] * 1000)); e = E[s["id"]]
        for (t0, t1, out) in e["preps"]:
            started += 1; canc += out == "cancelled"; comm += out == "committed"
        if s in B and any(out == "cancelled" and t0 < fe for t0, t1, out in e["preps"]): held_prep += 1
        com = [(t0, t1) for t0, t1, out in e["preps"] if out == "committed" and t1 >= fe]
        late = [x for x in e["takes"] if x >= fe]
        if com:
            t0, t1 = com[-1]; lead.append(t1 - t0); ends_prepped += 1
            for P in P_LIST: aud[P].append(max(t1, t0 + P) - fe)
        if late:
            for P in P_LIST: base[P].append(late[0] + P - fe)
    o["prep"] = dict(started=started, cancelled=canc, committed=comm, cancelled_per_100_ends=100 * canc / len(ends),
                     holds_with_cancelled_prep=(held_prep, len(B)), lead_p10=pctl(lead, .1), lead_p50=pctl(lead, .5),
                     lead_p90=pctl(lead, .9), ends_prepped=(ends_prepped, len(ends)),
                     first_audio_median={P: (pctl(aud[P], .5), pctl(base[P], .5)) for P in P_LIST})
    return o


def f(k): return pct(*k)


def show(o):
    b = o["bc"]
    print(f"  {o['name']}")
    print(f"    TakeTurn false on holds {f(o['false_hold'])}" + (f"   [EXP-0001 segment semantics: {f(o['false_hold_exp_semantics'])}]" if 'false_hold_exp_semantics' in o else "") + f"; on A natural pauses {f(o['false_A'])}; non-speech fired {o['C_fired']}/88")
    print(f"    lag at true end p50 {o['lag_p50']:.0f} p90 {o['lag_p90']:.0f} ms; no take at/after end {o['lag_none']}")
    print(f"    holds with >=1 nonverbal {f(b['hold_nv'])}; vocal {f(b['hold_voc'])}; vocal colliding(<{VOC_MS}ms) {f(b['hold_voc_collide'])}")
    print(f"    A natural pauses with nonverbal {f(b['A_nv'])}; vocal {f(b['A_voc'])}; collide {f(b['A_voc_collide'])}")
    print(f"    true ends preceded by nonverbal {f(b['end_nv'])}; by vocal {f(b['end_voc'])}")
    pr = o["prep"]
    if pr["started"]:
        print(f"    prepare: started {pr['started']} cancelled {pr['cancelled']} ({pr['cancelled_per_100_ends']:.0f} per 100 true ends) committed {pr['committed']}; holds that triggered a cancelled prep {f(pr['holds_with_cancelled_prep'])}")
        print(f"      lead (commit - prepare start) p10/p50/p90 {pr['lead_p10']:.0f}/{pr['lead_p50']:.0f}/{pr['lead_p90']:.0f} ms on {f(pr['ends_prepped'])}")
        print("      median first-audio lag after true end, assumed prep time P: " + "; ".join(f"P={P}: {a:.0f} vs {b_:.0f}" for P, (a, b_) in pr["first_audio_median"].items()))


CFGS = None


def cfgs():
    base = dict(take_ms=800)
    c = []
    for T in (300, 500, 600, 700, 800, 1000):
        c.append((f"P1 fixed-{T}", Cfg(take_ms=T), False))
    c += [
        ("P2 nv@300 (no vocal)", Cfg(**base, bc_nv_ms=300), False),
        ("P2 nv@300, vocal@500 (min preceding speech 1000)", Cfg(**base, bc_nv_ms=300, bc_voc_ms=500), False),
        ("P2 nv@300, vocal@500 (min preceding speech 3000)", Cfg(**base, bc_nv_ms=300, bc_voc_ms=500, bc_voc_min_speech_ms=3000), False),
        ("P2 nv@300, vocal@500 (min preceding speech 5000)", Cfg(**base, bc_nv_ms=300, bc_voc_ms=500, bc_voc_min_speech_ms=5000), False),
        ("P2 nv@300, vocal@400 (min preceding speech 1000)", Cfg(**base, bc_nv_ms=300, bc_voc_ms=400), False),
        ("P2 nv@500, vocal@500 (min preceding speech 1000)", Cfg(**base, bc_nv_ms=500, bc_voc_ms=500), False),
    ]
    for E in (300, 400, 500, 600):
        c.append((f"P3 = P2(nv@300, no vocal) + prepare@{E}, commit@800", Cfg(**base, bc_nv_ms=300, prep_ms=E), False))
    c.append(("P3s = P2(nv@300, no vocal) + prepare on Smart Turn score>=0.5 only (uncalibrated; start only), commit@800", Cfg(**base, bc_nv_ms=300, prep_uses_score=True), True))
    return c


if __name__ == "__main__":
    cond = sys.argv[1] if len(sys.argv) > 1 else "clean"
    print("=== condition", cond)
    allres = []
    for name, cfg, st in cfgs():
        o = evaluate(cond, name, cfg, st); show(o); allres.append(o)
    json.dump(allres, open(f"{SCR}/listen_{cond}.json", "w"), default=str, indent=1)
