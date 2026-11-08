# Floor-policy replay simulator (ADR-0005, #81)

Throwaway scripts behind [`docs/adr/0005-conversational-floor-policy.adoc`](../../../docs/adr/0005-conversational-floor-policy.adoc).
They are not production code, are not imported by any crate or TypeScript package, and **CI does not run them**.
The data are synthetic (EXP-0001 constructions); read the ADR's Known limitations before using any number.

No audio, models, per-stream scores or results are committed, and the scripts contain no machine paths.
They read the **saved outputs of EXP-0001** (`out/eval_clean.pkl`, `out/eval_noisy15.pkl`) and `audio/labels.json` from the
EXP-0001 work directory given by `TURN_BENCH_DIR`; produce those first with
[`../turn-observation/README.md`](../turn-observation/README.md). Results are written to `$TURN_BENCH_DIR/out/floor-sim/`.

Only `numpy` is needed (pinned in `requirements.txt`; the EXP-0001 pickles hold numpy arrays). Python 3.13.

```sh
export TURN_BENCH_DIR=/path/outside/the/repo/turn-bench      # PowerShell: $env:TURN_BENCH_DIR = "..."
PY="python -I -X utf8"
$PY repro.py                      # replays EXP-0001's own fixed-silence rule; must reproduce its published counts (145/83/7 of 194 clean)
$PY run_listen.py clean           # policies 1-3: TakeTurn false rate/lag, backchannel tiers, early prepare (also: noisy15)
$PY run_bargein.py clean          # barge-in on the performer's speech: duck/confirm/stop latency, burst surrogate (also: noisy15)
$PY run_stale.py clean            # stalled observation feed with and without the freshness rule (also: noisy15)
$PY run_overlap.py                # overlap of a spoken backchannel with resumed speech
$PY run_table.py                  # 20-row sanity table for the pure decide() function and a determinism check
```

`floor_sim.py` holds the pure `decide(state, obs, own, ev, cfg)` function and the frame-by-frame observation adapter.
Assumed constants (duck 50 ms, dispatch 50 ms, 400 ms backchannel asset, 8 s reply, 600 ms stall, preparation times P) are
parameters in the scripts, not measurements. Random draws use seed 20261108.
