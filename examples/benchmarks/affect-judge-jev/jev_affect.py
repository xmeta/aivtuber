"""EXP-0007 addendum: System One (Jev wire shape) affect judge, test-only.
Key only from env OPENROUTER_API_KEY; sends only the synthetic blind set; never prints the key.
Usage: python -I jev_affect.py probe | run <data_dir>
"""
import json, os, sys, time, urllib.request, urllib.error
URL = "https://openrouter.ai/api/alpha/decisions"
MODEL = "perplexity/pplx-decider-flash-v1.3:free"
CLASSES = ["neutral", "joy", "surprise", "calm", "sadness", "anger"]
# Same wire shape as crates/reflex/src/jev.rs: {state, model, questions:{name:{type:"choice",instructions,criteria}}}
CRITERIA = {
    "neutral": "Plain information, greeting, thanks or announcement; no feeling voiced.",
    "joy": "The speaker is happy, excited, delighted or laughing.",
    "surprise": "The speaker is astonished, shocked or startled.",
    "calm": "The speaker is relaxed, soothing, reassuring or gentle.",
    "sadness": "The speaker is sad, lonely, disappointed or sorry.",
    "anger": "The speaker is angry, annoyed, irritated or indignant.",
}
INSTR = ("Choose the emotion the VTuber's own spoken line expresses. Judge the speaker's own feeling, not feelings "
         "merely mentioned, quoted or negated; sarcasm counts as the real feeling. Most lines are neutral.")

def body(text):
    return {"model": MODEL, "state": {"line": text},
            "questions": {"affect": {"type": "choice", "instructions": INSTR, "criteria": CRITERIA}}}

def call(text, key):
    req = urllib.request.Request(URL, json.dumps(body(text)).encode(),
                                 {"Content-Type": "application/json", "Authorization": "Bearer " + key})
    t = time.perf_counter()
    try:
        with urllib.request.urlopen(req, timeout=30) as r:
            raw, status = r.read(), r.status
    except urllib.error.HTTPError as e:
        raw, status = e.read(), e.code
    except Exception as e:
        raw, status = repr(e).encode(), -1
    ms = (time.perf_counter() - t) * 1000
    pred, err = None, None
    if status == 200:
        try:
            c = json.loads(raw)["answers"]["affect"]["choice"]
            pred = c if c in CLASSES else None
            if pred is None: err = "bad_class"
        except Exception:
            err = "parse"
    else:
        err = "http_%s" % status
    return dict(pred=pred, err=err, ms=ms, status=status, raw=raw.decode("utf-8", "replace")[:1500])

if __name__ == "__main__":
    key = os.environ["OPENROUTER_API_KEY"]
    if sys.argv[1] == "probe":
        r = call("今日は配信に来てくれてありがとう！", key); print(json.dumps(r, ensure_ascii=False)[:1500])
    else:
        d = sys.argv[2].rstrip("/\\") + "/"
        data = json.load(open(d + "blind.json", encoding="utf-8"))
        out = []
        for i, row in enumerate(data):
            r = call(row["text"], key); r["id"] = row["id"]; out.append(r)
            print(i, r["status"], r["err"], round(r["ms"]), flush=True)
            json.dump(out, open(d + "raw_jev.json", "w", encoding="utf-8"), ensure_ascii=False)
