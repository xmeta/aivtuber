import os, time, numpy as np, onnxruntime as ort
from lib import *

SILERO_V6 = os.path.join(TB, "dl", "models", "vad", "silero_vad_master.onnx")
SILERO_V4_SHERPA = os.path.join(TB, "dl", "models", "vad", "sherpa_silero_vad.onnx")


class SileroOrt:
    """Silero VAD (snakers4 master export, 16 kHz) streamed 512 samples + 64 context at a time."""

    def __init__(self, path=SILERO_V6, threads=1):
        so = ort.SessionOptions()
        so.inter_op_num_threads = 1
        so.intra_op_num_threads = threads
        self.s = ort.InferenceSession(path, so, providers=["CPUExecutionProvider"])
        self.reset()

    def reset(self):
        self.state = np.zeros((2, 1, 128), np.float32)
        self.ctx = np.zeros((1, 64), np.float32)
        self.sr = np.array(16000, np.int64)

    def step(self, frame):
        x = np.concatenate([self.ctx, frame[None, :]], 1)
        out, self.state = self.s.run(None, {"input": x, "state": self.state, "sr": self.sr})
        self.ctx = x[:, -64:]
        return float(out[0, 0])


def run_probs(vad, x, timing=None):
    vad.reset()
    n = len(x) // FR
    p = np.zeros(n, np.float32)
    for i in range(n):
        t0 = time.perf_counter_ns()
        p[i] = vad.step(x[i * FR:(i + 1) * FR])
        if timing is not None:
            timing.append(time.perf_counter_ns() - t0)
    return p


def segments(p, thr=0.5, neg=None, min_speech_ms=250):
    """Hysteresis speech activity from per-frame probs. Returns list of (start_frame, end_frame_exclusive)
    where end = first frame of the silence. A segment is only reported once it lasted >= min_speech_ms."""
    neg = thr - 0.15 if neg is None else neg
    act = False
    segs = []
    st = 0
    for i, v in enumerate(p):
        if not act and v >= thr:
            act = True
            st = i
        elif act and v < neg:
            act = False
            segs.append((st, i))
    if act:
        segs.append((st, len(p)))
    fm = min_speech_ms / (FR * 1000 / SR)
    return [s for s in segs if s[1] - s[0] >= fm]
