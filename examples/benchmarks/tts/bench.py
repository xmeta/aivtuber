"""One engine, one fresh process: load, first call, warm-up, 50 replies twice (same process), handling set once.

Run:  python -I -X utf8 bench.py --engine <name> [--tag run1]
Writes $TTS_OUT_DIR/<engine>/<tag>/{result.json, wav/*.wav}. Times are wall-clock on one machine.
"""
import argparse, os, sys, threading, time

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)  # -I removes the script dir from sys.path

import numpy as np
import psutil
import soundfile as sf

import engines
from common import dump_json, pcm16_sha256, run_dir
from corpus import HANDLING, REPLIES


class RssPeak:
    """Peak resident set summed over the engine process (and its children, for a sidecar), sampled every 25 ms."""

    def __init__(self, pid):
        self.root, self.peak, self._stop = psutil.Process(pid), 0, False
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
            self.peak = max(self.peak, self.now())
            time.sleep(0.025)

    def stop(self):
        self._stop = True
        self.t.join()
        return self.peak


def timed(eng, text):
    t0 = time.perf_counter()
    try:
        x, sr, ttfa = eng.synth(text)
        err = None
    except Exception as e:  # noqa: BLE001
        x, sr, ttfa, err = np.zeros(0, np.float32), 0, None, f"{type(e).__name__}: {e}"[:300]
    dt = time.perf_counter() - t0
    return x, sr, ttfa, dt, err


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--engine", required=True)
    ap.add_argument("--tag", default="run1")
    a = ap.parse_args()
    eng = engines.make(a.engine)
    out = run_dir(a.engine, a.tag)
    rss = RssPeak(os.getpid())  # children included, so a sidecar engine process is counted
    base_rss = rss.now()
    t0 = time.perf_counter()
    eng.load()
    load_s = time.perf_counter() - t0
    after_load_rss = rss.now()
    res = {"engine": a.engine, "tag": a.tag, "load_s": load_s, "base_rss_mb": base_rss / 2**20, "after_load_rss_mb": after_load_rss / 2**20,
           "threads": engines.THREADS}
    # first call (lazy init cost), then three warm-ups, none of them in the tables
    _, _, _, first_s, err = timed(eng, REPLIES[0])
    res["first_call_s"], res["first_call_error"] = first_s, err
    for t in REPLIES[1:4]:
        timed(eng, t)

    items = []
    for i, text in enumerate(REPLIES):
        x, sr, ttfa, dt, err = timed(eng, text)
        dur = len(x) / sr if sr else 0.0
        sf.write(os.path.join(out, "wav", f"r{i:02d}.wav"), x, sr or 24000)
        items.append({"id": f"r{i:02d}", "kind": "reply", "text": text, "chars": len(text), "wall_s": dt, "ttfa_s": ttfa if ttfa is not None else dt,
                      "ttfa_is_streaming": ttfa is not None, "audio_s": dur, "sr": sr, "sha": pcm16_sha256(x), "error": err})
    for it, text in zip(items, REPLIES):  # second pass, same process: same text -> same audio?
        x, sr, _, _, _ = timed(eng, text)
        it["sha_pass2"] = pcm16_sha256(x)
    for j, (cat, text, reading) in enumerate(HANDLING):
        x, sr, ttfa, dt, err = timed(eng, text)
        sf.write(os.path.join(out, "wav", f"h{j:02d}.wav"), x, sr or 24000)
        items.append({"id": f"h{j:02d}", "kind": "handling", "category": cat, "text": text, "expected_reading": reading, "wall_s": dt,
                      "audio_s": len(x) / sr if sr else 0.0, "sr": sr, "sha": pcm16_sha256(x), "error": err})
    res["peak_rss_mb"] = rss.stop() / 2**20
    res["items"] = items
    dump_json(os.path.join(out, "result.json"), res)
    eng.close()
    ok = [i for i in items if i["kind"] == "reply" and not i["error"]]
    print(a.engine, a.tag, "load %.1fs first %.2fs peak_rss %.0f MB replies_ok %d/50 mean_wall %.3fs mean_audio %.2fs" % (
        load_s, first_s, res["peak_rss_mb"], len(ok), np.mean([i["wall_s"] for i in ok]) if ok else float("nan"),
        np.mean([i["audio_s"] for i in ok]) if ok else float("nan")), flush=True)


if __name__ == "__main__":
    main()
