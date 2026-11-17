"""Scores raw_jev.json against EXP-0007's per-line A (and B') predictions on the same 124-line eval split."""
import json, random, sys, collections
d = sys.argv[1].rstrip("/\\") + "/"
C = ["neutral", "joy", "surprise", "calm", "sadness", "anger"]
raw = {r["id"]: r for r in json.load(open(d + "raw_jev.json", encoding="utf-8"))}
cls = [r for r in json.load(open(d + "raw_classification.json", encoding="utf-8")) if not r["dev"]]
gold = [r["gold"] for r in cls]; A = [r["A"] for r in cls]; Bp = [r["B"] for r in cls]
B = [raw[r["id"]]["pred"] for r in cls]
def f1(g, p):
    s = 0
    for c in C:
        tp = sum(a == c and b == c for a, b in zip(g, p)); fp = sum(a != c and b == c for a, b in zip(g, p)); fn = sum(a == c and b != c for a, b in zip(g, p))
        s += 0 if tp == 0 else 2 * tp / (2 * tp + fp + fn)
    return s / 6
def m(g, p):
    neu = [(a, b) for a, b in zip(g, p) if a == "neutral"]
    return dict(n=len(g), acc=round(sum(a == b for a, b in zip(g, p)) / len(g), 3), macro_f1=round(f1(g, p), 3),
                fnn=round(sum(b not in ("neutral", None) for _, b in neu) / len(neu), 3), fnn_k=sum(b not in ("neutral", None) for _, b in neu), neu=len(neu),
                fail=sum(b is None for b in p))
def boot(g, x, y, n=2000):
    rng = random.Random(20261118); ds = []
    for _ in range(n):
        ix = [rng.randrange(len(g)) for _ in g]
        ds.append(f1([g[i] for i in ix], [x[i] for i in ix]) - f1([g[i] for i in ix], [y[i] for i in ix]))
    ds.sort(); return round(f1(g, x) - f1(g, y), 3), round(ds[int(.025 * n)], 3), round(ds[int(.975 * n) - 1], 3)
def pct(xs, q):
    xs = sorted(xs); k = (len(xs) - 1) * q; f = int(k); c = min(f + 1, len(xs) - 1); return xs[f] + (xs[c] - xs[f]) * (k - f)
res = {"A": m(gold, A), "Bprime": m(gold, Bp), "B_jev": m(gold, B), "gain_B_vs_A": boot(gold, B, A), "gain_Bprime_vs_A": boot(gold, Bp, A)}
ok = [i for i, r in enumerate(cls) if B[i] is not None]
res["B_jev_on_answered"] = m([gold[i] for i in ok], [B[i] for i in ok])
# upper bound: treat failures as if correct
res["B_jev_failures_as_correct_macroF1"] = round(f1(gold, [g if b is None else b for g, b in zip(gold, B)]), 3)
errs = collections.Counter(raw[r["id"]]["err"] for r in cls); res["errors_eval"] = dict(errs)
allr = list(raw.values()); res["errors_all144"] = dict(collections.Counter(r["err"] for r in allr))
ev = [raw[r["id"]] for r in cls]
res["lat_eval_all_ms"] = [round(pct([r["ms"] for r in ev], q)) for q in (.5, .95)] + [round(max(r["ms"] for r in ev))]
res["lat_eval_200_ms"] = [round(pct([r["ms"] for r in ev if r["status"] == 200], q)) for q in (.5, .95)]
res["lat_all144_200_ms"] = [round(pct([r["ms"] for r in allr if r["status"] == 200], q)) for q in (.5, .95)]
res["conf_B_jev"] = {f"{g}->{p}": n for (g, p), n in sorted(collections.Counter(zip(gold, B)).items(), key=str) if g != p}
tr = collections.defaultdict(list)
for r, g, b in zip(cls, gold, B): tr[r["trap"] or "none"].append(g == b)
res["acc_by_trap"] = {k: [round(sum(v) / len(v), 2), len(v)] for k, v in tr.items()}
dev = [r for r in json.load(open(d + "raw_classification.json", encoding="utf-8")) if r["dev"]]
res["B_jev_on_dev20"] = m([r["gold"] for r in dev], [raw[r["id"]]["pred"] for r in dev])
print(json.dumps(res, ensure_ascii=False, indent=1))
json.dump(res, open(d + "results_jev.json", "w"), indent=1)
