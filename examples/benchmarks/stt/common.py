"""Shared helpers: work-dir layout, text normalisation, CER. No machine paths; the work dir comes from $STT_BENCH_DIR."""
import json, os, re, unicodedata

import numpy as np
from rapidfuzz.distance import Levenshtein

WORK = os.environ["STT_BENCH_DIR"]
DL = os.path.join(WORK, "dl")
EVAL = os.path.join(WORK, "eval")
OUT = os.path.join(WORK, "out")
SR = 16000

_KANJI_NUM = "〇一二三四五六七八九十百千万億兆"
_NUM_RUN = re.compile("[0-9" + _KANJI_NUM + "]+")


def normalize(text: str) -> str:
    """Primary normalisation, applied identically to gold and hypothesis.

    NFKC (full/half width, compatibility forms), lower-case, then drop every character whose Unicode
    category is punctuation (P*), symbol (S*), separator (Z*) or control/format (C*). Kana, kanji,
    digits and Latin letters are kept as they are. Nothing else is rewritten: no kana<->kanji, no
    numeral conversion, no word list.
    """
    t = unicodedata.normalize("NFKC", text).lower()
    return "".join(ch for ch in t if unicodedata.category(ch)[0] not in "PSZC")


def number_insensitive(norm_text: str) -> str:
    """Secondary view: every run of digits / kanji numerals collapses to one '#'.

    Japanese ASR output chooses between '25', '二十五' and '二五' by model convention; this view stops
    that choice from counting as errors. It also hides real digit errors, so it is reported next to
    the primary figure, never instead of it.
    """
    return _NUM_RUN.sub("#", norm_text)


def edits(ref: str, hyp: str):
    """(edit distance, reference length) between two already-normalised strings."""
    return Levenshtein.distance(ref, hyp), len(ref)


def cer_table(golds, hyps):
    """Micro CER (sum of distances / sum of reference characters) for both views, plus per-clip arrays."""
    d1, d2, n1, n2 = [], [], [], []
    for g, h in zip(golds, hyps):
        ng, nh = normalize(g), normalize(h)
        a, b = edits(ng, nh)
        d1.append(a), n1.append(b)
        a, b = edits(number_insensitive(ng), number_insensitive(nh))
        d2.append(a), n2.append(b)
    return np.array(d1), np.array(n1), np.array(d2), np.array(n2)


def micro_cer(d, n):
    return float(d.sum() / n.sum())


def bootstrap_ci(d, n, reps=2000, seed=20261109):
    rng = np.random.default_rng(seed)
    idx = rng.integers(0, len(d), size=(reps, len(d)))
    v = d[idx].sum(1) / n[idx].sum(1)
    return float(np.percentile(v, 2.5)), float(np.percentile(v, 97.5))


def load_eval():
    with open(os.path.join(EVAL, "eval.json"), encoding="utf8") as f:
        return json.load(f)


def read_wav16k(path):
    import soundfile as sf

    x, sr = sf.read(path, dtype="float32")
    assert sr == SR and x.ndim == 1
    return x
