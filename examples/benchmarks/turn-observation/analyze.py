import sys, os; sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import json, pickle, math, numpy as np
from lib import *
from vadlib import segments

cond = sys.argv[1]
lab = json.load(open(os.path.join(TB, "audio", "labels.json"), encoding="utf-8"))["streams"]
R = pickle.load(open(os.path.join(TB, "out", f"eval_{cond}.pkl"), "rb"))
FT = FR / SR


def q(t):
    return math.ceil(t / FT - 1e-9) * FT


def ev_fixed(r, dur, T):
    segs = segments(r["p"])
    ev = []
    for gi, (a, b) in enumerate(segs):
        te = b * FT
        ns = segs[gi + 1][0] * FT if gi + 1 < len(segs) else dur
        if ns - te >= T:
            ev.append(q(te + T))
    return ev


def ev_st(r, dur, thr, mode, fb=None):
    segs = segments(r["p"])
    ev = []
    for gi, (a, b) in enumerate(segs):
        te = b * FT
        ns = segs[gi + 1][0] * FT if gi + 1 < len(segs) else dur
        ks = [200] if mode == "single" else list(range(200, 1501, 100))
        fired = None
        for k in ks:
            if fb is not None and k > fb:
                break
            v = r["st"].get((gi, k))
            if v is None:
                continue
            if v[0] >= thr:
                fired = v[1]
                break
        if fired is None and fb is not None and ns - te >= fb / 1000:
            fired = q(te + fb / 1000)
        if fired is not None:
            ev.append(fired)
    return ev


METHODS = {}
for T in (300, 500, 800):
    METHODS[f"fixed-{T}ms (Silero-v6+own hysteresis)"] = lambda r, d, T=T: ev_fixed(r, d, T / 1000)
for mn, mm in (("v4", "sherpa-onnx VAD, Silero v4 export"), ("v6", "sherpa-onnx VAD, Silero v6 model")):
    for T in (0.3, 0.5, 0.8):
        METHODS[f"{mm} min_silence={int(T*1000)}ms"] = lambda r, d, mn=mn, T=T: (r["sherpa"][(mn, T)] or [])
for thr in (0.3, 0.5, 0.7):
    METHODS[f"smart-turn v3.2 single-shot@200ms thr={thr}"] = lambda r, d, thr=thr: ev_st(r, d, thr, "single")
    METHODS[f"smart-turn v3.2 polled/100ms thr={thr}"] = lambda r, d, thr=thr: ev_st(r, d, thr, "poll")
    METHODS[f"smart-turn v3.2 polled thr={thr} + 800ms silence fallback"] = lambda r, d, thr=thr: ev_st(r, d, thr, "poll", 800)


def wilson(k, n):
    if n == 0:
        return (0, 0)
    z = 1.96
    p = k / n
    den = 1 + z * z / n
    c = (p + z * z / (2 * n)) / den
    h = z * math.sqrt(p * (1 - p) / n + z * z / (4 * n * n)) / den
    return c - h, c + h


def pct(k, n):
    lo, hi = wilson(k, n)
    return f"{100*k/max(n,1):.0f}% [{100*lo:.0f}-{100*hi:.0f}]"


out = {}
S = {s["id"]: s for s in lab}
cls_ids = {c: [s["id"] for s in lab if s["cls"] == c] for c in ("A", "B1", "B2", "C")}
holds = cls_ids["B1"] + cls_ids["B2"]
ends = cls_ids["A"] + holds
rows = []
for name, fn in METHODS.items():
    E = {sid: fn(R[sid], S[sid]["dur"]) for sid in R}
    d = dict(method=name)
    # false take on holds: any event before final end
    def ft(ids, pause=None):
        ids = [i for i in ids if pause is None or S[i]["pause_ms"] == pause]
        k = sum(1 for i in ids if any(t < S[i]["final_end"] for t in E[i]))
        return k, len(ids)
    for c in ("B1", "B2"):
        d["ft_" + c] = ft(cls_ids[c])
    d["ft_hold_all"] = ft(holds)
    for P in (300, 500, 700):
        d[f"ft_B_{P}"] = ft(holds, P)
        d[f"ft_B2B3_{P}"] = ft(cls_ids["B2"], P)
    d["ft_A_inspeech"] = ft(cls_ids["A"])
    # true-end detection
    lags = []
    miss = 0
    for i in ends:
        F = S[i]["final_end"]
        after = [t for t in E[i] if t >= F]
        if not after:
            miss += 1
        else:
            lags.append(after[0] - F)
    lags = np.array(lags)
    d["miss"] = (miss, len(ends))
    nofire = sum(1 for i in ends if not E[i])
    early_only = sum(1 for i in ends if E[i] and all(t < S[i]["final_end"] for t in E[i]))
    d["nofire"] = (nofire, len(ends))
    d["early_only"] = (early_only, len(ends))
    d["late1s"] = (int((lags > 1.0).sum()) + miss, len(ends))
    d["late600"] = (int((lags > 0.6).sum()) + miss, len(ends))
    d["late1000"] = d["late1s"]
    d["lag_p50"] = float(np.median(lags)) if len(lags) else None
    d["lag_p90"] = float(np.percentile(lags, 90)) if len(lags) else None
    # A only lag
    la = []
    for i in cls_ids["A"]:
        F = S[i]["final_end"]
        after = [t for t in E[i] if t >= F]
        if after:
            la.append(after[0] - F)
    d["lagA_p50"] = float(np.median(la)) if la else None
    d["lagA_p90"] = float(np.percentile(la, 90)) if la else None
    # non-speech
    d["C_streams_fired"] = (sum(1 for i in cls_ids["C"] if E[i]), len(cls_ids["C"]))
    rows.append(d)
out["rows"] = rows

# VAD behaviour
vad = {}
on, off = [], []
for i in cls_ids["A"]:
    segs = segments(R[i]["p"])
    s = S[i]
    on.append(segs[0][0] * FT - s["speech"][0][0] if segs else np.nan)
    off.append(segs[-1][1] * FT - s["final_end"] if segs else np.nan)
vad["onset_lag_ms_p50_p90"] = [float(np.nanmedian(on) * 1000), float(np.nanpercentile(on, 90) * 1000)]
vad["onset_lag_gt_100ms_frac"] = float(np.mean(np.array(on) > 0.1))
vad["offset_lag_ms_p50_p90"] = [float(np.nanmedian(off) * 1000), float(np.nanpercentile(off, 90) * 1000)]
vad["A_noseg"] = int(sum(np.isnan(on)))
# VAD-observed hold gap
gaps = {300: [], 500: [], 700: []}
for i in holds:
    s = S[i]
    h0, h1 = s["hold"][0]
    segs = segments(R[i]["p"])
    for gi, (a, b) in enumerate(segs[:-1]):
        if h0 - 0.15 <= b * FT <= h0 + 0.4:
            gaps[s["pause_ms"]].append(segs[gi + 1][0] * FT - b * FT)
            break
vad["observed_gap_ms_by_nominal_p10_p50_p90"] = {k: [float(np.percentile(v, p) * 1000) for p in (10, 50, 90)] + [len(v)] for k, v in gaps.items()}
# internal dips: A streams with >1 VAD segment
vad["A_multi_segment_frac"] = float(np.mean([len(segments(R[i]["p"])) > 1 for i in cls_ids["A"]]))
# non speech
act = 0.0
tot = 0.0
bysub = {}
for i in cls_ids["C"]:
    p = R[i]["p"]
    tot += len(p) * FT
    sg = segments(p)
    a = sum((b - a_) for a_, b in sg) * FT
    act += a
    bysub.setdefault(S[i]["sub"], [0, 0, 0.0])
    bysub[S[i]["sub"]][0] += 1
    bysub[S[i]["sub"]][1] += 1 if sg else 0
    bysub[S[i]["sub"]][2] += a
vad["nonspeech_active_s_per_min"] = act / tot * 60
vad["nonspeech_by_sub(n, streams_with_segment, active_s)"] = bysub
out["vad"] = vad
json.dump(out, open(os.path.join(TB, "out", f"analysis_{cond}.json"), "w"), indent=1, default=str)


print("cond", cond, {c: len(v) for c, v in cls_ids.items()})
print(json.dumps(vad, indent=1))
print()
print("TABLE holds (false turn-take = any turn-end event before the true final end)")
print("| method | all holds | pause 300 | 500 | 700 | B1 joined | B2 clause |")
for d in rows:
    print("| {} | {} | {} | {} | {} | {} | {} |".format(d["method"], pct(*d["ft_hold_all"]), pct(*d["ft_B_300"]), pct(*d["ft_B_500"]),
          pct(*d["ft_B_700"]), pct(*d["ft_B1"]), pct(*d["ft_B2"])))
print()
print("TABLE ends (A single utterance + final end of B streams)")
print("| method | A: fired before end (natural in-utterance pause) | no event >= end | no event at all | lag p50 | lag p90 | A-only lag p50/p90 | non-speech streams fired |")
for d in rows:
    f = lambda v: "-" if v is None else str(round(v * 1000))
    print("| {} | {} | {} | {} | {} | {} | {}/{} | {}/{} |".format(d["method"], pct(*d["ft_A_inspeech"]), pct(*d["miss"]), pct(*d["nofire"]),
          f(d["lag_p50"]), f(d["lag_p90"]), f(d["lagA_p50"]), f(d["lagA_p90"]), d["C_streams_fired"][0], d["C_streams_fired"][1]))
