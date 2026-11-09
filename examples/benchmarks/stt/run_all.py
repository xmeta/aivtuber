"""Run the whole matrix of the record, one fresh process per run, sequentially (never in parallel: timings).

  STT_BENCH_DIR=<dir> python -I -X utf8 run_all.py [--skip-existing] [--gpu]

About 3 hours on the machine class of the record, dominated by the Whisper runs.
"""
import os, subprocess, sys

HERE = os.path.dirname(os.path.abspath(__file__))
OUT = os.path.join(os.environ["STT_BENCH_DIR"], "out")

# (engine, threads, n, partial-n, repeat-n, tag)
MATRIX = [
    ("sherpa:k2v2-int8", 4, 400, 60, 100, "k2v2-int8_t4"),
    ("sherpa:k2v2-fp32", 4, 400, 60, 100, "k2v2-fp32_t4"),
    ("sherpa:k2-ja-en", 4, 400, 60, 100, "k2-ja-en_t4"),
    ("sherpa:parakeet-ja", 4, 400, 60, 100, "parakeet-ja_t4"),
    ("sherpa:sensevoice", 4, 400, 60, 100, "sensevoice_t4"),
    ("sherpa:k2v2-int8", 1, 400, 20, 20, "k2v2-int8_t1"),
    ("sherpa:k2v2-int8", 8, 400, 20, 20, "k2v2-int8_t8"),
    ("sherpa:k2v2-int8", 4, 400, 20, 20, "k2v2-int8_t4_run2"),
    ("vosk:small", 4, 200, 30, 50, "vosk-small"),
    ("vosk:small", 4, 60, 10, 20, "vosk-small_rerun"),  # second look at the small model's unexpectedly high RTF
    ("vosk:big", 4, 60, 10, 20, "vosk-big"),  # slower than real time: a reduced set is enough to say so
    ("whisper:small", 8, 150, 8, 10, "whisper-small_t8"),
    ("whisper:medium-q5_0", 8, 150, 8, 10, "whisper-medium-q5_0_t8"),
    ("whisper:large-v3-turbo-q5_0", 8, 150, 8, 10, "whisper-turbo-q5_0_t8"),
    ("whisper:kotoba-whisper-v2.0-q5_0", 8, 60, 4, 5, "kotoba-v2.0-q5_0_t8"),  # 60 clips: large-v3-sized encoder, about 15 s per clip on CPU
    # kotoba fp16 on CPU is the slowest run (about 40 minutes) and was only run on the GPU, see GPU_MATRIX
    ("whisper:small", 4, 40, 4, 10, "whisper-small_t4"),
]

# Optional, only with --gpu (needs fetch_assets.py --with-gpu and an NVIDIA GPU); reported separately from the CPU tables.
GPU_MATRIX = [
    ("whispergpu:kotoba-whisper-v2.0", 4, 150, 8, 10, "gpu-kotoba-v2.0-fp16"),
    ("whispergpu:large-v3-turbo", 4, 150, 8, 10, "gpu-turbo-fp16"),
]

if __name__ == "__main__":
    skip = "--skip-existing" in sys.argv
    for eng, thr, n, pn, rn, tag in MATRIX + (GPU_MATRIX if "--gpu" in sys.argv else []):
        if skip and os.path.exists(os.path.join(OUT, tag + ".json")):
            continue
        subprocess.run([sys.executable, "-I", "-X", "utf8", os.path.join(HERE, "bench.py"), "--engine", eng, "--threads", str(thr),
                        "--n", str(n), "--partial-n", str(pn), "--repeat-n", str(rn), "--tag", tag], check=True)
