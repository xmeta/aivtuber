"""TTS engine wrappers with one interface:  load() -> None ; synth(text) -> (float32 samples, sample_rate, ttfa_s | None) ; close().

ttfa_s is the time to the first audio chunk when the engine delivers one before the end (streaming callback), else None
(the caller then uses the whole-call time as time to first audio).
CPU only. Engine names (see bench.py --engine):
  sherpa-supertonic3[:sidN]       sherpa-onnx Python wheel, Supertonic 3 int8, language "ja" passed explicitly
  sherpa-supertonic3-defaultlang  same, but language left at the sherpa-onnx default (documents the default)
  sherpa-kokoro-int8 / sherpa-kokoro-fp32   sherpa-onnx kokoro-multi-lang-v1_0 with Japanese text (documents the failure mode)
  kokoro-onnx-int8 / kokoro-onnx-fp32       kokoro-onnx 0.6.1 + misaki.ja (pyopenjtalk) phonemes, voice jf_alpha
  voicevox[:speakerid]            VOICEVOX engine 0.26.1 started as a separate HTTP process on loopback (default speaker 3)
"""
import glob, io, json, os, subprocess, sys, threading, time, urllib.request

import numpy as np

from common import DL

THREADS = 4


def _sherpa_dir(sub):
    return glob.glob(os.path.join(DL, "sherpa", sub, "*"))[0]


class SherpaSupertonic:
    def __init__(self, sid=0, lang="ja", seed=1234):
        self.sid, self.lang, self.seed = sid, lang, seed

    def load(self):
        import sherpa_onnx as s

        d = _sherpa_dir("supertonic3-int8")
        p = lambda n: os.path.join(d, n)
        cfg = s.OfflineTtsConfig(model=s.OfflineTtsModelConfig(
            supertonic=s.OfflineTtsSupertonicModelConfig(
                duration_predictor=p("duration_predictor.int8.onnx"), text_encoder=p("text_encoder.int8.onnx"),
                vector_estimator=p("vector_estimator.int8.onnx"), vocoder=p("vocoder.int8.onnx"),
                tts_json=p("tts.json"), unicode_indexer=p("unicode_indexer.bin"), voice_style=p("voice.bin")),
            num_threads=THREADS, debug=False, provider="cpu"))
        assert cfg.validate()
        self.s = s
        self.tts = s.OfflineTts(cfg)

    def synth(self, text):
        g = self.s.GenerationConfig()
        g.sid = self.sid
        extra = {}
        if self.lang:
            extra["lang"] = self.lang
        if self.seed is not None:
            extra["seed"] = str(self.seed)
        g.extra = extra
        first = []
        t0 = time.perf_counter()

        def cb(samples, progress):
            if not first:
                first.append(time.perf_counter() - t0)
            return 1

        a = self.tts.generate(text, g, cb)
        return np.asarray(a.samples, dtype=np.float32), a.sample_rate, (first[0] if first else None)

    def close(self):
        pass


class SherpaKokoro:
    def __init__(self, int8, sid=0):
        self.int8, self.sid = int8, sid

    def load(self):
        import sherpa_onnx as s

        d = _sherpa_dir("kokoro-int8-multi-lang-v1_0" if self.int8 else "kokoro-multi-lang-v1_0")
        p = lambda n: os.path.join(d, n)
        k = s.OfflineTtsKokoroModelConfig(
            model=p("model.int8.onnx" if self.int8 else "model.onnx"), voices=p("voices.bin"), tokens=p("tokens.txt"),
            lexicon=",".join([p("lexicon-us-en.txt"), p("lexicon-zh.txt")]), data_dir=p("espeak-ng-data"), dict_dir=p("dict"), lang="ja")
        cfg = s.OfflineTtsConfig(model=s.OfflineTtsModelConfig(kokoro=k, num_threads=THREADS, debug=False, provider="cpu"),
                                 rule_fsts=",".join([p("date-zh.fst"), p("number-zh.fst"), p("phone-zh.fst")]))
        assert cfg.validate()
        self.tts = s.OfflineTts(cfg)

    def synth(self, text):
        a = self.tts.generate(text, sid=self.sid, speed=1.0)
        return np.asarray(a.samples, dtype=np.float32), a.sample_rate, None

    def close(self):
        pass


class KokoroOnnx:
    """kokoro-onnx 0.6.1 for the model, misaki.ja (pyopenjtalk-plus + fugashi/unidic-lite) for Japanese phonemes."""

    def __init__(self, int8, voice="jf_alpha"):
        self.int8, self.voice = int8, voice

    def load(self):
        here = os.path.dirname(os.path.abspath(__file__))
        sys.path.insert(0, os.path.join(here, "shims"))  # mojimoji stand-in, see shims/mojimoji.py
        import onnxruntime as ort
        from kokoro_onnx import Kokoro
        from misaki import ja

        so = ort.SessionOptions()
        so.intra_op_num_threads = THREADS
        so.inter_op_num_threads = 1
        d = os.path.join(DL, "kokoro-onnx")
        sess = ort.InferenceSession(os.path.join(d, "kokoro-v1.0.int8.onnx" if self.int8 else "kokoro-v1.0.onnx"), sess_options=so,
                                    providers=["CPUExecutionProvider"])
        self.k = Kokoro.from_session(sess, os.path.join(d, "voices-v1.0.bin"))
        self.g2p = ja.JAG2P()

    def synth(self, text):
        ph = self.g2p(text)  # misaki 0.7.4 ja.JAG2P returns the phoneme string
        if isinstance(ph, tuple):
            ph = ph[0]
        samples, sr = self.k.create(ph, voice=self.voice, speed=1.0, is_phonemes=True)
        return np.asarray(samples, dtype=np.float32), sr, None

    def close(self):
        pass


class Voicevox:
    """The engine runs as a separate process; we only speak HTTP to 127.0.0.1 (nothing is linked)."""

    PORT = 50021

    def __init__(self, speaker=3):
        self.speaker = speaker

    def load(self):
        exe = glob.glob(os.path.join(DL, "voicevox", "engine", "windows-cpu", "run.exe"))[0]
        self.proc = subprocess.Popen([exe, "--host", "127.0.0.1", "--port", str(self.PORT), "--cpu_num_threads", str(THREADS)],
                                     stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, cwd=os.path.dirname(exe))
        self.pid = self.proc.pid
        for _ in range(600):
            try:
                urllib.request.urlopen(f"http://127.0.0.1:{self.PORT}/version", timeout=1).read()
                return
            except Exception:  # noqa: BLE001
                time.sleep(0.5)
        raise RuntimeError("voicevox engine did not start")

    def _post(self, path, data=None, ctype=None):
        req = urllib.request.Request(f"http://127.0.0.1:{self.PORT}{path}", data=data if data is not None else b"", method="POST",
                                     headers={"Content-Type": ctype} if ctype else {})
        return urllib.request.urlopen(req, timeout=120).read()

    def synth(self, text):
        import soundfile as sf
        import urllib.parse

        q = self._post(f"/audio_query?speaker={self.speaker}&text={urllib.parse.quote(text)}")
        wav = self._post(f"/synthesis?speaker={self.speaker}", q, "application/json")
        x, sr = sf.read(io.BytesIO(wav), dtype="float32")
        return x, sr, None

    def tree_pids(self):
        return [self.pid]

    def close(self):
        self.proc.terminate()


def make(name):
    base, _, arg = name.partition(":")
    if base == "sherpa-supertonic3":
        return SherpaSupertonic(sid=int(arg) if arg else 0)
    if base == "sherpa-supertonic3-defaultlang":
        return SherpaSupertonic(sid=0, lang=None, seed=1234)
    if base == "sherpa-supertonic3-noseed":
        return SherpaSupertonic(sid=0, seed=None)
    if base == "sherpa-kokoro-int8":
        return SherpaKokoro(True)
    if base == "sherpa-kokoro-fp32":
        return SherpaKokoro(False)
    if base == "kokoro-onnx-int8":
        return KokoroOnnx(True)
    if base == "kokoro-onnx-fp32":
        return KokoroOnnx(False)
    if base == "voicevox":
        return Voicevox(int(arg) if arg else 3)
    raise SystemExit(f"unknown engine {name}")
