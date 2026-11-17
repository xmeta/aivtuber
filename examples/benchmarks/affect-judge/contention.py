import sys, threading, time, json, re, statistics; sys.path.insert(0,'.')
from harness import *
PROMPTS = ["今日のゲーム、難しそうだね","初見です！よろしくお願いします","おすすめの曲ある？","さっきの操作かっこよかった","眠いから雑談したいな","明日テストなんだよね","猫飼ってる？","ラスボス強すぎない？","今日の天気どうだった？","歌ってほしいな","ガチャ爆死しました","配信お疲れさま","好きな食べ物は何？","最近ハマってることある？","誕生日おめでとう！","このボス倒せるかな","もう朝だよ","新しいアバター可愛い","PCの音が小さいかも","夏休みの予定は？","hello from Brazil!","what game is this?","you look tired today","do you have a favorite anime?"]
SYS = "あなたは日本語で話す親しみやすいVTuberです。視聴者のコメントに1〜3文、80文字以内で返事をしてください。"
SYS_TAG = SYS + " 返事の先頭に必ず [affect:X] を付けてください。Xは neutral, joy, surprise, calm, sadness, anger のいずれか1語です。"
def reply(p, tag=False, seed=1):
    return stream({"model":"gemma","temperature":0.7,"seed":seed,"max_tokens":96,"messages":[{"role":"system","content":SYS_TAG if tag else SYS},{"role":"user","content":p}]})
stop=threading.Event(); bg=[]
def bgloop(pace, texts):
    i=0
    while not stop.is_set():
        t0=time.perf_counter(); b,ms=judge_b(texts[i%len(texts)]); bg.append(ms); i+=1
        if pace: time.sleep(max(0,pace-(time.perf_counter()-t0)))
d=json.load(open(DATA+'blind.json',encoding='utf-8')); texts=[x['text'] for x in d]
reply(PROMPTS[0]); judge_b(texts[0])  # warm
res={}
def run(cond, pace=None, n=2):
    global bg
    bg=[]; stop.clear(); th=None
    if cond!='alone':
        th=threading.Thread(target=bgloop,args=(pace,texts)); th.start(); time.sleep(0.5)
    out=[]
    for rep in range(n):
        for k,p in enumerate(PROMPTS):
            r=reply(p,seed=rep*100+k); gen=max(r['end']-(r['first'] or 0),1e-9)
            out.append(dict(first=r['first'],tps=(r['chunks']-1)/(gen/1000) if r['chunks']>1 else None,chunks=r['chunks'],end=r['end'],cond=cond,p=p,text=r['text']))
    if th: stop.set(); th.join()
    return out, list(bg)
rows=[]
for cond,pace in [('alone',None),('paced_1.5s',1.5),('saturating',0.0)]:
    pass
# interleave rounds to cancel drift: alone, paced, sat, alone, paced, sat (n=1 each, 24 prompts x2 rounds per cond total... use 2 passes)
agg={c:[] for c in ('alone','paced_1.5s','saturating')}; bgl={c:[] for c in agg}
for rnd in range(2):
    for cond,pace in [('alone',None),('paced_1.5s',1.5),('saturating',0.0)]:
        o,b=run(cond,pace,n=1); 
        for r in o: r['round']=rnd
        agg[cond]+=o; bgl[cond]+=b
summ={}
for c,o in agg.items():
    f=[r['first'] for r in o]; t=[r['tps'] for r in o if r['tps']]
    summ[c]=dict(n=len(o),first_p50=pct(f,.5),first_p95=pct(f,.95),first_mean=statistics.mean(f),tps_mean=statistics.mean(t),tps_p50=pct(t,.5),bg_calls=len(bgl[c]),bg_ms_p50=pct(bgl[c],.5) if bgl[c] else None,bg_ms_p95=pct(bgl[c],.95) if bgl[c] else None)
json.dump(dict(summary=summ,rows=agg,bg=bgl),open(DATA+'raw_contention.json','w',encoding='utf-8'),ensure_ascii=False)
print(json.dumps(summ,indent=1))
