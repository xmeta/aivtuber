import sys, os; sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import io, json, importlib.util, numpy as np, onnxruntime as ort, soundfile as sf, pyarrow.parquet as pq
from lib import *
spec = importlib.util.spec_from_file_location("wf", os.path.join(TB, "dl", "ref", "_whisper_features.py"))
wf = importlib.util.module_from_spec(spec); spec.loader.exec_module(wf)
so = ort.SessionOptions(); so.intra_op_num_threads = 1; so.inter_op_num_threads = 1
st = ort.InferenceSession(os.path.join(TB, "dl", "models", "smartturn", "smart-turn-v3.2-cpu.onnx"), so, providers=["CPUExecutionProvider"])
def prob(a):
    a = a[-8 * SR:]
    a = np.pad(a, (8 * SR - len(a), 0)) if len(a) < 8 * SR else a
    f = wf.compute_whisper_log_mel_features(a, do_normalize=True)
    return float(st.run(None, {"input_features": f[None].astype(np.float32)})[0][0].item())
pf = pq.ParquetFile(os.path.join(TB, "dl", "st_test", "shard0.parquet"))
rows = []
for b in pf.iter_batches(batch_size=256, columns=["audio", "language", "endpoint_bool", "midfiller", "endfiller", "synthetic", "dataset"]):
    d = b.to_pydict()
    for i in range(len(d["language"])):
        if d["language"][i] != "jpn":
            continue
        x, sr = sf.read(io.BytesIO(d["audio"][i]["bytes"]), dtype="float32")
        if x.ndim > 1: x = x.mean(1)
        if sr != SR:
            from scipy.signal import resample_poly
            from math import gcd
            g = gcd(sr, SR); x = resample_poly(x, SR // g, sr // g).astype(np.float32)
        rows.append((d["endpoint_bool"][i], d["synthetic"][i], d["dataset"][i], d["midfiller"][i], d["endfiller"][i], prob(x), len(x) / SR))
print("languages sample codes ok; n_ja =", len(rows))
import collections
print(collections.Counter(r[2] for r in rows))
y = np.array([r[0] for r in rows]); p = np.array([r[5] for r in rows])
for thr in (0.3, 0.5, 0.7):
    pred = p > thr
    print("thr", thr, "acc", (pred == y).mean().round(4), "FPR(incomplete->complete)", pred[~y].mean().round(4), "FNR", (~pred[y]).mean().round(4))
print("endpoint pos/neg", y.sum(), (~y).sum(), "synthetic frac", np.mean([r[1] for r in rows]))
