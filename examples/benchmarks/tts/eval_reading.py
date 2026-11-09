"""Round-trip intelligibility proxy, step 2 (venv with pyopenjtalk-plus, rapidfuzz): CER against the input text.

Run:  python -I -X utf8 eval_reading.py <engine> <tag>
Reads stt.json + result.json from $TTS_OUT_DIR/<engine>/<tag>/, writes eval.json.

Two CERs per reply (micro-averaged, Levenshtein over characters, normalisation as EXP-0002):
  text    : hypothesis vs the input text, orthographic. Penalises harmless spelling choices (25 vs 二十五).
  reading : both sides converted to katakana readings with Open JTalk (pyopenjtalk g2p), then compared. Insensitive to
            kanji/kana/numeral spelling, but both sides carry the analyzer's own reading errors.
For the handling set, `expected_reading` (hand-written, hiragana, single annotator) is compared with the reading of the
hypothesis; an item is a "misreading" when its reading CER is above 0.25. URL / symbol / emoji items have no expected reading.
"""
import os, sys, unicodedata

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

import numpy as np
import pyopenjtalk
from rapidfuzz.distance import Levenshtein

from common import dump_json, load_json, normalize, run_dir

MISREAD_THRESHOLD = 0.25


def kata(text):
    t = pyopenjtalk.g2p(unicodedata.normalize("NFKC", text), kana=True)
    return "".join(ch for ch in t if unicodedata.category(ch)[0] not in "PSZC")


def hira2kata(s):
    return "".join(chr(ord(c) + 0x60) if "ぁ" <= c <= "ゖ" else c for c in s)


def cer(pairs):
    d = sum(Levenshtein.distance(a, b) for a, b in pairs)
    n = sum(len(a) for a, _ in pairs)
    return d / n if n else float("nan")


def boot(pairs, reps=2000, seed=20261110):
    rng = np.random.default_rng(seed)
    d = np.array([Levenshtein.distance(a, b) for a, b in pairs])
    n = np.array([len(a) for a, _ in pairs])
    idx = rng.integers(0, len(pairs), size=(reps, len(pairs)))
    v = d[idx].sum(1) / n[idx].sum(1)
    return float(np.percentile(v, 2.5)), float(np.percentile(v, 97.5))


def main(engine, tag):
    out = run_dir(engine, tag)
    res, hyp = load_json(os.path.join(out, "result.json")), load_json(os.path.join(out, "stt.json"))
    tp, rp, per = [], [], []
    hand = []
    for it in res["items"]:
        h = hyp[it["id"]]
        if it["kind"] == "reply" and not it["error"]:
            tp.append((normalize(it["text"]), normalize(h)))
            rp.append((kata(it["text"]), kata(h)))
            per.append({"id": it["id"], "text": it["text"], "hyp": h})
        elif it["kind"] == "handling":
            row = {"id": it["id"], "category": it["category"], "text": it["text"], "hyp": h, "audio_s": it["audio_s"], "error": it["error"]}
            if it["expected_reading"]:
                exp, got = hira2kata(it["expected_reading"]), kata(h)
                row["reading_cer"] = Levenshtein.distance(exp, got) / len(exp)
                row["misread"] = row["reading_cer"] > MISREAD_THRESHOLD or bool(it["error"])
            hand.append(row)
    r = {"engine": engine, "tag": tag, "n_replies": len(tp),
         "cer_text": cer(tp) if tp else None, "cer_text_ci": boot(tp) if tp else None,
         "cer_reading": cer(rp) if rp else None, "cer_reading_ci": boot(rp) if rp else None,
         "replies_cer_text_gt_30pct": sum(1 for a, b in tp if Levenshtein.distance(a, b) / max(1, len(a)) > 0.3),
         "handling": hand}
    scored = [x for x in hand if "misread" in x]
    r["handling_scored"] = len(scored)
    r["handling_misreads"] = sum(1 for x in scored if x["misread"])
    by = {}
    for x in scored:
        b = by.setdefault(x["category"], [0, 0])
        b[0] += 1
        b[1] += int(x["misread"])
    r["handling_by_category"] = by
    dump_json(os.path.join(out, "eval.json"), r)
    print(engine, tag, "CER text %.1f%% reading %.1f%% | handling misreads %d/%d" % (
        100 * (r["cer_text"] or 0), 100 * (r["cer_reading"] or 0), r["handling_misreads"], r["handling_scored"]))


if __name__ == "__main__":
    main(sys.argv[1], sys.argv[2])
