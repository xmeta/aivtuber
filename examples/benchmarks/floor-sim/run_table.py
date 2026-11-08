import os, sys
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from floor_sim import *

cfg = Cfg(take_ms=800, bc_nv_ms=300, prep_ms=400, confirm_run_ms=400, max_obs_age_ms=160)
idle = Own(False, None, True)
spk = Own(True, 900, True)
spk_hard = Own(True, 900, False)
voice = Ev(3, True, "voice")
open_st = St(open=True, base_speech_ms=3000)
rows = []


def r(label, st, obs, own, ev, expect):
    s2, d = decide(st, obs, own, ev, cfg)
    ok = d.act == expect
    rows.append((label, d.act.value, d.level, d.prep.name, d.intr.name, d.duck, d.reason, ok))


def silence(ms, age=0):
    return Obs(10_000, age, False, 0, ms, None, False)


def run(ms, age=0):
    return Obs(10_000, age, True, ms, 0, None, False)


r("idle, open turn, 200 ms silence", open_st, silence(200), idle, voice, Act.WAIT)
r("idle, open turn, 350 ms silence", open_st, silence(350), idle, voice, Act.BACKCHANNEL)
r("idle, open turn, 420 ms silence (prepare starts)", replace_st(open_st, nv_done=True), silence(420), idle, voice, Act.WAIT)
r("idle, open turn, 850 ms silence", open_st, silence(850), idle, voice, Act.TAKE_TURN)
r("idle, user speaking again", replace_st(open_st, prep_active=True), run(64), idle, voice, Act.WAIT)
r("idle, stale observation (age 400 ms) at 850 ms silence", open_st, silence(850, 400), idle, voice, Act.WAIT)
r("idle, no open turn, 2 s silence", St(), silence(2000), idle, voice, Act.WAIT)
r("speaking, user run 64 ms (below tentative)", St(), run(64), spk, voice, Act.HOLD)
r("speaking, user run 160 ms", St(), run(160), spk, voice, Act.HOLD)
r("speaking, user run 450 ms", St(intr=Intr.TENTATIVE), run(450), spk, voice, Act.YIELD)
r("speaking, tentative then 600 ms silence", St(intr=Intr.TENTATIVE), silence(600), spk, voice, Act.HOLD)
r("speaking, stale observation during user run", St(), run(450, 400), spk, voice, Act.HOLD)
r("speaking, no VAD, critical trusted game event, interruptible", St(), Obs(0, 0, None, 0, 0, None, False), spk, Ev(1, True, "game_critical"), Act.INTERRUPT)
r("speaking, no VAD, critical event NOT trusted (normalizer rank 5)", St(), Obs(0, 0, None, 0, 0, None, False), spk, Ev(5, False, "game_critical"), Act.HOLD)
r("speaking, no VAD, critical trusted event, asset not interruptible", St(), Obs(0, 0, None, 0, 0, None, False), spk_hard, Ev(1, True, "game_critical"), Act.HOLD)
r("speaking, no VAD, donation", St(), Obs(0, 0, None, 0, 0, None, False), spk, Ev(2, True, "donation"), Act.BACKCHANNEL)
r("speaking, no VAD, normal chat", St(), Obs(0, 0, None, 0, 0, None, False), spk, Ev(3, True, "chat"), Act.HOLD)
r("idle, no VAD, normal chat", St(), Obs(0, 0, None, 0, 0, None, False), idle, Ev(3, True, "chat"), Act.TAKE_TURN)
r("idle, no VAD, normal chat, busy game segment", St(), Obs(0, 0, None, 0, 0, None, False), idle, Ev(3, True, "chat", True), Act.WAIT)
r("idle, no VAD, critical trusted event, busy game", St(), Obs(0, 0, None, 0, 0, None, False), idle, Ev(1, True, "game_critical", True), Act.TAKE_TURN)
print(f"{'case':66} {'action':12} lvl prep    intr          duck  reason  [as expected]")
for x in rows:
    print(f"{x[0]:66} {x[1]:12} {x[2]:3} {x[3]:7} {x[4]:13} {str(x[5]):5} {x[6]}  [{'ok' if x[7] else 'MISMATCH'}]")
# determinism: same inputs -> same outputs over 1000 repeats
a = decide(open_st, silence(850), idle, voice, cfg)
assert all(decide(open_st, silence(850), idle, voice, cfg) == a for _ in range(1000))
print("determinism over 1000 repeats: ok; mismatches:", sum(1 for x in rows if not x[7]))
