import sys, os; sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import json, time, importlib.util, numpy as np, onnxruntime as ort, soundfile as sf, sherpa_onnx
from lib import *
from vadlib import *

lab = json.load(open(os.path.join(TB, "audio", "labels.json"), encoding="utf-8"))["streams"]
ids = [s["id"] for s in lab if s["cls"] in ("A", "B1", "B2")][::2]  # 156 streams, speech+gaps, clean
xs = [sf.read(os.path.join(TB, "audio", "streams", i + ".wav"), dtype="float32")[0] for i in ids]
audio_s = sum(len(x) for x in xs) / SR
print("streams", len(xs), "audio s", round(audio_s, 1))
res = {}


def stats(a):
    a = np.array(a) / 1e6
    return dict(n=len(a), p50_ms=float(np.percentile(a, 50)), p95_ms=float(np.percentile(a, 95)),
                p99_ms=float(np.percentile(a, 99)), max_ms=float(a.max()), mean_ms=float(a.mean()))


# Silero via onnxruntime, 1 thread
for th in (1, 4):
    v = SileroOrt(SILERO_V6, threads=th)
    run_probs(v, xs[0])  # warm
    tm = []
    t0 = time.perf_counter()
    for x in xs:
        run_probs(v, x, tm)
    wall = time.perf_counter() - t0
    r = stats(tm)
    r["rtf_wall_incl_python_loop"] = wall / audio_s
    r["rtf_model_only"] = sum(tm) / 1e9 / audio_s
    res[f"silero_v6_ort_{th}thread"] = r
    print(f"silero ort {th}t", r, flush=True)


def shv(model):
    c = sherpa_onnx.VadModelConfig()
    c.silero_vad.model = model; c.silero_vad.threshold = 0.5
    c.silero_vad.min_silence_duration = 0.5; c.silero_vad.min_speech_duration = 0.25
    c.silero_vad.window_size = 512; c.silero_vad.max_speech_duration = 120
    c.sample_rate = 16000; c.num_threads = 1
    return sherpa_onnx.VoiceActivityDetector(c, buffer_size_in_seconds=60)


for nm, m in (("sherpa_vad_silero_v4", SILERO_V4_SHERPA), ("sherpa_vad_silero_v6", SILERO_V6)):
    tm = []
    t0 = time.perf_counter()
    for x in xs:
        vd = shv(m)
        for i in range(len(x) // FR):
            a = time.perf_counter_ns()
            vd.accept_waveform(x[i * FR:(i + 1) * FR])
            tm.append(time.perf_counter_ns() - a)
            while not vd.empty():
                vd.pop()
    wall = time.perf_counter() - t0
    r = stats(tm)
    r["rtf_wall_incl_python_loop"] = wall / audio_s
    r["rtf_model_only"] = sum(tm) / 1e9 / audio_s
    res[nm] = r
    print(nm, r, flush=True)

# smart-turn
spec = importlib.util.spec_from_file_location("wf", os.path.join(TB, "dl", "ref", "_whisper_features.py"))
wf = importlib.util.module_from_spec(spec); spec.loader.exec_module(wf)
rng = np.random.default_rng(3)
wins = []
for _ in range(120):
    x = xs[int(rng.integers(len(xs)))]
    n = int(rng.uniform(1.5, 8.0) * SR)
    n = min(n, len(x))
    wins.append(x[:n])
for th in (1, 4):
    so = ort.SessionOptions(); so.execution_mode = ort.ExecutionMode.ORT_SEQUENTIAL
    so.inter_op_num_threads = 1; so.intra_op_num_threads = th
    so.graph_optimization_level = ort.GraphOptimizationLevel.ORT_ENABLE_ALL
    st = ort.InferenceSession(os.path.join(TB, "dl", "models", "smartturn", "smart-turn-v3.2-cpu.onnx"), so, providers=["CPUExecutionProvider"])
    feat, inf = [], []
    for k, a in enumerate(wins + wins[:10]):
        a = a[-8 * SR:]
        a = np.pad(a, (8 * SR - len(a), 0)) if len(a) < 8 * SR else a
        t0 = time.perf_counter_ns()
        f = wf.compute_whisper_log_mel_features(a, do_normalize=True)[None].astype(np.float32)
        t1 = time.perf_counter_ns()
        st.run(None, {"input_features": f})
        t2 = time.perf_counter_ns()
        if k >= 10:
            feat.append(t1 - t0); inf.append(t2 - t1)
    tot = [a + b for a, b in zip(feat, inf)]
    res[f"smart_turn_v3.2_cpu_{th}thread"] = dict(features=stats(feat), onnx=stats(inf), total=stats(tot))
    print("smart-turn", th, res[f"smart_turn_v3.2_cpu_{th}thread"]["total"], flush=True)
json.dump(res, open(os.path.join(TB, "out", "timing.json"), "w"), indent=1)
