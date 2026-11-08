import sys, os; sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import json, numpy as np
from lib import *
from vadlib import *

clips = json.load(open(os.path.join(TB, "audio", "clips_raw.json"), encoding="utf-8"))
asr = json.load(open(os.path.join(TB, "audio", "asr_all.json"), encoding="utf-8"))
V = SileroOrt(SILERO_V6)
HOP = 0.01
out = {}
drop = {"lowsim": 0, "nochain": 0, "dur": 0, "vad_disagree": 0}


def chain_bounds(db):
    thr = np.percentile(db, 95) - 32
    sp = db > thr
    runs = []
    i, n = 0, len(sp)
    while i < n:
        if sp[i]:
            j = i
            while j < n and sp[j]:
                j += 1
            if j - i >= 12:  # >=120 ms
                runs.append([i, j])
            i = j
        else:
            i += 1
    if not runs:
        return None
    chains = [[runs[0][0], runs[0][1], runs[0][1] - runs[0][0]]]
    for r in runs[1:]:
        if r[0] - chains[-1][1] < 40:  # <400 ms gap: same chain
            chains[-1][1] = r[1]
            chains[-1][2] += r[1] - r[0]
        else:
            chains.append([r[0], r[1], r[1] - r[0]])
    c = max(chains, key=lambda c: c[2])
    return c[0] * HOP, (c[1] - 1) * HOP + 0.025, thr


for k, v in clips.items():
    a = asr[k]
    if a["sim"] < 0.7:
        drop["lowsim"] += 1; continue
    x = load16k(os.path.join(TB, "audio", "clips", k + ".wav"))
    b = chain_bounds(frame_db(x))
    if b is None:
        drop["nochain"] += 1; continue
    s, e, thr = b
    if not (1.2 <= e - s <= 9.0):
        drop["dur"] += 1; continue
    p = run_probs(V, x)
    sg = segments(p)
    if not sg:
        drop["vad_disagree"] += 1; continue
    vs, ve = sg[0][0] * FR / SR, sg[-1][1] * FR / SR
    if abs(vs - s) > 0.4 or abs(ve - e) > 0.4:
        drop["vad_disagree"] += 1; continue
    v2 = dict(v); v2["start"] = s; v2["end"] = e; v2["vad_start"] = vs; v2["vad_end"] = ve
    out[k] = v2
print("kept", len(out), "of", len(clips), drop)
from collections import Counter
print(Counter(v["role"] for v in out.values()))
json.dump(out, open(os.path.join(TB, "audio", "clips.json"), "w", encoding="utf-8"), ensure_ascii=False)
