import sys, os; sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import re, glob, json, numpy as np, pandas as pd, soundfile as sf
from lib import *
from vadlib import *
from chainlib import chain_bounds

clips = json.load(open(os.path.join(TB, "audio", "clips.json"), encoding="utf-8"))
tsv = pd.read_csv(os.path.join(TB, "dl", "cv22ja", "test.tsv"), sep="\t", quoting=3, dtype=str)
files = {}
for d in glob.glob(os.path.join(TB, "dl", "cv22ja", "x", "ja_test_0", "*")):
    files[os.path.basename(d)] = d
tsv = tsv[tsv.path.isin(files)]
tsv = tsv[(tsv.down_votes.astype(int) == 0) & (tsv.up_votes.astype(int) >= 2)]
pat = re.compile(r"(けれども|けれど|けど|から|ので|のに|が|て)、")
tsv = tsv[tsv.sentence.map(lambda s: bool(pat.search(s)) and s.count("、") <= 2)]
tsv = tsv.sample(frac=1.0, random_state=20261108)
print("text-eligible clips", len(tsv))
V = SileroOrt(SILERO_V6)
out = os.path.join(TB, "audio", "clips")
added = 0
for r in tsv.to_dict("records"):
    name = r["path"].replace(".mp3", "")
    if name in clips:
        continue
    x = load16k(files[r["path"]])
    b = chain_bounds(frame_db(x))
    if b is None or not (1.2 <= b[1] - b[0] <= 9.0):
        continue
    sg = segments(run_probs(V, x))
    if not sg:
        continue
    vs, ve = sg[0][0] * FR / SR, sg[-1][1] * FR / SR
    if abs(vs - b[0]) > 0.4 or abs(ve - b[1]) > 0.4:
        continue
    sf.write(os.path.join(out, name + ".wav"), x, SR, subtype="FLOAT")
    clips[name] = dict(role="B2x", client=r["client_id"], sentence=r["sentence"], start=b[0], end=b[1], vad_start=vs, vad_end=ve, dur=len(x) / SR)
    added += 1
print("added", added)
json.dump(clips, open(os.path.join(TB, "audio", "clips.json"), "w", encoding="utf-8"), ensure_ascii=False)
