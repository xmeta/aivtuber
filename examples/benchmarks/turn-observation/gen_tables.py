import sys, os, json, math; sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
TB = os.environ["TURN_BENCH_DIR"]  # work directory, outside the repository


def wilson(k, n):
    if n == 0:
        return 0, 0
    z = 1.96
    p = k / n
    den = 1 + z * z / n
    c = (p + z * z / (2 * n)) / den
    h = z * math.sqrt(p * (1 - p) / n + z * z / (4 * n * n)) / den
    return max(0, c - h), min(1, c + h)


def pct(kn):
    k, n = kn
    lo, hi = wilson(k, n)
    return f"{100*k/max(n,1):.0f}% ({k}/{n}; {100*lo:.0f}-{100*hi:.0f})"


def short(name):
    return (name.replace(" (Silero-v6+own hysteresis)", "").replace("sherpa-onnx VAD, Silero v4 export", "sherpa-onnx VAD (Silero v4 export)")
            .replace("sherpa-onnx VAD, Silero v6 model", "sherpa-onnx VAD (Silero v6)").replace("smart-turn v3.2 ", "smart-turn v3.2 "))


def holds(cond, only=None):
    rows = json.load(open(os.path.join(TB, "out", f"analysis_{cond}.json")))["rows"]
    L = ['[cols="4,2,2,2,2,2,2",options="header"]', "|===", "|Method |All holds |Pause 300 ms |Pause 500 ms |Pause 700 ms |B1 joined |B2 clause"]
    for d in rows:
        if only and not only(d["method"]):
            continue
        L.append(f"|{short(d['method'])} |{pct(d['ft_hold_all'])} |{pct(d['ft_B_300'])} |{pct(d['ft_B_500'])} |{pct(d['ft_B_700'])} |{pct(d['ft_B1'])} |{pct(d['ft_B2'])}")
    L.append("|===")
    return "\n".join(L)


def ends(cond, only=None):
    rows = json.load(open(os.path.join(TB, "out", f"analysis_{cond}.json")))["rows"]
    f = lambda v: "-" if v is None else str(round(v * 1000))
    L = ['[cols="4,2,2,2,1,1,2,2",options="header"]', "|===",
         "|Method |A: fired before end (natural pause) |No event at/after end |No event at all |Lag p50 (ms) |Lag p90 (ms) |Lag > 1 s or none |Non-speech streams fired"]
    for d in rows:
        if only and not only(d["method"]):
            continue
        L.append(f"|{short(d['method'])} |{pct(d['ft_A_inspeech'])} |{pct(d['miss'])} |{pct(d['nofire'])} |{f(d["lag_p50"])} |{f(d["lag_p90"])} |{pct(d["late1000"])} |{d['C_streams_fired'][0]}/{d['C_streams_fired'][1]}")
    L.append("|===")
    return "\n".join(L)


noisy_only = lambda m: m.startswith("fixed") or ("thr=0.5" in m and "smart" in m) or "min_silence=500" in m
out = dict(HOLDS_CLEAN=holds("clean"), ENDS_CLEAN=ends("clean"), HOLDS_NOISY=holds("noisy15", noisy_only), ENDS_NOISY=ends("noisy15", noisy_only))
json.dump(out, open(os.path.join(TB, "out", "tables.json"), "w"), ensure_ascii=False, indent=1)
print(out["HOLDS_NOISY"])
