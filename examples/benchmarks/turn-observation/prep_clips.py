import sys, os; sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import os, re, json, glob, random
import numpy as np, pandas as pd, soundfile as sf
from lib import *

random.seed(20261108)
tsv = pd.read_csv(os.path.join(TB, "dl", "cv22ja", "test.tsv"), sep="\t", quoting=3, dtype=str)
root = glob.glob(os.path.join(TB, "dl", "cv22ja", "x", "ja_test_0", "*"))
print("dirs", root[:1])
files = {}
for d in root:
    if os.path.isdir(d):
        for f in os.listdir(d):
            files[f] = os.path.join(d, f)
    else:
        files[os.path.basename(d)] = d
print("files", len(files), "rows", len(tsv))
tsv = tsv[tsv.path.isin(files)]
tsv = tsv[(tsv.down_votes.astype(int) == 0) & (tsv.up_votes.astype(int) >= 2)]
print("after vote filter", len(tsv))
cnt = tsv.client_id.value_counts()
tsv["ncl"] = tsv.client_id.map(cnt)
rows = tsv.sample(frac=1.0, random_state=20261108).to_dict("records")

part = re.compile(r"(けど|けれど|から|ので|て|が)(?=.{4,})")
out = os.path.join(TB, "audio", "clips")
os.makedirs(out, exist_ok=True)
info = {}
want = {"A": 220, "B1": 400, "B2": 520}
got = {k: 0 for k in want}
for r in rows:
    if all(got[k] >= want[k] for k in want):
        break
    sent = r["sentence"]
    role = "B2" if part.search(sent) and got["B2"] < want["B2"] else None
    if role is None:
        if r["ncl"] >= 6 and got["B1"] < want["B1"]:
            role = "B1"
        elif got["A"] < want["A"]:
            role = "A"
    if role is None:
        continue
    x = load16k(files[r["path"]])
    b = ref_bounds(x)
    if b is None or not (1.2 <= b[1] - b[0] <= 9.0):
        continue
    if b[0] < 0.0:
        continue
    pk = np.abs(x).max()
    if pk < 0.02:
        continue
    name = r["path"].replace(".mp3", "")
    sf.write(os.path.join(out, name + ".wav"), x, SR, subtype="FLOAT")
    info[name] = dict(role=role, client=r["client_id"], sentence=sent, start=b[0], end=b[1], dur=len(x) / SR)
    got[role] += 1
print(got)
json.dump(info, open(os.path.join(TB, "audio", "clips.json"), "w"), ensure_ascii=False, indent=0)
