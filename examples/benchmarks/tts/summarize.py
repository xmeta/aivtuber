"""Plain-text summary tables over $TTS_OUT_DIR (all engines that have a run1/result.json).

Run:  python -I -X utf8 summarize.py [engine-substring ...] > summary.txt
Latency: wall time of one synthesis call per reply, p50/p95 over the 50 replies (first call and 3 warm-ups excluded).
Time to first audio = whole-call time unless the engine reports a streaming first chunk (then that is used and flagged).
RTF = sum of call times / sum of audio durations. Determinism: the same text, same process (pass1 vs pass2) and a fresh
process (run1 vs run2), as 16-bit PCM sha256.
"""
import glob, json, os, sys

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

import numpy as np

from common import OUT, load_json


def main(filters):
    rows = []
    for d in sorted(glob.glob(os.path.join(OUT, "*"))):
        name = os.path.basename(d)
        if filters and not any(f in name for f in filters):
            continue
        runs = {os.path.basename(r): load_json(os.path.join(r, "result.json")) for r in sorted(glob.glob(os.path.join(d, "run*"))) if os.path.exists(os.path.join(r, "result.json"))}
        if "run1" not in runs:
            continue
        r = runs["run1"]
        rep = [i for i in r["items"] if i["kind"] == "reply" and not i["error"]]
        if not rep:
            rows.append((name, "no successful reply: " + str(next((i["error"] for i in r["items"] if i["error"]), "")), r))
            continue
        w = np.array([i["wall_s"] for i in rep])
        f = np.array([i["ttfa_s"] for i in rep])
        a = np.array([i["audio_s"] for i in rep])
        same_proc = sum(1 for i in rep if i["sha"] == i.get("sha_pass2"))
        fresh = None
        if "run2" in runs:
            b = {i["id"]: i["sha"] for i in runs["run2"]["items"] if i["kind"] == "reply"}
            fresh = sum(1 for i in rep if b.get(i["id"]) == i["sha"])
        ev = os.path.join(d, "run1", "eval.json")
        e = load_json(ev) if os.path.exists(ev) else None
        rows.append((name, None, r, dict(n=len(rep), p50=np.percentile(w, 50), p95=np.percentile(w, 95), mean=w.mean(), ttfa50=np.percentile(f, 50),
                                         stream=any(i["ttfa_is_streaming"] for i in rep), rtf=w.sum() / a.sum(), audio=a.mean(), same=same_proc, fresh=fresh, e=e)))
    print("%-32s %3s %7s %7s %7s %6s %6s %7s %7s %9s %9s %s" % ("engine", "n", "p50 s", "p95 s", "ttfa50", "RTF", "load s", "1st s", "RSS MB", "same-proc", "fresh-proc", "CER text/reading %"))
    for row in rows:
        name, err, r = row[:3]
        if err:
            print(name, "-", err)
            continue
        m = row[3]
        e = m["e"]
        c = "%.1f (%.1f-%.1f) / %.1f (%.1f-%.1f)" % (100 * e["cer_text"], 100 * e["cer_text_ci"][0], 100 * e["cer_text_ci"][1], 100 * e["cer_reading"], 100 * e["cer_reading_ci"][0], 100 * e["cer_reading_ci"][1]) if e else "-"
        print("%-32s %3d %7.3f %7.3f %7.3f %6.3f %6.1f %7.2f %7.0f %6d/%-3d %6s %s" % (
            name, m["n"], m["p50"], m["p95"], m["ttfa50"], m["rtf"], r["load_s"], r["first_call_s"], r["peak_rss_mb"], m["same"], m["n"],
            "-" if m["fresh"] is None else "%d/%d" % (m["fresh"], m["n"]), c))
    print()
    print("handling set (misreadings / scored items; the scored items are those with an expected reading):")
    for row in rows:
        if row[1] or not row[3]["e"]:
            continue
        e = row[3]["e"]
        print("  %-32s %d/%d  by category %s" % (row[0], e["handling_misreads"], e["handling_scored"], json.dumps(e["handling_by_category"], ensure_ascii=False)))


if __name__ == "__main__":
    main(sys.argv[1:])
