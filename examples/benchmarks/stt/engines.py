"""Engine adapters. Each engine exposes:

    load_s            seconds spent loading (for server engines: spawn to first answered request)
    final(x)          full-segment transcription, x = float32 mono 16 kHz
    partials(x, step) pseudo/real partials: list of (audio_end_s, text, cost_s); see bench.py for how it is read
    rss_peak_mb()     peak working set of the engine's process (this process, or the server child)
    close()

Streaming engines (vosk) feed 100 ms chunks. Offline engines (sherpa-onnx offline recognizers, whisper.cpp) emulate
partials by re-decoding the growing prefix every `step` seconds, which is what an integration would have to do.
"""
import glob, io, json, os, subprocess, time, urllib.request, uuid, wave

import numpy as np
import psutil

from common import DL, SR

CHUNK_S = 0.1


def _pcm16(x):
    return (np.clip(x, -1, 1) * 32767).astype(np.int16)


def _wav_bytes(x):
    b = io.BytesIO()
    with wave.open(b, "wb") as w:
        w.setnchannels(1), w.setsampwidth(2), w.setframerate(SR)
        w.writeframes(_pcm16(x).tobytes())
    return b.getvalue()


class Base:
    def rss_peak_mb(self):
        return psutil.Process().memory_info().peak_wset / 2**20

    def partials(self, x, step):
        out, n = [], len(x)
        k = int(step * SR)
        for end in list(range(k, n, k)) + [n]:
            t0 = time.perf_counter()
            text = self.final(x[:end])
            out.append((end / SR, text, time.perf_counter() - t0))
        return out

    def close(self):
        pass


class Sherpa(Base):
    def __init__(self, kind, threads):
        import sherpa_onnx as so

        t0 = time.perf_counter()
        p = os.path.join
        if kind in ("k2v2-int8", "k2v2-fp32"):
            suf = ".int8.onnx" if kind.endswith("int8") else ".onnx"
            d = p(DL, "k2v2")
            r = so.OfflineRecognizer.from_transducer(
                encoder=p(d, f"encoder-epoch-99-avg-1{suf}"), decoder=p(d, f"decoder-epoch-99-avg-1{suf}"),
                joiner=p(d, f"joiner-epoch-99-avg-1{suf}"), tokens=p(d, "tokens.txt"),
                num_threads=threads, sample_rate=SR, feature_dim=80, decoding_method="greedy_search", provider="cpu")
        elif kind == "k2-ja-en":
            d = glob.glob(p(DL, "sherpa", "zipformer-ja-en", "*"))[0]
            fs = os.listdir(d)
            pick = lambda pre: p(d, sorted(f for f in fs if f.startswith(pre) and f.endswith(".int8.onnx"))[0])  # noqa: E731
            r = so.OfflineRecognizer.from_transducer(
                encoder=pick("encoder"), decoder=pick("decoder"), joiner=pick("joiner"), tokens=p(d, "tokens.txt"),
                num_threads=threads, sample_rate=SR, feature_dim=80, decoding_method="greedy_search", provider="cpu")
        elif kind == "parakeet-ja":
            d = glob.glob(p(DL, "sherpa", "parakeet-ja", "*"))[0]
            # The sherpa-onnx export of parakeet-tdt_ctc-0.6b-ja ships the CTC head only (one model.int8.onnx).
            r = so.OfflineRecognizer.from_nemo_ctc(
                model=p(d, "model.int8.onnx"), tokens=p(d, "tokens.txt"), num_threads=threads, provider="cpu")
        elif kind == "sensevoice":
            d = glob.glob(p(DL, "sherpa", "sensevoice-2024", "*"))[0]
            r = so.OfflineRecognizer.from_sense_voice(
                model=p(d, "model.int8.onnx"), tokens=p(d, "tokens.txt"), num_threads=threads,
                language="ja", use_itn=False, provider="cpu")
        else:
            raise SystemExit(f"unknown sherpa engine {kind}")
        self.r = r
        self.load_s = time.perf_counter() - t0

    def final(self, x):
        s = self.r.create_stream()
        s.accept_waveform(SR, x)
        self.r.decode_stream(s)
        return s.result.text


class Vosk(Base):
    def __init__(self, kind, threads):
        import vosk

        vosk.SetLogLevel(-1)
        t0 = time.perf_counter()
        d = glob.glob(os.path.join(DL, "vosk", kind, "*"))[0]
        self.m = vosk.Model(d)
        self.vosk = vosk
        self.load_s = time.perf_counter() - t0

    def _rec(self):
        return self.vosk.KaldiRecognizer(self.m, SR)

    def final(self, x):
        rec = self._rec()
        pcm = _pcm16(x)
        k = int(CHUNK_S * SR)
        for i in range(0, len(pcm), k):
            rec.AcceptWaveform(pcm[i:i + k].tobytes())
        return json.loads(rec.FinalResult()).get("text", "").replace(" ", "")

    def tail_cost(self, x):
        """Streaming latency at speech end: time to process the last chunk and call FinalResult."""
        rec = self._rec()
        pcm = _pcm16(x)
        k = int(CHUNK_S * SR)
        starts = list(range(0, len(pcm), k))
        for i in starts[:-1]:
            rec.AcceptWaveform(pcm[i:i + k].tobytes())
        t0 = time.perf_counter()
        rec.AcceptWaveform(pcm[starts[-1]:starts[-1] + k].tobytes())
        text = json.loads(rec.FinalResult()).get("text", "").replace(" ", "")
        return text, time.perf_counter() - t0

    def partials(self, x, step):
        rec = self._rec()
        pcm = _pcm16(x)
        k = int(CHUNK_S * SR)
        out = []
        for i in range(0, len(pcm), k):
            t0 = time.perf_counter()
            rec.AcceptWaveform(pcm[i:i + k].tobytes())
            part = json.loads(rec.PartialResult()).get("partial", "").replace(" ", "")
            out.append((min(i + k, len(pcm)) / SR, part, time.perf_counter() - t0))
        return out


class WhisperServer(Base):
    """whisper.cpp `whisper-server` as a child process on loopback; model loaded once."""

    def __init__(self, model, threads, port=8178, extra=(), gpu=False):
        exe = glob.glob(os.path.join(DL, "whispercpp", "bin-cuda" if gpu else "bin", "**", "whisper-server.exe"), recursive=True)[0]
        mp = os.path.join(DL, "whispercpp", f"ggml-{model}.bin")
        self.port = port
        t0 = time.perf_counter()
        self.p = subprocess.Popen([exe, "-m", mp, "-l", "ja", "-t", str(threads), "--host", "127.0.0.1", "--port", str(port),
                                   "-nt", *([] if gpu else ["-ng"]), *extra], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        for _ in range(600):
            try:
                urllib.request.urlopen(f"http://127.0.0.1:{port}/", timeout=1)
                break
            except urllib.error.HTTPError:
                break
            except Exception:  # noqa: BLE001
                if self.p.poll() is not None:
                    raise SystemExit("whisper-server exited early")
                time.sleep(0.1)
        self.load_s = time.perf_counter() - t0
        self.proc = psutil.Process(self.p.pid)
        self._peak = 0

    def final(self, x):
        bd = uuid.uuid4().hex
        wav = _wav_bytes(x)
        parts = [
            (f'--{bd}\r\nContent-Disposition: form-data; name="file"; filename="a.wav"\r\nContent-Type: audio/wav\r\n\r\n').encode() + wav + b"\r\n",
            (f'--{bd}\r\nContent-Disposition: form-data; name="temperature"\r\n\r\n0.0\r\n').encode(),
            (f'--{bd}\r\nContent-Disposition: form-data; name="response_format"\r\n\r\njson\r\n').encode(),
            f"--{bd}--\r\n".encode(),
        ]
        req = urllib.request.Request(f"http://127.0.0.1:{self.port}/inference", data=b"".join(parts),
                                     headers={"Content-Type": f"multipart/form-data; boundary={bd}"})
        text = json.load(urllib.request.urlopen(req, timeout=300))["text"]
        try:
            self._peak = max(self._peak, self.proc.memory_info().peak_wset)
        except psutil.Error:
            pass
        return text.strip()

    def rss_peak_mb(self):
        return self._peak / 2**20

    def close(self):
        self.p.terminate()
        try:
            self.p.wait(10)
        except subprocess.TimeoutExpired:
            self.p.kill()


def make(spec, threads, **kw):
    fam, _, name = spec.partition(":")
    if fam == "sherpa":
        return Sherpa(name, threads)
    if fam == "vosk":
        return Vosk(name, threads)
    if fam == "whisper":
        return WhisperServer(name, threads, **kw)
    if fam == "whispergpu":  # CUDA build of whisper-server; reported separately from the CPU tables
        return WhisperServer(name, threads, gpu=True, **kw)
    raise SystemExit(f"unknown engine {spec}")
