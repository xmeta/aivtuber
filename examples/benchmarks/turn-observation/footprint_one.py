import sys, os; sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import time, json, psutil
t_proc = time.perf_counter()
import numpy as np, onnxruntime as ort, sherpa_onnx
from lib import *
from vadlib import *
p = psutil.Process()
name = sys.argv[1]
base = p.memory_info().rss
t0 = time.perf_counter()
if name == "silero_v6_ort":
    m = SileroOrt(SILERO_V6)
    m.step(np.zeros(512, np.float32))
elif name.startswith("sherpa_vad"):
    mm = SILERO_V4_SHERPA if name.endswith("v4") else SILERO_V6
    c = sherpa_onnx.VadModelConfig()
    c.silero_vad.model = mm; c.silero_vad.threshold = 0.5; c.silero_vad.min_silence_duration = 0.5
    c.silero_vad.min_speech_duration = 0.25; c.silero_vad.window_size = 512; c.sample_rate = 16000; c.num_threads = 1
    m = sherpa_onnx.VoiceActivityDetector(c, buffer_size_in_seconds=60)
    m.accept_waveform(np.zeros(512, np.float32))
elif name == "smart_turn_v3.2_cpu":
    import importlib.util
    spec = importlib.util.spec_from_file_location("wf", os.path.join(TB, "dl", "ref", "_whisper_features.py"))
    wf = importlib.util.module_from_spec(spec); spec.loader.exec_module(wf)
    so = ort.SessionOptions(); so.intra_op_num_threads = 1; so.inter_op_num_threads = 1
    s = ort.InferenceSession(os.path.join(TB, "dl", "models", "smartturn", "smart-turn-v3.2-cpu.onnx"), so, providers=["CPUExecutionProvider"])
    s.run(None, {"input_features": wf.compute_whisper_log_mel_features(np.zeros(8 * SR, np.float32))[None].astype(np.float32)})
load = time.perf_counter() - t0
mi = p.memory_full_info()
print(json.dumps(dict(name=name, load_s=load, rss_base_mb=base / 2**20, rss_after_mb=mi.rss / 2**20,
                      peak_wset_mb=mi.peak_wset / 2**20, delta_mb=(mi.rss - base) / 2**20)))
