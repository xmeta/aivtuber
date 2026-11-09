"""Round-trip intelligibility proxy, step 1 (sherpa-onnx venv): transcribe every wav of a run with the repository's chosen STT.

Run:  python -I -X utf8 eval_stt.py <engine> <tag>
Reads  $TTS_OUT_DIR/<engine>/<tag>/{result.json,wav/*.wav}; writes stt.json next to them: {id: hypothesis text}.
STT = sherpa-onnx ReazonSpeech k2-v2 int8 (the engine merged in #272), greedy decoding, 4 threads, whole utterance (offline).
The TTS audio is resampled to 16 kHz mono and padded with 0.3 s of silence before and 0.5 s after (EXP-0002 segment shape).
This measures what the STT can recover, not naturalness; STT errors count against the TTS (see EXP-0003 limitations).
"""
import os, sys

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

import numpy as np
import sherpa_onnx as so
import soundfile as sf
from scipy.signal import resample_poly

from common import DL, dump_json, load_json, run_dir


def main(engine, tag):
    d = os.path.join(DL, "k2v2")
    p = os.path.join
    rec = so.OfflineRecognizer.from_transducer(
        encoder=p(d, "encoder-epoch-99-avg-1.int8.onnx"), decoder=p(d, "decoder-epoch-99-avg-1.int8.onnx"),
        joiner=p(d, "joiner-epoch-99-avg-1.int8.onnx"), tokens=p(d, "tokens.txt"),
        num_threads=4, sample_rate=16000, feature_dim=80, decoding_method="greedy_search", provider="cpu")
    out = run_dir(engine, tag)
    res = load_json(os.path.join(out, "result.json"))
    hyp = {}
    for it in res["items"]:
        x, sr = sf.read(os.path.join(out, "wav", it["id"] + ".wav"), dtype="float32")
        if x.ndim > 1:
            x = x.mean(1)
        if len(x) < 100:
            hyp[it["id"]] = ""
            continue
        if sr != 16000:
            g = np.gcd(sr, 16000)
            x = resample_poly(x, 16000 // g, sr // g).astype(np.float32)
        x = np.concatenate([np.zeros(4800, np.float32), x, np.zeros(8000, np.float32)])
        s = rec.create_stream()
        s.accept_waveform(16000, x)
        rec.decode_stream(s)
        hyp[it["id"]] = s.result.text
    dump_json(os.path.join(out, "stt.json"), hyp)
    print(engine, tag, "transcribed", len(hyp))


if __name__ == "__main__":
    main(sys.argv[1], sys.argv[2])
