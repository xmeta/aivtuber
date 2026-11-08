import os, numpy as np, soundfile as sf
from scipy.signal import resample_poly

TB = os.environ["TURN_BENCH_DIR"]  # work directory, outside the repository
SR = 16000
FR = 512  # 32 ms VAD frame


def load16k(path):
    x, sr = sf.read(path, dtype="float32")
    if x.ndim > 1:
        x = x.mean(1)
    if sr != SR:
        from math import gcd
        g = gcd(sr, SR)
        x = resample_poly(x, SR // g, sr // g).astype(np.float32)
    return x


def frame_db(x, hop=160, win=400):
    n = max(0, (len(x) - win) // hop + 1)
    if n == 0:
        return np.zeros(0)
    idx = np.arange(win)[None, :] + hop * np.arange(n)[:, None]
    rms = np.sqrt((x[idx] ** 2).mean(1) + 1e-12)
    return 20 * np.log10(rms)


def ref_bounds(x):
    """Reference speech start/end (seconds) from an energy detector on the clean clip.
    frame (25ms/10ms hop) is speech if dB > max(floor+12, p95-32); floor = 10th pct.
    Isolated runs < 80 ms are dropped; gaps < 150 ms are bridged. Returns (start,end) or None."""
    db = frame_db(x)
    if len(db) < 20:
        return None
    floor = np.percentile(db, 10)
    p95 = np.percentile(db, 95)
    thr = max(floor + 12, p95 - 32)
    sp = db > thr
    # bridge short gaps
    i = 0
    n = len(sp)
    runs = []
    while i < n:
        if sp[i]:
            j = i
            while j < n and sp[j]:
                j += 1
            runs.append([i, j])
            i = j
        else:
            i += 1
    merged = []
    for r in runs:
        if merged and r[0] - merged[-1][1] < 15:
            merged[-1][1] = r[1]
        else:
            merged.append(r)
    merged = [r for r in merged if r[1] - r[0] >= 8]
    if not merged:
        return None
    s = merged[0][0] * 160 / SR
    e = (merged[-1][1] - 1) * 160 / SR + 0.025
    return s, e


def floor_window(x, ms=300):
    """Quietest `ms` window of x (used as room tone)."""
    w = int(ms * SR / 1000)
    if len(x) < w * 2:
        return None
    db = frame_db(x, hop=w // 2, win=w)
    k = int(np.argmin(db))
    return x[k * (w // 2): k * (w // 2) + w].copy()


def tile_floor(win, n, rng):
    """n samples of room tone: random 50ms chunks of the floor window with crossfades."""
    if win is None:
        return (rng.standard_normal(n) * 1e-4).astype(np.float32)
    c = int(0.05 * SR)
    out = np.zeros(n + c, np.float32)
    pos = 0
    fade = np.linspace(0, 1, 160, dtype=np.float32)
    while pos < n:
        s = rng.integers(0, len(win) - c)
        ch = win[s:s + c].copy()
        ch[:160] *= fade
        ch[-160:] *= fade[::-1]
        out[pos:pos + c] += ch
        pos += c - 160
    return out[:n]


def fadein(x, ms=10):
    k = int(ms * SR / 1000)
    x = x.copy()
    x[:k] *= np.linspace(0, 1, k, dtype=np.float32)
    return x


def fadeout(x, ms=10):
    k = int(ms * SR / 1000)
    x = x.copy()
    x[-k:] *= np.linspace(1, 0, k, dtype=np.float32)
    return x


def rms_speech(x, b):
    s, e = int(b[0] * SR), int(b[1] * SR)
    seg = x[s:e]
    return float(np.sqrt((seg ** 2).mean() + 1e-12))


def pink(n, rng):
    w = rng.standard_normal(n)
    f = np.fft.rfft(w)
    k = np.arange(len(f))
    k[0] = 1
    f = f / np.sqrt(k)
    p = np.fft.irfft(f, n)
    return (p / np.sqrt((p ** 2).mean())).astype(np.float32)
