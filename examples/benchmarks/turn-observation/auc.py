import sys, os; sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import json, pickle, numpy as np
from lib import *
from vadlib import segments
cond = sys.argv[1]
lab = {s["id"]: s for s in json.load(open(os.path.join(TB, "audio", "labels.json"), encoding="utf-8"))["streams"]}
R = pickle.load(open(os.path.join(TB, "out", f"eval_{cond}.pkl"), "rb"))
FT = FR / SR


def auc(pos, neg):
    pos, neg = np.array(pos), np.array(neg)
    if len(pos) == 0 or len(neg) == 0:
        return float("nan")
    allv = np.concatenate([pos, neg])
    ranks = allv.argsort().argsort() + 1.0
    # average ties
    for v in np.unique(allv):
        m = allv == v
        if m.sum() > 1:
            ranks[m] = ranks[m].mean()
    return (ranks[: len(pos)].sum() - len(pos) * (len(pos) + 1) / 2) / (len(pos) * len(neg))


res = {}
for k in (200, 300, 500, 700):
    ends, hold = [], {"B1": [], "B2": []}
    for sid, r in R.items():
        s = lab[sid]
        if s["cls"] == "C":
            continue
        segs = segments(r["p"])
        if not segs:
            continue
        F = s["final_end"]
        gi = len(segs) - 1
        if segs[gi][1] * FT >= F - 0.15:
            v = r["st"].get((gi, k))
            if v:
                ends.append(v[0])
        if s["cls"] in hold:
            h0 = s["hold"][0][0]
            for gi2, (a, b) in enumerate(segs[:-1]):
                if h0 - 0.15 <= b * FT <= h0 + 0.4:
                    v = r["st"].get((gi2, k))
                    if v:
                        hold[s["cls"]].append(v[0])
                    break
    allh = sum(hold.values(), [])
    res[k] = dict(n_end=len(ends), p_end_median=float(np.median(ends)), p_end_p10=float(np.percentile(ends, 10)),
                  **{f"n_{c}": len(v) for c, v in hold.items()},
                  **{f"p_{c}_median": float(np.median(v)) if v else None for c, v in hold.items()},
                  **{f"p_{c}_p90": float(np.percentile(v, 90)) if v else None for c, v in hold.items()},
                  auc_all=auc(ends, allh), **{f"auc_{c}": auc(ends, v) for c, v in hold.items()})
    # fraction of probabilities > .9 / <.1 (bimodality)
    both = np.array(ends + allh)
    res[k]["frac_gt_0.9"] = float((both > 0.9).mean()); res[k]["frac_lt_0.1"] = float((both < 0.1).mean())
print(json.dumps(res, indent=1))
json.dump(res, open(os.path.join(TB, "out", f"auc_{cond}.json"), "w"), indent=1)
