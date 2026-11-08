import sys, os; sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import json, pickle, importlib.util, time, numpy as np, onnxruntime as ort, soundfile as sf, sherpa_onnx
from lib import *
from vadlib import *

cond = sys.argv[1]  # clean | noisy15
lab = json.load(open(os.path.join(TB, "audio", "labels.json"), encoding="utf-8"))
streams = lab["streams"]
if len(sys.argv) > 2:
    streams = streams[:: int(sys.argv[2])]

# smart-turn (pipecat reference preprocessing, pinned by sha256 in the record)
spec = importlib.util.spec_from_file_location("wf", os.path.join(TB, "dl", "ref", "_whisper_features.py"))
wf = importlib.util.module_from_spec(spec); spec.loader.exec_module(wf)
so = ort.SessionOptions(); so.execution_mode = ort.ExecutionMode.ORT_SEQUENTIAL
so.inter_op_num_threads = 1; so.intra_op_num_threads = 1
so.graph_optimization_level = ort.GraphOptimizationLevel.ORT_ENABLE_ALL
st = ort.InferenceSession(os.path.join(TB, "dl", "models", "smartturn", "smart-turn-v3.2-cpu.onnx"), so, providers=["CPUExecutionProvider"])


def st_prob(a):
    a = a[-8 * SR:]
    if len(a) < 8 * SR:
        a = np.pad(a, (8 * SR - len(a), 0))
    f = wf.compute_whisper_log_mel_features(a, do_normalize=True)
    return float(st.run(None, {"input_features": f[None].astype(np.float32)})[0][0].item())


def sherpa_vad(model, T):
    c = sherpa_onnx.VadModelConfig()
    c.silero_vad.model = model
    c.silero_vad.threshold = 0.5
    c.silero_vad.min_silence_duration = T
    c.silero_vad.min_speech_duration = 0.25
    c.silero_vad.window_size = 512
    c.silero_vad.max_speech_duration = 120
    c.sample_rate = 16000
    c.num_threads = 1
    return sherpa_onnx.VoiceActivityDetector(c, buffer_size_in_seconds=60)


def sherpa_events(model, T, x):
    v = sherpa_vad(model, T)
    ev = []
    n = len(x) // FR
    for i in range(n):
        v.accept_waveform(x[i * FR:(i + 1) * FR])
        while not v.empty():
            ev.append((i + 1) * FR / SR)
            v.pop()
    return ev


V6 = SileroOrt(SILERO_V6)
res = {}
rng = np.random.default_rng(7)
t_start = time.time()
for si, s in enumerate(streams):
    x, _ = sf.read(os.path.join(TB, "audio", "streams", s["id"] + ".wav"), dtype="float32")
    if cond == "noisy15":
        nz = pink(len(x), rng) * 0.07 * 10 ** (-15 / 20)
        x = (x + nz).astype(np.float32)
    p = run_probs(V6, x)
    r = dict(p=p, sherpa={}, st={})
    for mname, model in [("v4", SILERO_V4_SHERPA), ("v6", SILERO_V6)]:
        for T in (0.3, 0.5, 0.8):
            try:
                r["sherpa"][(mname, T)] = sherpa_events(model, T, x)
            except Exception as e:
                r["sherpa"][(mname, T)] = None
                if si == 0:
                    print("sherpa", mname, T, "failed", e)
    segs = segments(p)
    first = segs[0][0] * FR / SR if segs else 0
    ctx0 = max(0, int((first - 0.5) * SR))
    for gi, (a, b) in enumerate(segs):
        te = b * FR / SR
        nxt = segs[gi + 1][0] * FR / SR if gi + 1 < len(segs) else 1e9
        for k in range(200, 1501, 100):
            tt = te + k / 1000
            if tt >= nxt or tt * SR > len(x):
                break
            t0 = time.perf_counter_ns()
            pr = st_prob(x[ctx0:int(tt * SR)])
            r["st"][(gi, k)] = (pr, tt)
    res[s["id"]] = r
    if si % 25 == 0:
        print(cond, si, len(streams), f"{time.time()-t_start:.0f}s", flush=True)
pickle.dump(res, open(os.path.join(TB, "out", f"eval_{cond}.pkl"), "wb"))
print("done", cond)
