import sys, json, re, statistics; sys.path.insert(0,'.')
from common_reply import *
def jaratio(s):
    ch=[c for c in s if c.isalpha()]; 
    return sum(1 for c in ch if ord(c)>0x3000)/max(1,len(ch))
rows=[]
for k,p in enumerate(PROMPTS):
    for rep in range(2):
        for tag in (False,True):
            r=reply(p,tag=tag,seed=rep*100+k); t=r['text'].strip()
            m=re.match(r"\[affect:\s*(neutral|joy|surprise|calm|sadness|anger)\s*\]\s*(.*)",t,re.S)
            body=m.group(2) if m else re.sub(r"^\[[^\]]*\]\s*","",t)
            rows.append(dict(p=p,tag=tag,rep=rep,ok=bool(m) if tag else None,affect=m.group(1) if m else None,len=len(body),ja=jaratio(body),prompt_ja=jaratio(p)>0.5,first=r['first'],end=r['end'],raw=t))
json.dump(rows,open(DATA+'raw_reply_quality.json','w',encoding='utf-8'),ensure_ascii=False)
def agg(sel):
    return dict(n=len(sel),len_mean=statistics.mean(r['len'] for r in sel),len_p95=pct([r['len'] for r in sel],.95),ja_match=statistics.mean((r['ja']>0.5)==r['prompt_ja'] for r in sel) if False else statistics.mean((r['ja']>0.5) for r in sel if r['prompt_ja']),first_p50=pct([r['first'] for r in sel],.5),end_p50=pct([r['end'] for r in sel],.5))
res=dict(plain=agg([r for r in rows if not r['tag']]),tagged=agg([r for r in rows if r['tag']]),parse_ok=statistics.mean(r['ok'] for r in rows if r['tag']),parse_fail=sum(1 for r in rows if r['tag'] and not r['ok']),tag_dist={a:sum(1 for r in rows if r['affect']==a) for a in CLASSES})
json.dump(res,open(DATA+'results_reply_quality.json','w'),indent=1); print(json.dumps(res,indent=1))
