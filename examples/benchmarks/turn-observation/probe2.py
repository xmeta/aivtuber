import sys, os; sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import json, importlib.util, numpy as np, onnxruntime as ort, soundfile as sf
from lib import *
spec = importlib.util.spec_from_file_location("wf", os.path.join(TB, "dl", "ref", "_whisper_features.py"))
wf = importlib.util.module_from_spec(spec); spec.loader.exec_module(wf)
st = ort.InferenceSession(os.path.join(TB, "dl", "models", "smartturn", "smart-turn-v3.2-cpu.onnx"), providers=["CPUExecutionProvider"])
def prob(a):
    a = a[-8 * SR:]
    a = np.pad(a, (8 * SR - len(a), 0)) if len(a) < 8 * SR else a
    f = wf.compute_whisper_log_mel_features(a, do_normalize=True)
    return float(st.run(None, {"input_features": f[None].astype(np.float32)})[0][0].item())
lab = json.load(open(os.path.join(TB, "audio", "labels.json"), encoding="utf-8"))["streams"]
rng = np.random.default_rng(1)
rows = []
for s in lab:
    if s["cls"] != "A":
        continue
    x, _ = sf.read(os.path.join(TB, "audio", "streams", s["id"] + ".wav"), dtype="float32")
    e = s["final_end"]
    st0 = s["speech"][0][0]
    full = prob(x[: int((e + 0.4) * SR)])
    f = rng.uniform(0.35, 0.75)
    c = st0 + f * (e - st0)
    tr = prob(np.concatenate([x[: int(c * SR)], x[int((e + 0.5) * SR): int((e + 0.9) * SR)]]))
    rows.append((full, tr))
    if len(rows) >= 40:
        break
r = np.array(rows)
print("complete-audio mean p", r[:, 0].mean(), "min", r[:, 0].min(), "| mid-utterance cut + 400ms silence mean p", r[:, 1].mean(), "frac>0.5", (r[:, 1] > 0.5).mean())
