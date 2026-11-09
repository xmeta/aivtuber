"""Shared helpers: work-dir layout, text normalisation, CER. No machine paths: dirs come from the environment.

TTS_BENCH_DIR : the (new, empty) directory holding downloads in dl/   (fetch_assets.py)
TTS_OUT_DIR   : per-run outputs (wavs, json). Defaults to $TTS_BENCH_DIR/out. Never inside the repository.
"""
import hashlib, json, os, re, unicodedata

WORK = os.environ["TTS_BENCH_DIR"]
DL = os.path.join(WORK, "dl")
OUT = os.environ.get("TTS_OUT_DIR", os.path.join(WORK, "out"))

_KANJI_NUM = "〇一二三四五六七八九十百千万億兆"
_NUM_RUN = re.compile("[0-9" + _KANJI_NUM + "]+")


def normalize(text: str) -> str:
    """Same normalisation as EXP-0002, applied identically to reference and hypothesis.

    NFKC, lower-case, then drop every character whose Unicode category is punctuation (P*), symbol (S*),
    separator (Z*) or control/format (C*). Nothing else is rewritten.
    """
    t = unicodedata.normalize("NFKC", text).lower()
    return "".join(ch for ch in t if unicodedata.category(ch)[0] not in "PSZC")


def number_insensitive(norm_text: str) -> str:
    return _NUM_RUN.sub("#", norm_text)


def pcm16_sha256(samples) -> str:
    """sha256 of the audio as 16-bit little-endian PCM (what a WAV writer would store)."""
    import numpy as np

    x = np.clip(np.asarray(samples, dtype=np.float32), -1.0, 1.0)
    return hashlib.sha256((x * 32767.0).round().astype("<i2").tobytes()).hexdigest()


def run_dir(engine: str, tag: str) -> str:
    d = os.path.join(OUT, engine.replace(":", "_"), tag)
    os.makedirs(os.path.join(d, "wav"), exist_ok=True)
    return d


def load_json(path):
    with open(path, encoding="utf8") as f:
        return json.load(f)


def dump_json(path, obj):
    with open(path, "w", encoding="utf8") as f:
        json.dump(obj, f, ensure_ascii=False, indent=1)
