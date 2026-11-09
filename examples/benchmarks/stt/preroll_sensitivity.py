"""How much does a clipped speech onset cost? Re-run one engine on the first N clips with the segment start moved.

  STT_BENCH_DIR=<dir> python -I -X utf8 preroll_sensitivity.py [--engine sherpa:k2v2-int8] [--n 200]

The onset of each clip is the first 20 ms frame above the same energy threshold build_eval.py uses. The segment is cut to start at
onset + shift for shift in SHIFTS_MS (negative = pre-roll kept before the onset, positive = speech onset already lost). This
emulates a VAD that reports the start late by `shift` ms with no pre-roll (shift > 0), or with a pre-roll of `-shift` ms (shift < 0).
Read together with the VAD onset-lag distribution of EXP-0001 (p50 48 ms, p90 112-144 ms, 12.7% of clean utterances above 100 ms).
"""
import argparse, os, sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import numpy as np  # noqa: E402

from common import SR, bootstrap_ci, cer_table, load_eval, micro_cer, read_wav16k, EVAL  # noqa: E402
from engines import make  # noqa: E402

SHIFTS_MS = [-300, -150, -100, -50, 0, 50, 100, 150, 200, 300]


def onset_index(x):
    f = int(0.02 * SR)
    n = len(x) // f
    db = 20 * np.log10(np.sqrt((x[: n * f].reshape(n, f) ** 2).mean(1) + 1e-12))
    thr = max(-50.0, db.max() - 35.0)
    return int(np.flatnonzero(db > thr)[0] * f)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--engine", default="sherpa:k2v2-int8")
    ap.add_argument("--threads", type=int, default=4)
    ap.add_argument("--n", type=int, default=200)
    a = ap.parse_args()
    clips = load_eval()[: a.n]
    eng = make(a.engine, a.threads)
    audio = [read_wav16k(os.path.join(EVAL, c["wav"])) for c in clips]
    onsets = [onset_index(x) for x in audio]
    print(f"engine {a.engine} threads {a.threads} clips {len(clips)}; median onset in segment {np.median(onsets)/SR*1000:.0f} ms")
    for sh in SHIFTS_MS:
        hyps = []
        for x, o in zip(audio, onsets):
            start = max(0, o + int(sh * SR / 1000))
            hyps.append(eng.final(x[start:]))
        d1, n1, _, _ = cer_table([c["gold"] for c in clips], hyps)
        lo, hi = bootstrap_ci(d1, n1)
        print(f"start at onset {sh:+5d} ms: CER {100*micro_cer(d1, n1):5.2f}% (95% CI {100*lo:.2f}-{100*hi:.2f})")
    eng.close()


if __name__ == "__main__":
    main()
