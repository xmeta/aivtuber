import json, sys, time, random, re, urllib.request, collections, statistics, threading
URL = "http://127.0.0.1:18090/v1/chat/completions"
DATA = "I:/temp/expaffect-data/"
CLASSES = ["neutral", "joy", "surprise", "calm", "sadness", "anger"]

def post(body):
    req = urllib.request.Request(URL, json.dumps(body).encode(), {"Content-Type": "application/json"})
    t = time.perf_counter()
    with urllib.request.urlopen(req, timeout=120) as r:
        out = json.loads(r.read())
    return out, (time.perf_counter() - t) * 1000

def stream(body):
    body = dict(body, stream=True)
    req = urllib.request.Request(URL, json.dumps(body).encode(), {"Content-Type": "application/json"})
    t = time.perf_counter(); first = None; n = 0; text = ""
    with urllib.request.urlopen(req, timeout=180) as r:
        for raw in r:
            raw = raw.strip()
            if not raw.startswith(b"data:") or raw.endswith(b"[DONE]"):
                continue
            ch = json.loads(raw[5:])
            if not ch.get("choices"): continue
            c = ch["choices"][0]["delta"].get("content")
            if c:
                if first is None: first = (time.perf_counter() - t) * 1000
                n += 1; text += c
    end = (time.perf_counter() - t) * 1000
    return dict(first=first, end=end, chunks=n, text=text)

DEFS = ("neutral = plain information, no feeling; joy = happy, excited, delighted, laughing; "
        "surprise = astonished, shocked, startled; calm = relaxed, soothing, reassuring, gentle; "
        "sadness = sad, lonely, disappointed, sorry; anger = angry, annoyed, irritated, indignant.")
PROMPT_B = ("You label the emotion that a VTuber's spoken line expresses. Answer with exactly one word from: "
            "neutral, joy, surprise, calm, sadness, anger. " + DEFS +
            " Judge the speaker's own feeling, not feelings merely mentioned, quoted or negated; "
            "sarcasm counts as the real (not the literal) feeling. Most lines are neutral: greetings, thanks, announcements, facts and plain remarks are neutral. Choose another class only when the line itself clearly voices that feeling; otherwise answer neutral.")

def parse_word(s):
    s = s.strip().lower()
    m = re.search(r"\b(neutral|joy|surprise|calm|sadness|anger)\b", s)
    return m.group(1) if m else None

def judge_b(text, seed=0):
    out, ms = post({"model": "gemma", "temperature": 0, "max_tokens": 8, "seed": seed,
                    "messages": [{"role": "system", "content": PROMPT_B}, {"role": "user", "content": text}]})
    return parse_word(out["choices"][0]["message"]["content"] or ""), ms

PROMPT_C = ("You are a Japanese VTuber. You are given a line you are about to say. Do not change, add or remove anything in the line. Output exactly "
            "'[affect:X] ' followed by the line unchanged, where X is the one emotion the line expresses, one of: "
            "neutral, joy, surprise, calm, sadness, anger. " + DEFS +
            " Judge the speaker's own feeling, not feelings merely mentioned, quoted or negated; sarcasm counts as "
            "the real feeling. Most lines are neutral: greetings, thanks, announcements, facts and plain remarks are neutral. Use another class only when the line itself clearly voices that feeling. The tag must use exactly one of the six class names.")

def judge_c(text):
    out, ms = post({"model": "gemma", "temperature": 0, "max_tokens": 120,
                    "messages": [{"role": "system", "content": PROMPT_C}, {"role": "user", "content": text}]})
    content = (out["choices"][0]["message"]["content"] or "").strip()
    m = re.match(r"\[affect:\s*(neutral|joy|surprise|calm|sadness|anger)\s*\]\s*(.*)", content, re.S)
    return (m.group(1) if m else None), (m.group(2) if m else content), ms, out["usage"]["completion_tokens"]

def load():
    d = json.load(open(DATA + "blind.json", encoding="utf-8"))
    rng = random.Random(20261118)
    dev = set(rng.sample(range(len(d)), 20))
    return d, dev

def metrics(gold, pred):
    # pred None (parse failure) counts as wrong and as its own non-class
    n = len(gold); acc = sum(g == p for g, p in zip(gold, pred)) / n
    f1s = {}
    for c in CLASSES:
        tp = sum(g == c and p == c for g, p in zip(gold, pred))
        fp = sum(g != c and p == c for g, p in zip(gold, pred))
        fn = sum(g == c and p != c for g, p in zip(gold, pred))
        f1s[c] = 0 if tp == 0 else 2 * tp / (2 * tp + fp + fn)
    neu = [(g, p) for g, p in zip(gold, pred) if g == "neutral"]
    fnn = sum(p not in ("neutral", None) for g, p in neu) / len(neu)
    return dict(n=n, acc=round(acc, 3), macro_f1=round(sum(f1s.values()) / 6, 3), false_non_neutral=round(fnn, 3),
                f1=f1s, parse_fail=sum(p is None for p in pred))

def pct(xs, q):
    xs = sorted(xs); k = (len(xs) - 1) * q; f = int(k); c = min(f + 1, len(xs) - 1)
    return xs[f] + (xs[c] - xs[f]) * (k - f)
