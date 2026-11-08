import sys, os; sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import json, numpy as np, soundfile as sf
from lib import *

SEED = 20261108
rng = np.random.default_rng(SEED)
clips = json.load(open(os.path.join(TB, "audio", "clips.json"), encoding="utf-8"))
asr = json.load(open(os.path.join(TB, "audio", "asr_all.json"), encoding="utf-8"))
OUT = os.path.join(TB, "audio", "streams")
os.makedirs(OUT, exist_ok=True)
TARGET_RMS = 0.07
LEAD = 0.4
TAIL = 0.04
GAP = 2.5
cache = {}


def get(k):
    if k not in cache:
        x = load16k(os.path.join(TB, "audio", "clips", k + ".wav"))
        c = clips[k]
        g = TARGET_RMS / rms_speech(x, (c["start"], c["end"]))
        x = x * g
        cache[k] = (x, floor_window(x))
    return cache[k]


def seg(x, a, b, fl):
    """x[a:b] seconds; pad front with room tone if a<0."""
    out = []
    if a < 0:
        out.append(tile_floor(fl, int(-a * SR), rng))
        a = 0
    out.append(x[int(a * SR):int(b * SR)])
    return np.concatenate(out)


def save(sid, y, meta):
    y = y + pink(len(y), rng) * 0.001  # -60 dBFS pink room-tone floor
    pk = float(np.abs(y).max())
    if pk > 0.99:
        y = y * (0.99 / pk)
    sf.write(os.path.join(OUT, sid + ".wav"), y.astype(np.float32), SR, subtype="FLOAT")
    meta["id"] = sid
    meta["dur"] = len(y) / SR
    return meta


streams = []
# ---------- A: true turn ends
A = [k for k, v in clips.items() if v["role"] == "A"]
for i, k in enumerate(A):
    x, fl = get(k)
    c = clips[k]
    y = np.concatenate([seg(x, c["start"] - LEAD, c["end"] + TAIL, fl), tile_floor(fl, int(GAP * SR), rng)])
    e = LEAD + (c["end"] - c["start"])
    streams.append(save(f"A_{i:03d}", y, dict(cls="A", pause_ms=None, speech=[[LEAD, e]], hold=[], final_end=e, src=[k])))

# ---------- B1: two clips from same speaker, joined with pause
by_client = {}
for k, v in clips.items():
    if v["role"] == "B1":
        by_client.setdefault(v["client"], []).append(k)
pairs = []
for cl, ks in by_client.items():
    rng.shuffle(ks)
    for j in range(0, len(ks) - 1, 2):
        pairs.append((ks[j], ks[j + 1]))
rng.shuffle(pairs)
n_clients_with_pairs = len({clips[a]["client"] for a, b in pairs})
print("B1 pairs available", len(pairs), "speakers", n_clients_with_pairs)
if len(pairs) < 150:
    # fallback: use leftover single clips of any speaker role B1 are not allowed; report shortage
    print("WARNING: only", len(pairs), "same-speaker pairs")
PAUSES = [300, 500, 700]
for i, (ka, kb) in enumerate(pairs[:150]):
    P = PAUSES[i % 3] / 1000
    xa, fa = get(ka)
    xb, fb = get(kb)
    ca, cb = clips[ka], clips[kb]
    sa = seg(xa, ca["start"] - LEAD, ca["end"] + TAIL, fa)
    mid = tile_floor(fa, int((P - TAIL - 0.10) * SR), rng)
    sb = seg(xb, cb["start"] - 0.10, cb["end"] + TAIL, fb)
    y = np.concatenate([fadeout(sa, 8), mid, fadein(sb, 8), tile_floor(fb, int(GAP * SR), rng)])
    ea = LEAD + (ca["end"] - ca["start"])
    sb0 = ea + P
    eb = sb0 + (cb["end"] - cb["start"])
    streams.append(save(f"B1_{i:03d}", y, dict(cls="B1", pause_ms=int(P * 1000), speech=[[LEAD, ea], [sb0, eb]],
                                              hold=[[ea, sb0]], final_end=eb, src=[ka, kb])))

# ---------- B2: pause at a written clause boundary (CV punctuation + natural dip)
# A Common Voice sentence contains 「<clause ending in けど/から/ので/て/が>、」. The comma's expected time is
# proportional to its character position inside the energy-defined speech span; the natural sub-threshold
# dip (>=80 ms) nearest to that time (accepted only within 12% of the utterance duration) is replaced by P ms
# of room tone. So the pause is placed at a real prosodic break, not a fabricated cut inside a word.
import re
PART = ("けれども", "けれど", "けど", "から", "ので", "のに", "が", "て")
used_ab = {s["src"][0] for s in streams if s["cls"] == "A"} | {k for s in streams if s["cls"] == "B1" for k in s["src"]}
b2 = []
for k, v in clips.items():
    if k in used_ab:
        continue
    sent = v["sentence"]
    if sent.count("、") > 2:
        continue
    plain = re.sub(r"[、。！？「」\s]", "", sent)
    pos = [i for i, ch in enumerate(sent) if ch == "、"]
    cand_m = None
    for i in pos:
        before = sent[:i]
        if any(before.endswith(p) for p in PART):
            m = len(re.sub(r"[、。！？「」\s]", "", before))
            if 3 <= m <= len(plain) - 4:
                cand_m = (m, [p for p in PART if before.endswith(p)][0])
                break
    if cand_m is None:
        continue
    m, part = cand_m
    x, fl = get(k)
    db = frame_db(x)
    thr = np.percentile(db, 95) - 32
    sp = db > thr
    s0, e0 = v["start"], v["end"]
    dips = []
    i = int((s0 + 0.3) / 0.01)
    iend = int((e0 - 0.3) / 0.01)
    while i < iend:
        if not sp[i]:
            j = i
            while j < iend and not sp[j]:
                j += 1
            if j - i >= 8:
                dips.append((i * 0.01, j * 0.01))
            i = j
        else:
            i += 1
    if not dips:
        continue
    t_exp = s0 + (m / len(plain)) * (e0 - s0)
    d = min(dips, key=lambda d: abs((d[0] + d[1]) / 2 - t_exp))
    if abs((d[0] + d[1]) / 2 - t_exp) > 0.15 * (e0 - s0):
        continue
    if d[0] - s0 < 0.6 or e0 - d[1] < 0.6:
        continue
    b2.append((k, part, d[0], d[1], d[1] - d[0]))
rng.shuffle(b2)
print("B2 candidates", len(b2))
for i, (k, part, d0, d1, dl) in enumerate(b2[:150]):
    P = PAUSES[i % 3] / 1000
    x, fl = get(k)
    v = clips[k]
    s1 = seg(x, v["start"] - LEAD, d0, fl)
    s2 = x[int(d1 * SR):int((v["end"] + TAIL) * SR)]
    y = np.concatenate([fadeout(s1, 5), tile_floor(fl, int(P * SR), rng), fadein(s2, 5), tile_floor(fl, int(GAP * SR), rng)])
    c0 = LEAD + (d0 - v["start"])
    e = c0 + P + (v["end"] - d1)
    streams.append(save(f"B2_{i:03d}", y, dict(cls="B2", pause_ms=int(P * 1000), particle=part, natural_dip_ms=int(dl * 1000),
                                              speech=[[LEAD, c0], [c0 + P, e]], hold=[[c0, c0 + P]], final_end=e, src=[k])))

# ---------- C: non-speech
Ckeys = A[:40]
n = 6 * SR
ci = 0
for i in range(30):  # real mic room tone
    x, fl = get(Ckeys[i % len(Ckeys)])
    y = tile_floor(fl, n, rng)
    streams.append(save(f"C_{ci:03d}", y, dict(cls="C", sub="room_tone", speech=[], hold=[], final_end=None, src=[Ckeys[i % len(Ckeys)]]))); ci += 1
for sub, gen in [("white", lambda: rng.standard_normal(n)), ("pink", lambda: pink(n, rng))]:
    for i in range(12):
        lvl = 10 ** (rng.uniform(-48, -26) / 20)
        y = (gen() * lvl).astype(np.float32)
        streams.append(save(f"C_{ci:03d}", y, dict(cls="C", sub=sub, speech=[], hold=[], final_end=None, src=[]))); ci += 1
for i in range(10):  # mains hum
    t = np.arange(n) / SR
    f0 = rng.choice([50.0, 60.0])
    y = sum(np.sin(2 * np.pi * f0 * h * t) / h for h in range(1, 6)) * 10 ** (rng.uniform(-40, -25) / 20)
    streams.append(save(f"C_{ci:03d}", y.astype(np.float32), dict(cls="C", sub="hum", speech=[], hold=[], final_end=None, src=[]))); ci += 1
for i in range(14):  # clicks / keyboard-like impulsive bursts over room tone
    x, fl = get(Ckeys[(i + 7) % len(Ckeys)])
    y = tile_floor(fl, n, rng)
    for _ in range(rng.integers(3, 7)):
        L = int(rng.uniform(0.005, 0.03) * SR)
        p = rng.integers(SR // 2, n - L - 1)
        env = np.exp(-np.arange(L) / (L / 4))
        y[p:p + L] += (rng.standard_normal(L) * env * 10 ** (rng.uniform(-22, -10) / 20)).astype(np.float32)
    streams.append(save(f"C_{ci:03d}", y, dict(cls="C", sub="clicks", speech=[], hold=[], final_end=None, src=[]))); ci += 1
for i in range(10):  # beeps / whistles (tonal, non-speech)
    t = np.arange(n) / SR
    y = np.zeros(n, np.float32)
    for _ in range(rng.integers(1, 4)):
        f = rng.uniform(500, 3000)
        p = rng.integers(SR // 2, n - SR)
        L = int(rng.uniform(0.2, 0.8) * SR)
        env = np.hanning(L)
        y[p:p + L] += (np.sin(2 * np.pi * f * t[:L]) * env * 10 ** (rng.uniform(-30, -15) / 20)).astype(np.float32)
    streams.append(save(f"C_{ci:03d}", y, dict(cls="C", sub="tone", speech=[], hold=[], final_end=None, src=[]))); ci += 1

json.dump(dict(seed=SEED, target_rms=TARGET_RMS, lead=LEAD, tail=TAIL, gap=GAP, streams=streams),
          open(os.path.join(TB, "audio", "labels.json"), "w", encoding="utf-8"), ensure_ascii=False, indent=0)
from collections import Counter
print(Counter((s["cls"], s.get("pause_ms")) for s in streams), "total dur min", sum(s["dur"] for s in streams) / 60)
