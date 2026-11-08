import sys, os; sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import json, difflib, numpy as np, sherpa_onnx
from lib import *

M = os.path.join(TB, "dl", "models", "reazon-k2-v2")
rec = sherpa_onnx.OfflineRecognizer.from_transducer(
    encoder=os.path.join(M, "encoder-epoch-99-avg-1.int8.onnx"),
    decoder=os.path.join(M, "decoder-epoch-99-avg-1.int8.onnx"),
    joiner=os.path.join(M, "joiner-epoch-99-avg-1.int8.onnx"),
    tokens=os.path.join(M, "tokens.txt"), num_threads=4, sample_rate=16000, decoding_method="greedy_search")
info = json.load(open(os.path.join(TB, "audio", "clips.json"), encoding="utf-8"))
out = {}
names = list(info)
for n, k in enumerate(names):
    x = load16k(os.path.join(TB, "audio", "clips", k + ".wav"))
    s = rec.create_stream()
    s.accept_waveform(16000, x)
    rec.decode_stream(s)
    r = s.result
    sim = difflib.SequenceMatcher(None, r.text, info[k]["sentence"]).ratio()
    out[k] = dict(text=r.text, tokens=list(r.tokens), ts=list(r.timestamps), sim=sim)
    if n % 50 == 0:
        print(n, len(names), r.text, "|", info[k]["sentence"], sim, flush=True)
json.dump(out, open(os.path.join(TB, "audio", "asr_all.json"), "w"), ensure_ascii=False)
