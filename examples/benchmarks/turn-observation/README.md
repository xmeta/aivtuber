# Turn-observation bench (EXP-0001, #92)

Throwaway measurement scripts behind
[`docs/experiments/EXP-0001-japanese-turn-observation-bench.adoc`](../../../docs/experiments/EXP-0001-japanese-turn-observation-bench.adoc).
They are not production code, are not imported by any crate or TypeScript package, and **CI does not run them**.
Read the record's limitations (section 9) before using any number they produce.

Nothing large lives in the repository: no audio, models, virtual environments or per-stream results.
Everything is written under one work directory outside the repository, given by the environment variable `TURN_BENCH_DIR`.
The scripts contain no machine paths.

## Layout of `$TURN_BENCH_DIR`

```
dl/      downloaded third-party material (untrusted data; created by fetch_assets.py)
audio/   clips/, streams/, clips.json, labels.json, asr_all.json   (generated)
out/     eval_*.pkl, analysis_*.json, timing.json, footprint.json  (generated)
```

Keep the work directory away from the scripts directory and run Python with `-I` (isolated mode: no current-directory or `PYTHON*` path lookups) and `-X utf8`.
The scripts add their own directory to `sys.path` explicitly.

## Reproduction

Python 3.13, CPU only. Free disk needed: roughly 2 GB, plus 0.5 GB if you add the optional vendor shard.

```sh
python -m venv venv && venv/bin/pip install -r requirements.txt        # Windows: venv\Scripts\...
export TURN_BENCH_DIR=/path/outside/the/repo/turn-bench                  # PowerShell: $env:TURN_BENCH_DIR = "..."
PY="python -I -X utf8"

$PY fetch_assets.py [--with-vendor-shard]   # pinned downloads, sha256-verified, extracts the Common Voice tar
$PY prep_clips.py                           # decode + pick clips; writes audio/clips/ and audio/clips.json
$PY asr_bounds.py                           # ReazonSpeech text-similarity check; writes audio/asr_all.json
cp "$TURN_BENCH_DIR/audio/clips.json" "$TURN_BENCH_DIR/audio/clips_raw.json"
$PY refine2.py                              # energy-chain reference bounds + VAD-agreement filter -> clips.json
$PY prep_b2.py                              # adds clips with a comma after a clause-final particle (class B2)
$PY build_set.py                            # labelled streams -> audio/streams/, audio/labels.json (seed 20261108)
$PY run_eval.py clean                       # silero + sherpa + smart-turn scores -> out/eval_clean.pkl (~15-20 min)
$PY run_eval.py noisy15                     # same with pink noise 15 dB below speech
$PY analyze.py clean  > "$TURN_BENCH_DIR/out/analysis_clean.txt"
$PY analyze.py noisy15 > "$TURN_BENCH_DIR/out/analysis_noisy15.txt"
$PY auc.py clean; $PY auc.py noisy15        # Smart Turn separability
$PY gen_tables.py                           # AsciiDoc tables used in the record
$PY timing_bench.py                         # per-frame latency / RTF / Smart Turn cost (run on a quiet machine)
$PY footprint_all.py                        # load time and RSS, fresh process each
$PY validate_st.py                          # needs --with-vendor-shard; checks the Smart Turn pipeline on vendor Japanese rows
$PY probe2.py                               # abrupt-cut probe (supplementary)
```

`asr_bounds.py`, `prep_clips.py` and the other early steps read `clips.json` as written by the previous step; run them in the order above.
`footprint_all.py` launches `footprint_one.py` with the current interpreter.
Results vary with CPU, clip sampling is deterministic given the pinned mirror revision, and the numbers in the record were produced on the single machine class described there.

## Pinned inputs (also encoded in `fetch_assets.py`)

| Input | Version / revision | sha256 |
|---|---|---|
| Common Voice 22.0 `ja` test split, HF mirror `fsicoli/common_voice_22_0` (third-party mirror) | revision `ae911de250fe9375d08e2daa2105b671d659d446` | `ja_test_0.tar` `40cd0b6d...`; `test.tsv` `11b2ee42...` |
| Silero VAD `silero_vad.onnx` | snakers4/silero-vad tag `v6.2.3` | `1a153a22f4509e29...` |
| sherpa-onnx release asset `silero_vad.onnx` (Silero v4 export) | k2-fsa/sherpa-onnx `asr-models` release; sherpa-onnx 2.0.1 | `9e2449e1087496d8...` |
| Smart Turn `smart-turn-v3.2-cpu.onnx` | HF `pipecat-ai/smart-turn-v3` revision `f766f81d3cfdf7737ac64aad813d91bbfd56bf93` | `2bb026316b14a660...` |
| Pipecat `_whisper_features.py` | Pipecat `main` as fetched 2026-11-08 (not a tag) | `139b047d10dee42a...` |
| ReazonSpeech k2 v2 int8 encoder/decoder/joiner/tokens | HF `reazon-research/reazonspeech-k2-v2` revision `291488c8151be24d7da4bf7af26e533fad96e407` | see `fetch_assets.py` |
| Optional: Smart Turn v3.2 test data, shard 0 | HF `pipecat-ai/smart-turn-data-v3.2-test`, `main` | see `fetch_assets.py` |

## License notes

| Item | License | Caveat |
|---|---|---|
| Silero VAD code and model | MIT | |
| sherpa-onnx | Apache-2.0 | covers the framework, not every model |
| Smart Turn code and weights | BSD-2-Clause (repository and model card) | upstream Whisper Tiny weights and the commercial-TTS training audio are **not** verified; see the record, section 3.2 |
| Smart Turn data | CC-BY-4.0 (attribution) | Japanese rows seen were synthetic TTS |
| ReazonSpeech k2 v2 (alignment aid only) | Apache-2.0 (model card) | training corpus license not verified |
| Common Voice 22.0 | CC0-1.0 | obtained from a third-party mirror |
| TEN VAD | Apache-2.0 plus Agora conditions | not permissive; never run here, deliberately absent from these scripts |

The scripts themselves are repository code under the repository's license. Pipecat's feature code is downloaded, not copied.
