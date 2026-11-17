"""One Irodori-TTS checkpoint, one fresh process: load, first call, warm-ups, 50 replies twice, handling set once.

Throwaway measurement script behind docs/experiments/EXP-0006-irodori-tts-small-voices.adoc (#409).
Not production code, imported by nothing, not run by CI.

It writes result.json in the same shape as examples/benchmarks/tts/bench.py, so the evaluators and the
summariser of the EXP-0003 bench work unchanged on these runs:

  python -I -X utf8 ../tts/eval_stt.py      <engine> <tag>
  python -I -X utf8 ../tts/eval_reading.py  <engine> <tag>
  python -I -X utf8 ../tts/summarize.py

Run:  python -I -X utf8 bench.py --engine sun-cpu-bf16 --model SUN --device cpu --model-dtype bf16 \
          --codec-dtype fp32 [--tag run1] [--limit 3]

Environment (same convention as the EXP-0003 bench): TTS_BENCH_DIR names a directory outside the
repository holding dl/, models/ and out/; TTS_OUT_DIR defaults to $TTS_BENCH_DIR/out.
The official Irodori-TTS code (patched with code_patches/cpu_bf16_ode_fp32.patch) must be on
PYTHONPATH or named by --code-root.
"""
import argparse, os, sys, threading, time

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
sys.path.insert(0, os.path.join(HERE, "..", "tts"))  # -I removes the script dir from sys.path

import numpy as np
import psutil
import soundfile as sf
import torch

from common import WORK, dump_json, pcm16_sha256, run_dir
from corpus import HANDLING, REPLIES

THREADS = 4  # CPU rows: 4 threads, as in EXP-0003 and in the model card's benchmark
SEED = 1234
_REQUEST = None  # SamplingRequest, imported in main (python -I ignores PYTHONPATH)


class Peak:
    """Peak resident set of this process tree (RSS) and peak CUDA allocation, sampled every 25 ms."""

    def __init__(self, pid):
        self.root, self.rss_peak, self.vram_peak, self._stop = psutil.Process(pid), 0, 0, False
        self.t = threading.Thread(target=self._run, daemon=True)
        self.t.start()

    def now(self):
        try:
            procs = [self.root] + self.root.children(recursive=True)
            return sum(p.memory_info().rss for p in procs)
        except psutil.Error:
            return 0

    def _run(self):
        while not self._stop:
            self.rss_peak = max(self.rss_peak, self.now())
            if torch.cuda.is_available():
                self.vram_peak = max(self.vram_peak, torch.cuda.memory_allocated())
            time.sleep(0.025)

    def stop(self):
        self._stop = True
        self.t.join()
        return self.rss_peak / 2**20, self.vram_peak / 2**20


def timed(rt, text, steps):
    t0 = time.perf_counter()
    try:
        r = rt.synthesize(_REQUEST(text=text, seed=SEED, no_ref=True, num_steps=steps))
        x = r.audio.detach().float().cpu().numpy().reshape(-1)
        sr = int(r.sample_rate)
        err = None
    except Exception as e:  # noqa: BLE001
        x, sr, err = np.zeros(0, np.float32), 0, f"{type(e).__name__}: {e}"[:300]
    return x, sr, time.perf_counter() - t0, err


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--engine", required=True, help="name used for the output directory and the tables")
    ap.add_argument("--model", required=True, choices=["SUN", "LUNA"])
    ap.add_argument("--device", default="cpu", choices=["cpu", "cuda"])
    ap.add_argument("--model-dtype", default="fp32", choices=["fp32", "bf16"])
    ap.add_argument("--codec-dtype", default="fp32", choices=["fp32", "bf16"])
    ap.add_argument("--steps", type=int, default=None, help="default: 24 for SUN, unset (checkpoint default) for LUNA")
    ap.add_argument("--tag", default="run1")
    ap.add_argument("--limit", type=int, default=0, help="smoke test: only the first N replies/handling items")
    ap.add_argument("--reply-limit", type=int, default=0, help="use only the first N replies (slow CPU rows)")
    ap.add_argument("--handling-limit", type=int, default=0, help="use only the first N handling items")
    ap.add_argument("--warmups", type=int, default=3, help="untimed warm-up calls after the first call")
    ap.add_argument("--no-pass2", action="store_true", help="skip the second in-process pass (doubles the cost)")
    ap.add_argument("--no-fuse", action="store_true", help="measure the official code path without speed_patch")
    ap.add_argument("--code-root", default=os.environ.get("IRODORI_CODE_ROOT"),
                    help="directory containing the patched irodori_tts package (python -I ignores PYTHONPATH)")
    a = ap.parse_args()

    if a.code_root:
        sys.path.insert(0, os.path.abspath(a.code_root))
    from irodori_tts.inference_runtime import InferenceRuntime, RuntimeKey, SamplingRequest  # noqa: PLC0415

    global _REQUEST
    _REQUEST = SamplingRequest

    repo = os.path.join(WORK, "models", f"Irodori-TTS-v4.1-Small-{a.model}")
    sys.path.insert(0, repo)  # speed_patch.py of this checkpoint
    import speed_patch  # noqa: PLC0415 - resolved from the checkpoint directory
    torch.set_num_threads(THREADS)
    steps = a.steps if a.steps is not None else (24 if a.model == "SUN" else None)

    peak = Peak(os.getpid())
    t0 = time.perf_counter()
    rt = InferenceRuntime.from_key(RuntimeKey(
        checkpoint=os.path.join(repo, "model.safetensors"),
        model_device=a.device, codec_device=a.device,
        model_precision=a.model_dtype, codec_precision=a.codec_dtype,
    ))
    load_s = time.perf_counter() - t0
    if getattr(rt, "watermarker", None) is not None:  # no watermark model offline; the card's script does the same
        rt.watermarker.model = None
    if not a.no_fuse:
        speed_patch.apply(rt, snake=(a.model == "SUN"), fold_codec=True, rope=True, fuse=True,
                          model_dtype=a.model_dtype, codec_dtype=a.codec_dtype)
    after_load_rss = peak.now()

    out = run_dir(a.engine, a.tag)
    res = {"engine": a.engine, "tag": a.tag, "model": a.model, "device": a.device, "model_dtype": a.model_dtype,
           "codec_dtype": a.codec_dtype, "steps": steps, "speed_patch": not a.no_fuse, "threads": THREADS,
           "load_s": load_s, "after_load_rss_mb": after_load_rss / 2**20}
    x, sr, first_s, err = timed(rt, REPLIES[0], steps)
    res["first_call_s"], res["first_call_error"], res["sample_rate"] = first_s, err, sr
    for t in REPLIES[1 : 1 + a.warmups]:
        timed(rt, t, steps)

    n_rep = a.limit or a.reply_limit or len(REPLIES)
    n_hand = a.limit or a.handling_limit or len(HANDLING)
    replies, handling = REPLIES[:n_rep], HANDLING[:n_hand]
    items = []
    for i, text in enumerate(replies):
        x, sr, dt, err = timed(rt, text, steps)
        dur = len(x) / sr if sr else 0.0
        sf.write(os.path.join(out, "wav", f"r{i:02d}.wav"), x, sr or 24000)
        items.append({"id": f"r{i:02d}", "kind": "reply", "text": text, "chars": len(text), "wall_s": dt,
                      "ttfa_s": dt, "ttfa_is_streaming": False, "audio_s": dur, "sr": sr,
                      "sha": pcm16_sha256(x), "error": err})
    if a.no_pass2:
        for it in items:
            it["sha_pass2"] = None
    else:
        for it, text in zip(items, replies):  # second pass, same process: same text -> same audio?
            x, _, _, _ = timed(rt, text, steps)
            it["sha_pass2"] = pcm16_sha256(x)
    for j, (cat, text, reading) in enumerate(handling):
        x, sr, dt, err = timed(rt, text, steps)
        sf.write(os.path.join(out, "wav", f"h{j:02d}.wav"), x, sr or 24000)
        items.append({"id": f"h{j:02d}", "kind": "handling", "category": cat, "text": text,
                      "expected_reading": reading, "wall_s": dt, "audio_s": len(x) / sr if sr else 0.0,
                      "sr": sr, "sha": pcm16_sha256(x), "error": err})
    res["peak_rss_mb"], res["peak_vram_mb"] = peak.stop()
    res["items"] = items
    if a.device == "cuda":
        res["cuda_max_alloc_mb"] = torch.cuda.max_memory_allocated() / 2**20
        res["cuda_max_reserved_mb"] = torch.cuda.max_memory_reserved() / 2**20
    dump_json(os.path.join(out, "result.json"), res)
    ok = [i for i in items if i["kind"] == "reply" and not i["error"]]
    print(a.engine, a.tag, "load %.1fs first %.2fs peak_rss %.0f MB peak_vram %.0f MB sr %s replies_ok %d/%d mean_wall %.3fs mean_audio %.2fs" % (
        load_s, first_s, res["peak_rss_mb"], res["peak_vram_mb"], sr, len(ok), len(replies),
        np.mean([i["wall_s"] for i in ok]) if ok else float("nan"),
        np.mean([i["audio_s"] for i in ok]) if ok else float("nan")), flush=True)


if __name__ == "__main__":
    main()
