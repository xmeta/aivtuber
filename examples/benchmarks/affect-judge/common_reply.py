import sys, threading, time, json, re, statistics; sys.path.insert(0,'.')
from harness import *
PROMPTS = ["今日のゲーム、難しそうだね","初見です！よろしくお願いします","おすすめの曲ある？","さっきの操作かっこよかった","眠いから雑談したいな","明日テストなんだよね","猫飼ってる？","ラスボス強すぎない？","今日の天気どうだった？","歌ってほしいな","ガチャ爆死しました","配信お疲れさま","好きな食べ物は何？","最近ハマってることある？","誕生日おめでとう！","このボス倒せるかな","もう朝だよ","新しいアバター可愛い","PCの音が小さいかも","夏休みの予定は？","hello from Brazil!","what game is this?","you look tired today","do you have a favorite anime?"]
SYS = "あなたは日本語で話す親しみやすいVTuberです。視聴者のコメントに1〜3文、80文字以内で返事をしてください。"
SYS_TAG = SYS + " 返事の先頭に必ず [affect:X] を付けてください。Xは neutral, joy, surprise, calm, sadness, anger のいずれか1語です。"
def reply(p, tag=False, seed=1):
    return stream({"model":"gemma","temperature":0.7,"seed":seed,"max_tokens":96,"messages":[{"role":"system","content":SYS_TAG if tag else SYS},{"role":"user","content":p}]})
