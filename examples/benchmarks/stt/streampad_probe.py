"""Does a streaming model need trailing zeros before input_finished()? Runs one streaming engine on the first N clips with
several tail paddings and prints CER and the number of empty outputs.

  STT_BENCH_DIR=<dir> python -I -X utf8 streampad_probe.py [--engine sherpastream:nemotron-560] [--n 100]
"""
import argparse, os, sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import numpy as np  # noqa: E402

from common import EVAL, bootstrap_ci, cer_table, load_eval, micro_cer, read_wav16k  # noqa: E402
from engines import make  # noqa: E402

PADS = [0.0, 0.3, 0.66, 1.0, 1.5]

if __name__ == "__main__":
    ap = argparse.ArgumentParser()
    ap.add_argument("--engine", default="sherpastream:nemotron-560")
    ap.add_argument("--n", type=int, default=100)
    ap.add_argument("--threads", type=int, default=4)
    a = ap.parse_args()
    clips = load_eval()[: a.n]
    audio = [read_wav16k(os.path.join(EVAL, c["wav"])) for c in clips]
    for pad in PADS:
        eng = make(f"{a.engine}@pad{pad}", a.threads)
        hyps = [eng.final(x) for x in audio]
        d, n, _, _ = cer_table([c["gold"] for c in clips], hyps)
        lo, hi = bootstrap_ci(d, n)
        print(f"pad {pad:.2f} s: CER {100*micro_cer(d, n):.2f}% ({100*lo:.2f}-{100*hi:.2f}), empty outputs {sum(1 for h in hyps if not h.strip())}")
