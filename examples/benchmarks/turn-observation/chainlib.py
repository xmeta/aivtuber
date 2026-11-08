import numpy as np


def chain_bounds(db, HOP=0.01):
    """Energy-chain speech bounds: frames > p95-32 dB; runs >=120 ms; runs closer than 400 ms chain together;
    the chain with the most voiced time is the utterance (drops clicks/thumps). Returns (start, end, thr)."""
    thr = np.percentile(db, 95) - 32
    sp = db > thr
    runs = []
    i, n = 0, len(sp)
    while i < n:
        if sp[i]:
            j = i
            while j < n and sp[j]:
                j += 1
            if j - i >= 12:
                runs.append([i, j])
            i = j
        else:
            i += 1
    if not runs:
        return None
    chains = [[runs[0][0], runs[0][1], runs[0][1] - runs[0][0]]]
    for r in runs[1:]:
        if r[0] - chains[-1][1] < 40:
            chains[-1][1] = r[1]
            chains[-1][2] += r[1] - r[0]
        else:
            chains.append([r[0], r[1], r[1] - r[0]])
    c = max(chains, key=lambda c: c[2])
    return c[0] * HOP, (c[1] - 1) * HOP + 0.025, thr
