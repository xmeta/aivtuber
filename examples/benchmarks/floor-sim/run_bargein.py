import os, sys, json, random
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from floor_sim import *

SCR = os.path.join(os.environ["TURN_BENCH_DIR"], "out", "floor-sim")
os.makedirs(SCR, exist_ok=True)
DUCK_MS = 50       # assumed: adapter gain ramp / avatar gaze command applied this long after the decision
DISPATCH_MS = 50   # assumed: scheduler tick + preempt dispatch before the boundary search starts
REPLY_MS = 8000    # synthetic performer reply length
K = 10             # random phases per stream


def boundaries(rng, density):
    if density == "none":
        return [REPLY_MS]
    pts, t = [], 0
    while True:
        t += rng.randint(1200, 2400) if density == "phrase" else 600
        if t >= REPLY_MS:
            break
        pts.append(t)
    return pts + [REPLY_MS]


def run_trace(p, f_start, f_end, cfg, U_ms, onset_ms):
    """Feed frames [f_start, f_end) with performer speaking from trial-time 0. Returns first-tentative time, first
    confirm time, unduck (false) time. Times in trial ms; frame i observed at trial time U + (i+1)*FT - onset."""
    ad = Adapter(None)
    st, own, ev = St(), Own(True, None, True), Ev(3, True, "voice")
    t_tent = t_conf = t_false = None
    for i in range(f_start, f_end):
        o = ad.feed(i, float(p[i]), 0)
        tt = U_ms + o.t_ms - onset_ms
        o = o._replace(t_ms=tt)
        st, d = decide(st, o, own, ev, cfg)
        if d.intr == Intr.TENTATIVE and t_tent is None:
            t_tent = tt
        if d.intr == Intr.CONFIRMED and t_conf is None:
            t_conf = tt
            if t_tent is None:
                t_tent = tt
            break
        if d.intr == Intr.FALSE_RESUME:
            t_false = tt
            break
    return t_tent, t_conf, t_false


def first_run_start(p):
    segs = segments(p)
    return segs[0][0] if segs else None


def burst(p, f0, d_ms):
    q = p.copy()
    cut = f0 + int(round(d_ms / FT))
    q[cut:] = 0.02
    return q


def main(cond):
    R, L = load(cond)
    users = [s for s in L if s["cls"] in ("A", "B1", "B2")]
    C = [s for s in L if s["cls"] == "C"]
    cfgs = [("hard-cancel on first activity (>=96 ms) [OLV / Pipecat-default pattern]", Cfg(hard_cancel=True))]
    for M in (256, 400, 600, 800):
        cfgs.append((f"tentative>=96 ms, confirm at run>={M} ms", Cfg(confirm_run_ms=M)))
    out = []
    for name, cfg in cfgs:
        rng = random.Random(20261108)
        lat = {"tent": [], "conf": [], "stop": {"phrase": [], "fine": [], "none": []}}
        miss = 0; trials = 0
        for s in users:
            p = R[s["id"]]["p"]
            f0 = first_run_start(p)
            if f0 is None:
                continue
            onset_ms = int(round(s["speech"][0][0] * 1000))
            for k in range(K):
                U = rng.randint(300, 6000)
                trials += 1
                t_tent, t_conf, t_false = run_trace(p, max(0, f0 - 12), min(len(p), f0 + 70), cfg, U, onset_ms)
                if t_conf is None:
                    miss += 1; continue
                lat["tent"].append(t_tent + DUCK_MS - U)             # onset(ref) is at trial time U (ref start)
                lat["conf"].append(t_conf - U)
                for dens in ("phrase", "fine", "none"):
                    pts = boundaries(random.Random(rng.random()), dens)
                    t_req = t_conf + DISPATCH_MS
                    stop = next((b for b in pts if b >= t_req), REPLY_MS)
                    lat["stop"][dens].append(stop - U)
        # false-yield on non-speech and on synthetic short bursts
        fy_c = tent_c = 0
        for s in C:
            p = R[s["id"]]["p"]
            tt, tc, tf = run_trace(p, 0, len(p), cfg, 3000, 0)
            tent_c += tt is not None; fy_c += tc is not None
        bres = {}
        for d_ms in (128, 192, 288, 416, 640):
            n = tent = conf = 0; ducked = []
            for s in users:
                p = R[s["id"]]["p"]; f0 = first_run_start(p)
                if f0 is None:
                    continue
                n += 1
                q = burst(p, f0, d_ms)
                tt, tc, tf = run_trace(q, max(0, f0 - 12), min(len(q), f0 + 70), cfg, 3000, int(round(s["speech"][0][0] * 1000)))
                tent += tt is not None; conf += tc is not None
                if tt is not None and tc is None and tf is not None:
                    ducked.append(tf - tt)
            bres[d_ms] = dict(n=n, tentative=tent, confirmed=conf, ducked_p50=pctl(ducked, .5), ducked_n=len(ducked))
        r = dict(name=name, trials=trials, no_confirm=miss,
                 tent_p50=pctl(lat["tent"], .5), tent_p90=pctl(lat["tent"], .9),
                 conf_p50=pctl(lat["conf"], .5), conf_p90=pctl(lat["conf"], .9),
                 stop={k: (pctl(v, .5), pctl(v, .9)) for k, v in lat["stop"].items()},
                 noise=dict(streams=len(C), tentative=tent_c, confirmed=fy_c), bursts=bres)
        out.append(r)
        print(r["name"])
        print(f"  true barge-in trials {trials}, never confirmed {miss}")
        print(f"  duck (perceived yield start) latency from acoustic onset p50/p90 {r['tent_p50']:.0f}/{r['tent_p90']:.0f} ms")
        print(f"  confirm decision latency p50/p90 {r['conf_p50']:.0f}/{r['conf_p90']:.0f} ms")
        print("  performer-audio-stops latency p50/p90 [ms] by boundary density: " + "; ".join(f"{k}: {a:.0f}/{b:.0f}" for k, (a, b) in r["stop"].items()))
        print(f"  non-speech C streams ({len(C)}): tentative {tent_c}, confirmed (false yield) {fy_c}")
        for d_ms, b in bres.items():
            print(f"  burst {d_ms} ms (VAD trace truncated; {b['n']} trials): tentative {b['tentative']}, confirmed(false yield / false cancel) {b['confirmed']}, resumed after ducking, median duck duration {b['ducked_p50']:.0f} ms (n={b['ducked_n']})")
    json.dump(out, open(f"{SCR}/bargein_{cond}.json", "w"), default=str, indent=1)


if __name__ == "__main__":
    print("=== barge-in", sys.argv[1])
    main(sys.argv[1])
