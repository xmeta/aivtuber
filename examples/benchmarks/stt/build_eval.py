"""Build the evaluation set: seeded sample of Common Voice 22.0 ja test clips -> trimmed 16 kHz mono 16-bit WAV.

Run as:  STT_BENCH_DIR=<dir> python -I -X utf8 build_eval.py [N]      (default N = 400)

Selection: only clips present in the extracted tar, up_votes >= 2 and down_votes == 0, one clip per
distinct sentence_id, decoded duration 1.0-12.0 s, seeded shuffle (seed 20261109), first N kept.
Segmenting: the clip is trimmed to a VAD-like segment, i.e. from 0.30 s before the first 20 ms frame
whose RMS exceeds max(-50 dBFS, peak-RMS - 35 dB) to 0.50 s after the last such frame. This stands in
for "Silero segment + pre-roll + silence tail" (S2) without involving the VAD model; section limitations
of the record say what it does and does not mean. Gold text is the Common Voice sentence verbatim.
"""
import csv, glob, json, os, random, sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))  # python -I does not add the script directory
import numpy as np  # noqa: E402
import soundfile as sf  # noqa: E402
from scipy.signal import resample_poly  # noqa: E402

from common import DL, EVAL, SR  # noqa: E402

SEED = 20261109
PRE, TAIL = 0.30, 0.50


def decode16k(path):
    x, sr = sf.read(path, dtype="float32", always_2d=True)
    x = x.mean(1)
    if sr != SR:
        g = np.gcd(sr, SR)
        x = resample_poly(x, SR // g, sr // g).astype(np.float32)
    return x


def segment(x):
    f = int(0.02 * SR)
    n = len(x) // f
    rms = np.sqrt((x[: n * f].reshape(n, f) ** 2).mean(1) + 1e-12)
    db = 20 * np.log10(rms)
    thr = max(-50.0, db.max() - 35.0)
    on = np.flatnonzero(db > thr)
    if len(on) == 0:
        return None
    a = max(0, int(on[0] * f - PRE * SR))
    b = min(len(x), int((on[-1] + 1) * f + TAIL * SR))
    return x[a:b]


def main():
    n_want = int(sys.argv[1]) if len(sys.argv) > 1 else 400
    root = os.path.join(DL, "cv22ja", "x")
    files = {os.path.basename(p): p for p in glob.glob(os.path.join(root, "**", "*.mp3"), recursive=True)}
    rows = []
    with open(os.path.join(DL, "cv22ja", "test.tsv"), encoding="utf8", newline="") as f:
        for r in csv.DictReader(f, delimiter="\t", quoting=csv.QUOTE_NONE):
            if r["path"] in files and int(r["down_votes"]) == 0 and int(r["up_votes"]) >= 2:
                rows.append(r)
    print("candidates", len(rows), "files", len(files))
    random.Random(SEED).shuffle(rows)
    os.makedirs(os.path.join(EVAL, "wav"), exist_ok=True)
    seen, out = set(), []
    for r in rows:
        if len(out) >= n_want:
            break
        if r["sentence_id"] in seen:
            continue
        x = decode16k(files[r["path"]])
        if not (1.0 <= len(x) / SR <= 12.0):
            continue
        seg = segment(x)
        if seg is None or np.abs(seg).max() < 0.01:
            continue
        cid = r["path"].replace(".mp3", "")
        sf.write(os.path.join(EVAL, "wav", cid + ".wav"), seg, SR, subtype="PCM_16")
        seen.add(r["sentence_id"])
        out.append({"id": cid, "wav": f"wav/{cid}.wav", "gold": r["sentence"], "dur": round(len(seg) / SR, 3)})
    with open(os.path.join(EVAL, "eval.json"), "w", encoding="utf8") as f:
        json.dump(out, f, ensure_ascii=False, indent=0)
    d = np.array([o["dur"] for o in out])
    print(f"clips {len(out)} total {d.sum():.1f}s mean {d.mean():.2f}s p50 {np.median(d):.2f}s max {d.max():.2f}s")


if __name__ == "__main__":
    main()
