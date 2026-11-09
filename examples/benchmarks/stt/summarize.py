"""Read out/*.json and print AsciiDoc tables (accuracy, latency, cost, determinism).

  STT_BENCH_DIR=<dir> python -I -X utf8 summarize.py [tag-substring ...]
"""
import glob, json, os, sys

import numpy as np

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from common import OUT, bootstrap_ci, cer_table, load_eval, micro_cer  # noqa: E402


def pct(v, q):
    return float(np.percentile(v, q)) if len(v) else float("nan")


def main():
    gold = {c["id"]: c["gold"] for c in load_eval()}
    args = sys.argv[1:]
    first = None
    if "--first" in args:  # restrict accuracy to the first N clips so engines run on different n stay comparable
        i = args.index("--first")
        first = int(args[i + 1])
        del args[i:i + 2]
    drop_common_bad = "--drop-common-bad" in args
    args = [a for a in args if a != "--drop-common-bad"]
    want = args
    runs = []
    for p in sorted(glob.glob(os.path.join(OUT, "*.json"))):
        tag = os.path.basename(p)[:-5]
        if want and not any(w in tag for w in want):
            continue
        r = json.load(open(p, encoding="utf8"))
        if first:
            r["rows"] = r["rows"][:first]
        runs.append((tag, r))
    if drop_common_bad:
        # Exploratory view: drop clips on which EVERY selected run exceeds 50% CER (probable wrong label or unusable audio;
        # Common Voice contains both). Applied to all engines identically; it never replaces the all-clips figure.
        from common import normalize as _n, edits as _e
        bad_sets = []
        for _, r in runs:
            s = set()
            for x in r["rows"]:
                d, n = _e(_n(gold[x["id"]]), _n(x["text"]))
                if d / max(n, 1) > 0.5:
                    s.add(x["id"])
            bad_sets.append(s)
        common_bad = set.intersection(*bad_sets) if bad_sets else set()
        print(f"dropped {len(common_bad)} clips on which every selected run exceeds 50% CER")
        for _, r in runs:
            r["rows"] = [x for x in r["rows"] if x["id"] not in common_bad]
    print("== run-to-run agreement (same engine, different tag/threads/process) ==")
    for i, (ta, ra) in enumerate(runs):
        for tb, rb in runs[i + 1:]:
            if ra["engine"] != rb["engine"]:
                continue
            ma = {x["id"]: x["text"] for x in ra["rows"]}
            both = [k for k in ma if k in {x["id"] for x in rb["rows"]}]
            mb = {x["id"]: x["text"] for x in rb["rows"]}
            same = sum(1 for k in both if ma[k] == mb[k])
            print(f"{ta} vs {tb}: identical text on {same}/{len(both)} clips")
    print()
    print("== accuracy ==")
    print("|tag |n |CER % (95% CI) |CER number-insensitive % |clips with CER > 50% |empty outputs")
    for tag, r in runs:
        g = [gold[x["id"]] for x in r["rows"]]
        h = [x["text"] for x in r["rows"]]
        d1, n1, d2, n2 = cer_table(g, h)
        lo, hi = bootstrap_ci(d1, n1)
        per = d1 / n1
        print(f"|{tag} |{len(h)} |{100*micro_cer(d1, n1):.2f} ({100*lo:.2f}-{100*hi:.2f}) |{100*micro_cer(d2, n2):.2f} "
              f"|{int((per > 0.5).sum())} |{sum(1 for t in h if not t.strip())}")
    base_tag = next((os.environ[k] for k in ("STT_BASE",) if k in os.environ), None)
    if base_tag and base_tag in {t for t, _ in runs}:
        print(f"\n== paired CER difference against {base_tag} (same clips; negative = better than the base; bootstrap 95% CI) ==")
        by = {t: r for t, r in runs}
        bm = {x["id"]: x["text"] for x in by[base_tag]["rows"]}
        for tag, r in runs:
            if tag == base_tag:
                continue
            ids = [x["id"] for x in r["rows"] if x["id"] in bm]
            tm = {x["id"]: x["text"] for x in r["rows"]}
            da, na, _, _ = cer_table([gold[i] for i in ids], [tm[i] for i in ids])
            db, nb, _, _ = cer_table([gold[i] for i in ids], [bm[i] for i in ids])
            rng = np.random.default_rng(20261109)
            idx = rng.integers(0, len(ids), size=(2000, len(ids)))
            diff = da[idx].sum(1) / na[idx].sum(1) - db[idx].sum(1) / nb[idx].sum(1)
            print(f"|{tag} |{len(ids)} clips |{100*(da.sum()/na.sum() - db.sum()/nb.sum()):+.2f} pp "
                  f"({100*np.percentile(diff, 2.5):+.2f} to {100*np.percentile(diff, 97.5):+.2f})")
    print("\n== latency and cost (final = whole segment decode call) ==")
    print("|tag |thr |RTF |final p50 ms |p95 ms |max ms |tail-of-stream ms p50/p95 |first call ms |load s |RSS peak MB |same text on re-run")
    for tag, r in runs:
        f = np.array([x["final_s"] for x in r["rows"]]) * 1000
        t = r.get("tail_s")
        ts = f"{pct(np.array(t)*1000, 50):.0f}/{pct(np.array(t)*1000, 95):.0f}" if t else "-"
        print(f"|{tag} |{r['threads']} |{r['rtf']:.3f} |{pct(f, 50):.0f} |{pct(f, 95):.0f} |{f.max():.0f} |{ts} "
              f"|{r['first_call_s']*1000:.0f} |{r['load_s']:.2f} |{r['rss_peak_mb']:.0f} |{r['repeat_same']}/{r['repeat_n']}")
    print("\n== partials (emulated every step for offline engines; 100 ms chunks for streaming engines) ==")
    print("|tag |clips |time to first non-empty partial after speech onset, p50 ms |p95 ms |partial call cost p50 ms |p95 ms |max ms |step s")
    for tag, r in runs:
        first, cost = [], []
        for c in r["partials"]:
            p = c["p"]
            cost += [x[2] for x in p]
            for t, txt, cs in p:
                if txt.strip():
                    first.append(t - r["onset_s"] + cs)
                    break
        cost = np.array(cost) * 1000
        step = "0.1 (stream)" if r["engine"].startswith("vosk") else r["partial_step"]
        print(f"|{tag} |{len(r['partials'])} |{pct(np.array(first)*1000, 50):.0f} |{pct(np.array(first)*1000, 95):.0f} "
              f"|{pct(cost, 50):.0f} |{pct(cost, 95):.0f} |{cost.max():.0f} |{step}")
    print("\n== partial stability (non-empty partials before the last one) ==")
    print("|tag |partials |share that is a prefix of the last partial (after normalisation) |mean partials per clip")
    from common import normalize as _norm
    for tag, r in runs:
        tot = pre = 0
        for c in r["partials"]:
            texts = [_norm(t) for _, t, _ in c["p"]]
            last = texts[-1]
            for t in texts[:-1]:
                if t:
                    tot += 1
                    pre += last.startswith(t)
        n = max(len(r["partials"]), 1)
        print(f"|{tag} |{tot} |{(100 * pre / tot if tot else float('nan')):.1f}% |{sum(len(c['p']) for c in r['partials']) / n:.1f}")


if __name__ == "__main__":
    main()
