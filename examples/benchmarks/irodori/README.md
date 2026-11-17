# Irodori-TTS bench (EXP-0006, #409)

Throwaway measurement scripts behind
[`docs/experiments/EXP-0006-irodori-tts-small-voices.adoc`](../../../docs/experiments/EXP-0006-irodori-tts-small-voices.adoc).
They are not production code, are imported by no crate or TypeScript package, and **CI does not run them**.

Nothing large lives in the repository: no audio, models, virtual environments or per-run results.
The script contains no machine paths; two environment variables name directories outside the repository,
the same convention as the EXP-0003 bench:

| Variable | Meaning |
|---|---|
| `TTS_BENCH_DIR` | a **new, empty** directory outside the repository; models go in `models/`, the STT model in `dl/`, per-run output in `out/` |
| `TTS_OUT_DIR` | per-run outputs (wavs, `result.json`, `stt.json`, `eval.json`); defaults to `$TTS_BENCH_DIR/out` |

The 50 replies and the 38-item text-handling set are **reused unchanged** from
[`../tts/corpus.py`](../tts/corpus.py), and so are the evaluators and the summariser of the EXP-0003 bench
(`../tts/eval_stt.py`, `../tts/eval_reading.py`, `../tts/summarize.py`): `bench.py` writes `result.json`
in the same shape. Run Python with `-I -X utf8`.

## Environment (one virtual environment outside the repository)

The official inference code is a GitHub repository, not a PyPI package:

```sh
git clone https://github.com/Aratako/Irodori-TTS.git irodori-tts
cd irodori-tts && git checkout 382205679ebc32295b7d1bfb329327d04c8f4eed   # the COMMIT this record read
patch -p1 < /path/to/Irodori-TTS-v4.1-Small-SUN/code_patches/cpu_bf16_ode_fp32.patch   # required by both checkpoints

python -m venv venv
venv/bin/pip install "transformers>=5.12.1,<6" soundfile sentencepiece safetensors torchaudio \
    "dacvae @ git+https://github.com/facebookresearch/dacvae@414c20785fc3a28373073ea8ef7a1316eeeaca6e" \
    sherpa-onnx pyopenjtalk-plus rapidfuzz psutil    # plus the torch build for the platform
```

Notes from this run (Windows, Python 3.13): `sentencepiece` has no wheel and is not needed by the checkpoint's
tokenizer; `silentcipher` (the optional watermarker) was not installed, so no watermark was applied, as in the
model card's own benchmark. The codec is downloaded from the Hub at load time
(`Aratako/Semantic-DACVAE-Japanese-32dim`, 410 MB) and the tokenizer comes from
`sbintuitions/modernbert-ja-310m`; the text encoder itself is inside the checkpoint.

## Reproduction

```sh
export TTS_BENCH_DIR=/path/outside/the/repo/exp-0006      # PowerShell: $env:TTS_BENCH_DIR = "..."
PY="python -I -X utf8"
CR=/path/to/irodori-tts                                   # patched official code

# one checkpoint, one fresh process; --tag run2 repeats the same replies in a new process
$PY bench.py --engine sun-gpu-fp32 --model SUN --device cuda --model-dtype fp32 --codec-dtype fp32 \
    --tag run1 --code-root "$CR"
$PY ../tts/eval_stt.py     sun-gpu-fp32 run1              # ReazonSpeech k2-v2 int8 round trip
$PY ../tts/eval_reading.py sun-gpu-fp32 run1              # text and Open JTalk reading CER, handling items
$PY ../tts/summarize.py > summary.txt
```

Engines used in the record: `sun-cpu-bf16`, `luna-cpu-bf16` (3 replies, no handling set: CPU is far slower
than real time), and `sun-gpu-fp32`, `luna-gpu-fp32`, `sun-gpu-bf16`, `luna-gpu-bf16` (full 50 replies and
38 handling items). `--model` selects `$TTS_BENCH_DIR/models/Irodori-TTS-v4.1-Small-<MODEL>`, whose
`speed_patch.py` is imported and applied by the harness; `--no-fuse` runs the official code path instead.

## What is measured

* Load time (`InferenceRuntime.from_key` + `speed_patch.apply`), first call, three warm-ups, then the replies.
  CPU rows set `torch.set_num_threads(4)` (the model card's and EXP-0003's convention); GPU rows leave it at 4 too.
* Wall time of one `synthesize()` call per reply, audio duration, 48 kHz sample rate, RTF as the sum of
  call times over the sum of audio durations. One call produces all of the audio: no streaming.
* Peak resident set of the process tree and peak CUDA allocation, sampled every 25 ms.
* Determinism: sha256 of the 16-bit PCM, a second pass in the same process, and a fresh process (run1 vs run2).
* The round-trip proxy and the reading metric are the EXP-0003 ones; they measure what the repository's
  STT can recover, not naturalness.
