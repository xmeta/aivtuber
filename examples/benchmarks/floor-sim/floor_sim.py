"""Throwaway floor-policy replay simulator for ADR-0005 (#81).

Pure function  decide(state, obs, own, ev, cfg) -> (state', Decision)
Replays EXP-0001 (#92) per-frame Silero v6 speech probabilities (saved scores) as causal
observations. Everything about the data is synthetic (see EXP-0001 section 4 and 9).
Not production code; nothing here is imported by the repository.
"""
from __future__ import annotations
import os
import json, math, os, pickle, random, sys
from dataclasses import dataclass, replace
from enum import Enum
from typing import NamedTuple, Optional

TB = os.environ["TURN_BENCH_DIR"]  # EXP-0001 work directory (saved scores), outside the repository
FT = 32                    # ms per VAD frame (512 samples at 16 kHz)
ON_T, OFF_T = 0.5, 0.35    # EXP-0001 hysteresis
CONFIRM_RUN_MS = 256       # EXP-0001 min speech (8 frames); a run is "real" once it reaches this
ST_LATENCY_MS = 120        # Smart Turn inference p95 ~119 ms on 1 CPU thread (EXP-0001 5.2); added to its availability
INF = 10 ** 9


# ---------------------------------------------------------------- domain-ish types
class Act(Enum):
    HOLD = "Hold"
    YIELD = "Yield"
    BACKCHANNEL = "Backchannel"
    TAKE_TURN = "TakeTurn"
    INTERRUPT = "Interrupt"
    WAIT = "Wait"


class Prep(Enum):
    NONE = 0
    START = 1
    CANCEL = 2


class Intr(Enum):  # tentative-interruption sub-state, only meaningful while performer speaks
    NONE = 0
    TENTATIVE = 1
    CONFIRMED = 2
    FALSE_RESUME = 3


class Obs(NamedTuple):
    t_ms: int                  # time this observation is valid for (logical ms)
    age_ms: int                # now - producer time; stale if > cfg.max_obs_age_ms
    speech_active: Optional[bool]   # None = no VAD (text-chat-only)
    run_ms: int                # length of the current raw speech run
    silence_ms: int            # time since last raw speech offset (0 while active / unknown)
    score: Optional[float]     # Smart Turn style provider score
    calibrated: bool           # #92 flag: only a calibrated score may be treated as probability


class Own(NamedTuple):
    speaking: bool
    ms_to_boundary: Optional[int]   # scheduler-owned: time to next declared safe interrupt point
    interruptible: bool
    basis: str = "LogicalTimeline"  # #222 D-6: not audible truth


class Ev(NamedTuple):
    prio_rank: int             # scheduler Priority::rank (0 Operator .. 5 Background)
    trusted: bool              # assigned by normalizer, never by payload
    kind: str = "voice"        # voice | chat | donation | game_critical | game | none
    busy_game: bool = False    # scheduler/game-segment busy flag


@dataclass(frozen=True)
class Cfg:
    take_ms: int = INF              # TakeTurn silence threshold (irreversible)
    bc_nv_ms: Optional[int] = None  # non-verbal backchannel (ladder 1-3) threshold
    bc_voc_ms: Optional[int] = None  # short cached vocal backchannel (ladder 4) threshold
    bc_voc_min_speech_ms: int = 1000
    prep_ms: Optional[int] = None   # early-prepare threshold
    prep_uses_score: bool = False   # also start prepare on an (uncalibrated) provider score
    prep_score_thr: float = 0.5
    max_prep_per_turn: int = 3      # bound (LiveKit preemptive max_retries = 3)
    max_obs_age_ms: int = INF       # stale-observation rejection
    # barge-in (performer speaking)
    tent_run_ms: int = 96           # raw run before Tentative (3 frames)
    confirm_run_ms: int = 400       # raw run before Confirmed
    hard_cancel: bool = False       # baseline: any tentative = confirmed (OLV / Pipecat default)
    resume_holdoff_ms: int = 500    # silence required before a tentative interruption is declared false


class St(NamedTuple):
    open: bool = False
    nv_done: bool = False
    voc_done: bool = False
    prep_active: bool = False
    prep_start_ms: int = -1
    prep_n: int = 0
    base_speech_ms: int = 0   # folded speech of finished runs in the open turn
    cur_run_ms: int = 0       # current raw run
    intr: Intr = Intr.NONE


class Dec(NamedTuple):
    act: Act
    level: int = 0                  # backchannel ladder 0..5
    prep: Prep = Prep.NONE
    intr: Intr = Intr.NONE
    duck: bool = False
    reason: str = ""


def decide(st: St, obs: Obs, own: Own, ev: Ev, c: Cfg):
    """Pure, total, deterministic. No clock, no I/O, no randomness."""
    stale = obs.age_ms > c.max_obs_age_ms

    # ---- no VAD: only event-driven path (text-chat-only fallback)
    if obs.speech_active is None:
        return st, event_only(own, ev)

    # ---- performer speaking: barge-in path (tentative -> confirmed / false -> resume)
    if own.speaking:
        if stale:
            return st, Dec(Act.HOLD, reason="stale_obs_hold")  # never yield on stale evidence
        if obs.speech_active and obs.run_ms >= c.tent_run_ms:
            confirmed = c.hard_cancel or obs.run_ms >= c.confirm_run_ms
            if confirmed and ev.prio_rank >= 0:
                n = replace_st(st, intr=Intr.CONFIRMED)
                return n, Dec(Act.YIELD, intr=Intr.CONFIRMED, duck=True, reason="confirmed")
            n = replace_st(st, intr=Intr.TENTATIVE)
            return n, Dec(Act.HOLD, intr=Intr.TENTATIVE, duck=True, reason="tentative")
        if st.intr == Intr.TENTATIVE and not obs.speech_active:
            if obs.silence_ms < c.resume_holdoff_ms:
                return st, Dec(Act.HOLD, intr=Intr.TENTATIVE, duck=True, reason="tentative_wait_silence")
            return replace_st(st, intr=Intr.NONE), Dec(Act.HOLD, intr=Intr.FALSE_RESUME, reason="false_interrupt_resume")
        if st.intr == Intr.CONFIRMED:
            return st, Dec(Act.YIELD, intr=Intr.CONFIRMED, reason="confirmed_sticky")
        if st.intr == Intr.TENTATIVE:
            return st, Dec(Act.HOLD, intr=Intr.TENTATIVE, duck=True, reason="tentative_flicker")
        return st, Dec(Act.HOLD, reason="holding")

    # ---- performer idle: listening path
    if stale:
        # cannot trust the silence clock: cancel anything speculative, commit nothing
        pc = Prep.CANCEL if st.prep_active else Prep.NONE
        return replace_st(st, prep_active=False), Dec(Act.WAIT, prep=pc, reason="stale_obs_wait")

    if obs.speech_active:
        n = st
        if obs.run_ms >= CONFIRM_RUN_MS:
            n = replace_st(n, open=True)
        pc = Prep.CANCEL if st.prep_active else Prep.NONE
        n = replace_st(n, nv_done=False, voc_done=False, prep_active=False, cur_run_ms=obs.run_ms)
        return n, Dec(Act.WAIT, prep=pc, reason="user_speaking")

    if st.cur_run_ms:
        st = replace_st(st, base_speech_ms=st.base_speech_ms + st.cur_run_ms, cur_run_ms=0)
    if not st.open:
        return st, Dec(Act.WAIT, reason="no_open_turn")

    s = obs.silence_ms
    if s >= c.take_ms:
        n = St()  # turn closed; commit barrier consumes prepared work
        return n, Dec(Act.TAKE_TURN, reason="silence_commit")

    act, level, prep = Act.WAIT, 0, Prep.NONE
    n = st
    if c.bc_voc_ms is not None and s >= c.bc_voc_ms and not st.voc_done:
        if st.base_speech_ms >= c.bc_voc_min_speech_ms:
            act, level = Act.BACKCHANNEL, 4
            n = replace_st(n, voc_done=True, nv_done=True)
    if act == Act.WAIT and c.bc_nv_ms is not None and s >= c.bc_nv_ms and not st.nv_done:
        act, level = Act.BACKCHANNEL, 3
        n = replace_st(n, nv_done=True)
    start = False
    if not st.prep_active and st.prep_n < c.max_prep_per_turn:
        if c.prep_ms is not None and s >= c.prep_ms:
            start = True
        if c.prep_uses_score and obs.score is not None and obs.score >= c.prep_score_thr:
            start = True   # uncalibrated score: allowed to START speculative work only, never to commit
    if start:
        prep = Prep.START
        n = replace_st(n, prep_active=True, prep_start_ms=obs.t_ms, prep_n=st.prep_n + 1)
    return n, Dec(act, level=level, prep=prep, reason="silence_tiers")


def replace_st(st: St, **kw) -> St:
    d = st._asdict(); d.update(kw); return St(**d)


def event_only(own: Own, ev: Ev) -> Dec:
    """Text-chat-only / no-VAD path: no silence clock exists; the typed message boundary is the turn end."""
    if own.speaking:
        if ev.prio_rank <= 1 and ev.trusted and own.interruptible:
            return Dec(Act.INTERRUPT, reason="critical_trusted")
        if ev.kind == "donation":
            return Dec(Act.BACKCHANNEL, level=4, reason="donation_ack")
        return Dec(Act.HOLD, level=0, reason="defer_while_speaking")
    if ev.kind == "none":
        return Dec(Act.WAIT, reason="nothing")
    if ev.busy_game and ev.prio_rank >= 3:
        return Dec(Act.WAIT, reason="defer_busy_game")
    return Dec(Act.TAKE_TURN, reason="complete_message")


# ---------------------------------------------------------------- observation adapter (VAD frames -> Obs)
class Adapter:
    """Online hysteresis over per-frame speech probability (EXP-0001 thresholds). Causal: the observation for
    frame i is available at (i+1)*FT."""

    def __init__(self, score_lookup=None):
        self.act = False
        self.run_start = 0
        self.off_f = None
        self.lookup = score_lookup
        self.turn_speech = 0

    def feed(self, i: int, p: float, age_ms: int = 0, silence_idx_ok=True):
        if not self.act and p >= ON_T:
            self.act, self.run_start = True, i
        elif self.act and p < OFF_T:
            self.act, self.off_f = False, i
        t = (i + 1) * FT
        if self.act:
            return Obs(t, age_ms, True, (i - self.run_start + 1) * FT, 0, None, False)
        sil = (t - self.off_f * FT) if self.off_f is not None else 0
        score = None
        if self.lookup is not None and self.off_f is not None:
            score = self.lookup(self.off_f, t)
        return Obs(t, age_ms, False, 0, sil, score, False)


# ---------------------------------------------------------------- replay of one stream
def load(cond):
    R = pickle.load(open(f"{TB}/out/eval_{cond}.pkl", "rb"))
    L = json.load(open(f"{TB}/audio/labels.json", encoding="utf-8"))["streams"]
    return R, L


def raw_runs(p):
    act, runs, st = False, [], 0
    for i, v in enumerate(p):
        if not act and v >= ON_T:
            act, st = True, i
        elif act and v < OFF_T:
            act = False; runs.append((st, i))
    if act:
        runs.append((st, len(p)))
    return runs


def segments(p, min_frames=8):
    return [r for r in raw_runs(p) if r[1] - r[0] >= min_frames]


def st_lookup_factory(r, segs):
    """Smart Turn score available at time t for the silence run that started at frame off_f. The score for
    poll k is only available at its tt + inference latency (causal)."""
    by_end = {b: gi for gi, (a, b) in enumerate(segs)}
    stt = r["st"]

    def look(off_f, t):
        gi = by_end.get(off_f)
        if gi is None:
            return None
        best = None
        for k in range(200, 1501, 100):
            v = stt.get((gi, k))
            if v is None:
                break
            prob, tt = v
            if tt * 1000 + ST_LATENCY_MS <= t:
                best = prob
        return best
    return look


def replay_listen(r, cfg: Cfg, use_st=False, stall=None):
    """Replay one stream through decide(). stall=(t0,t1): the observation feed is not delivered in [t0,t1);
    the policy is still ticked every frame with the last delivered observation, extrapolated, with age set."""
    p = r["p"]
    segs = segments(p)
    ad = Adapter(st_lookup_factory(r, segs) if use_st else None)
    st, own, ev = St(), Own(False, None, True), Ev(3, True, "voice")
    takes, bcs, preps, open_prep = [], [], [], None
    ld = None
    for i in range(len(p)):
        now = (i + 1) * FT
        o = ad.feed(i, float(p[i]), 0)
        if stall is not None and stall[0] <= now < stall[1] and ld is not None:
            age = now - ld.t_ms
            if ld.speech_active:
                o = ld._replace(t_ms=now, age_ms=age, run_ms=ld.run_ms + age)
            else:
                o = ld._replace(t_ms=now, age_ms=age, silence_ms=ld.silence_ms + age)
        else:
            ld = o
        st, d = decide(st, o, own, ev, cfg)
        if d.prep == Prep.START:
            open_prep = now
        if d.prep == Prep.CANCEL and open_prep is not None:
            preps.append((open_prep, now, "cancelled")); open_prep = None
        if d.act == Act.BACKCHANNEL:
            bcs.append((now, d.level))
        if d.act == Act.TAKE_TURN:
            takes.append(now)
            if open_prep is not None:
                preps.append((open_prep, now, "committed")); open_prep = None
    if open_prep is not None:
        preps.append((open_prep, None, "dangling"))
    return dict(takes=takes, bcs=bcs, preps=preps, segs=segs)


# ---------------------------------------------------------------- statistics
def wilson(k, n):
    if n == 0:
        return (0.0, 0.0)
    z = 1.96; p = k / n; den = 1 + z * z / n
    c = (p + z * z / (2 * n)) / den
    h = z * math.sqrt(p * (1 - p) / n + z * z / (4 * n * n)) / den
    return c - h, c + h


def pct(k, n):
    lo, hi = wilson(k, n)
    return f"{100*k/max(n,1):.0f}% ({k}/{n}; {100*lo:.0f}-{100*hi:.0f})"


def pctl(xs, q):
    if not xs:
        return float("nan")
    xs = sorted(xs); k = (len(xs) - 1) * q
    f = math.floor(k); c = min(f + 1, len(xs) - 1)
    return xs[f] + (xs[c] - xs[f]) * (k - f)
