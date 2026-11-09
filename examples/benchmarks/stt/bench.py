"""Run one engine over the evaluation set and write out/<tag>.json.

  STT_BENCH_DIR=<dir> python -I -X utf8 bench.py --engine sherpa:k2v2-int8 --threads 4 [--n 400] [--partial-n 60]
        [--repeat-n 100] [--tag name]

Run each engine in its own fresh process (load time and peak RSS are per process). Quiet machine, nothing else heavy running.
Measured per clip: wall time of the final-transcription call (`final_s`), text. Then, on the first --partial-n clips,
the partial emulation of engines.py. Then the first --repeat-n clips again in the same process to check that the same
input gives the same text.
"""
import argparse, json, os, sys, time

import numpy as np
import psutil

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from common import OUT, SR, load_eval, read_wav16k  # noqa: E402
from engines import make  # noqa: E402

ONSET_S = 0.30  # build_eval.py puts the first speech frame ~0.30 s into every segment


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--engine", required=True)
    ap.add_argument("--threads", type=int, default=4)
    ap.add_argument("--n", type=int, default=400)
    ap.add_argument("--partial-n", type=int, default=60)
    ap.add_argument("--partial-step", type=float, default=0.5)
    ap.add_argument("--repeat-n", type=int, default=100)
    ap.add_argument("--tag")
    a = ap.parse_args()
    tag = a.tag or f"{a.engine.replace(':', '_')}_t{a.threads}"
    clips = load_eval()[: a.n]
    rss0 = psutil.Process().memory_info().rss / 2**20
    eng = make(a.engine, a.threads)
    rss_loaded = (psutil.Process().memory_info().rss / 2**20) if hasattr(eng, "r") or hasattr(eng, "m") else None
    audio = [read_wav16k(os.path.join(os.path.dirname(OUT), "eval", c["wav"])) for c in clips]

    # The very first call is timed separately (lazy initialisation); three more calls warm up and are not recorded.
    t0 = time.perf_counter()
    eng.final(audio[0])
    first_call_s = time.perf_counter() - t0
    for x in audio[1:4]:
        eng.final(x)

    rows, wall0 = [], time.perf_counter()
    for c, x in zip(clips, audio):
        t0 = time.perf_counter()
        text = eng.final(x)
        rows.append({"id": c["id"], "text": text, "final_s": time.perf_counter() - t0, "dur": len(x) / SR})
    wall = time.perf_counter() - wall0
    audio_s = sum(r["dur"] for r in rows)

    tail = None
    if hasattr(eng, "tail_cost"):
        tail = [eng.tail_cost(x)[1] for x in audio]

    parts = []
    for c, x in zip(clips[: a.partial_n], audio[: a.partial_n]):
        parts.append({"id": c["id"], "p": [(round(t, 3), txt, round(cost, 5)) for t, txt, cost in eng.partials(x, a.partial_step)]})

    rep = [eng.final(x) for x in audio[: a.repeat_n]]
    same = sum(1 for r, row in zip(rep, rows[: a.repeat_n]) if r == row["text"])

    gpu_mem = None
    if a.engine.startswith("whispergpu"):  # whole-GPU memory in use while the server still holds the model
        import subprocess
        gpu_mem = subprocess.run(["nvidia-smi", "--query-gpu=memory.used", "--format=csv,noheader,nounits"],
                                 capture_output=True, text=True).stdout.strip()
    res = {
        "gpu_mem_used_mb_whole_gpu": gpu_mem,
        "engine": a.engine, "threads": a.threads, "n": len(rows), "audio_s": audio_s, "wall_s": wall, "rtf": wall / audio_s,
        "load_s": eng.load_s, "first_call_s": first_call_s, "rss_baseline_mb": rss0, "rss_loaded_mb": rss_loaded,
        "rss_peak_mb": eng.rss_peak_mb(), "rows": rows, "tail_s": tail, "partials": parts, "partial_step": a.partial_step,
        "onset_s": ONSET_S, "repeat_same": same, "repeat_n": len(rep),
    }
    os.makedirs(OUT, exist_ok=True)
    with open(os.path.join(OUT, tag + ".json"), "w", encoding="utf8") as f:
        json.dump(res, f, ensure_ascii=False)
    fs = np.array([r["final_s"] for r in rows])
    print(f"{tag}: n={len(rows)} rtf={res['rtf']:.4f} load={res['load_s']:.2f}s final p50={np.median(fs)*1000:.0f}ms "
          f"p95={np.percentile(fs, 95)*1000:.0f}ms rss_peak={res['rss_peak_mb']:.0f}MB repeat_same={same}/{len(rep)}")
    eng.close()


if __name__ == "__main__":
    main()
