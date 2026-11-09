# Japanese TTS bench (EXP-0003, #220 S2a)

Throwaway measurement scripts behind
[`docs/experiments/EXP-0003-japanese-tts-bench.adoc`](../../../docs/experiments/EXP-0003-japanese-tts-bench.adoc).
They are not production code, are not imported by any crate or TypeScript package, and **CI does not run them**.
Read the record's limitations before using any number they produce.

Nothing large lives in the repository: no audio, models, binaries, virtual environments or per-run results.
The scripts contain no machine paths; two environment variables name directories outside the repository:

| Variable | Meaning |
|---|---|
| `TTS_BENCH_DIR` | a **new, empty** directory; `fetch_assets.py` creates `dl/` in it (third-party downloads, untrusted data, every file sha256-checked) |
| `TTS_OUT_DIR` | per-run outputs (wavs, `result.json`, `stt.json`, `eval.json`); defaults to `$TTS_BENCH_DIR/out` |

Keep both away from the scripts directory and run Python with `-I` (isolated mode) and `-X utf8`.
Disk: about 3 GB for `dl/` plus about 2.2 GB for the extracted VOICEVOX engine.

## Environments (two virtual environments; CPU only)

```sh
# A: sherpa-onnx engines and the STT used for the round trip
python -m venv venvA && venvA/bin/pip install -r requirements-sherpa.txt         # Windows: venvA\Scripts\...
# B: kokoro-onnx + misaki.ja (Open JTalk through pyopenjtalk)
python -m venv venvB && venvB/bin/pip install --no-deps -r requirements-kokoro.txt   # --no-deps: misaki[ja] would pull mojimoji
```

`requirements-kokoro.txt` names `pyopenjtalk-plus`, because `pyopenjtalk` itself has no Windows wheel for Python 3.13 and
`misaki[ja]` needs `mojimoji`, which has no Windows wheel either (it needs a C compiler). `shims/mojimoji.py` is a pure-Python
stand-in for the two calls misaki 0.7.4 makes; see the record for this deviation. Where a C compiler exists, install the real packages.

## Reproduction

```sh
export TTS_BENCH_DIR=/path/outside/the/repo/tts-bench-dl      # PowerShell: $env:TTS_BENCH_DIR = "..."
export TTS_OUT_DIR=/path/outside/the/repo/tts-bench-out
PY="python -I -X utf8"

$PY fetch_assets.py [--only substring ...]     # pinned downloads, sha256-verified (about 2.6 GB; VOICEVOX engine 1.9 GB as a 7z)
7z x -o"$TTS_BENCH_DIR/dl/voicevox/engine" "$TTS_BENCH_DIR/dl/voicevox/voicevox_engine-windows-cpu-0.26.1.7z.001"

# one engine, one fresh process; run it twice (run1, run2) to compare hashes across processes
$PY bench.py --engine sherpa-supertonic3 --tag run1       # use the interpreter of the environment named below
$PY bench.py --engine sherpa-supertonic3 --tag run2
$PY eval_stt.py  sherpa-supertonic3 run1                  # environment A: transcribe every wav with ReazonSpeech k2-v2 int8
$PY eval_reading.py sherpa-supertonic3 run1               # environment B: CER (text and Open JTalk reading), handling-set misreadings
$PY summarize.py > summary.txt                            # tables
```

Engines (`bench.py --engine`): `sherpa-supertonic3[:sid]`, `sherpa-supertonic3-defaultlang`, `sherpa-supertonic3-noseed` (environment A);
`sherpa-kokoro-int8`, `sherpa-kokoro-fp32` (A); `kokoro-onnx-int8`, `kokoro-onnx-fp32` (B); `voicevox[:speaker]` (A; starts the engine
process on 127.0.0.1:50021 and stops it).

## What is measured

* `corpus.py`: 50 short VTuber-style replies and a 38-item text-handling set (numbers, English words, kanji readings, URLs,
  symbols, emoji). Written for this bench; the expected readings are one annotator's.
* `bench.py`: load time, first call, peak resident set (process tree), per-reply synthesis time, audio duration, sha256 of the
  16-bit PCM, a second pass in the same process (determinism).
* `eval_stt.py` + `eval_reading.py`: round trip through the repository's chosen STT (sherpa-onnx ReazonSpeech k2-v2 int8, the
  engine merged in #272). **This is an intelligibility proxy, not naturalness**, and STT errors count against the TTS.
* Normalisation is the one of EXP-0002 (NFKC, lower-case, drop P/S/Z/C categories); the second CER first converts both sides to
  katakana readings with Open JTalk so that 25 / 二十五 and kanji/kana spelling do not count.

Timings are wall-clock on one machine and vary with background load; run on a quiet machine, never two runs at once.
The scripts are repository code under the repository's license.
