"""EXP-0004b: blind A/B presentation of two models' replies, plus mechanical reply statistics.

    python -I -X utf8 pair_view.py show REPLY_A.json REPLY_B.json KEY.json     # prints blinded pairs, writes the A/B key to KEY.json
    python -I -X utf8 pair_view.py score KEY.json VERDICTS.json                 # VERDICTS: {"case": "A"|"B"|"="}; prints wins per model
    python -I -X utf8 pair_view.py stats REPLY.json [...]                        # length, truncation, markdown, script mix

Which model is A or B is decided per case by a fixed-seed shuffle and stored only in the key file, which the reader
does not open until the verdicts are written.
"""
import json
import random
import re
import sys
from pathlib import Path


def load(p):
    return {r["case"]: r for r in json.loads(Path(p).read_text(encoding="utf-8"))["rows"]}, json.loads(Path(p).read_text(encoding="utf-8"))["model"]


def show(pa, pb, key):
    a, ma = load(pa)
    b, mb = load(pb)
    rnd = random.Random(20261111)
    k = {}
    for case in a:
        swap = rnd.random() < 0.5
        k[case] = {"A": mb if swap else ma, "B": ma if swap else mb}
        x, y = (b[case]["text"], a[case]["text"]) if swap else (a[case]["text"], b[case]["text"])
        print(f"## {case}\nA: {x!r}\nB: {y!r}\n")
    Path(key).write_text(json.dumps(k, indent=1), encoding="utf-8")


def score(key, verdicts):
    k = json.loads(Path(key).read_text(encoding="utf-8"))
    v = json.loads(Path(verdicts).read_text(encoding="utf-8"))
    tally = {}
    for case, w in v.items():
        name = "tie" if w == "=" else k[case][w]
        tally[name] = tally.get(name, 0) + 1
    print(tally)


def stats(paths):
    for p in paths:
        j = json.loads(Path(p).read_text(encoding="utf-8"))
        rows = j["rows"]
        texts = [r["text"] for r in rows]
        n = len(texts)
        kana = lambda t: len(re.findall(r"[぀-ヿ]", t))
        han = lambda t: len(re.findall(r"[一-鿿]", t))
        latin = lambda t: len(re.findall(r"[A-Za-z]", t))
        print(f"{j['model']}: n={n} chars mean {sum(map(len, texts)) / n:.0f} max {max(map(len, texts))}; "
              f"no terminal punctuation {sum(1 for t in texts if not re.search(r'[。！？!?…~〜）)」』]$', t.strip()))}; "
              f"with newline {sum(1 for t in texts if chr(10) in t.strip())}; markdown {sum(1 for t in texts if re.search(r'[*#`]|^- ', t, re.M))}; "
              f"kana-free {sum(1 for t in texts if kana(t) == 0)}; latin letters mean {sum(map(latin, texts)) / n:.1f}; "
              f"contains URL {sum(1 for t in texts if 'http' in t)}")


if __name__ == "__main__":
    {"show": lambda: show(*sys.argv[2:5]), "score": lambda: score(*sys.argv[2:4]), "stats": lambda: stats(sys.argv[2:])}[sys.argv[1]]()
