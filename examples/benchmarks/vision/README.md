# Screen-vision VLM bench (EXP-0004, #90)

Throwaway measurement scripts behind
[`docs/experiments/EXP-0004-screen-vision-vlm-bench.adoc`](../../../docs/experiments/EXP-0004-screen-vision-vlm-bench.adoc).
They are not production code, are not imported by any crate or TypeScript package, and **CI does not run them**.
Read the record's limitations before using any number they produce.

Nothing large lives in the repository: no models, binaries, images or per-run results. The test frames are drawn
by `frames.py` from primitives and text (nothing third-party), so they are regenerated rather than committed.
The scripts contain no machine paths; every path is a command-line argument.

| File | Purpose |
|---|---|
| `frames.py` | Pillow-rendered 768x432 frames: 30 benign (6 each of menu, loading, cutscene, gameplay, result) and 8 prompt-injection frames (F3) with invented secret-like strings. Needs a Japanese-capable font for the Japanese strings: set `VISION_BENCH_FONT`. |
| `vision_bench.py` | Starts loopback-only `llama-server` children and measures: `vlm` (F1, VLM alone), `think` (Thinking first-token baseline), `both` (Thinking and VLM resident together, with the VLM job idle, every 10 s, every 5 s and back-to-back), `f3` (injection frames, raw answers kept), `reply` (EXP-0004b: the 30 Thinking requests to one server, replies and first-token latency), `mux` (EXP-0004b: one multimodal server serving Thinking and captions, `--parallel N`), `captions` (EXP-0004c: only a caption loop of `--interval` s for `--seconds`, as background load). With `--attach`, `mux` and `captions` use the llama-server already listening on `--port` (EXP-0004c: the full local profile) and never stop it. |
| `f3_replay.py` | Turns the F3 raw answers into a perception replay fixture (the format of `examples/perception-observations/replay`) so the Rust `PerceptionTracker` can be run over real model output. |
| `reply_prompts.py` | EXP-0004b: 30 maintainer-written Japanese Thinking requests shaped like the local profile's (instruction plus compact ThinkingRequest JSON). |
| `pair_view.py` | EXP-0004b: blind A/B presentation of two models' replies with a seed-shuffled key file, tally, and mechanical reply statistics. |
| `obs_f2.py` | F2 procedure: polls `GetSourceScreenshot` from obs-websocket 5 at a fixed interval and records OBS `GetStats` deltas. Needs an OBS instance you own, its websocket password in `OBS_WS_PASSWORD`, and a recording or stream you started yourself. |

## Environment

Python 3.13 with `Pillow` and `jsonschema` (nothing else; `vision_bench.py` otherwise uses the standard library) and `nvidia-smi` on
`PATH` for the VRAM columns. Run Python with `-I -X utf8`. Keep the downloads away from the scripts directory.

Pinned artifacts (all downloaded from the upstream publisher, SHA-256 checked; the record lists them):

* llama.cpp `b12224` Vulkan Windows build (the repository's local-profile pin, `packaging/local-profile-pins.json`).
* `Qwen/Qwen3-VL-4B-Instruct-GGUF` `Qwen3VL-4B-Instruct-Q4_K_M.gguf` and `mmproj-Qwen3VL-4B-Instruct-Q8_0.gguf`.
* `ggml-org/SmolVLM-500M-Instruct-GGUF` `SmolVLM-500M-Instruct-Q8_0.gguf` and `mmproj-SmolVLM-500M-Instruct-Q8_0.gguf`.
* `unsloth/Qwen3-4B-Instruct-2507-GGUF` `Qwen3-4B-Instruct-2507-Q4_K_M.gguf` (the resident local-profile model).

## Reproduction

```sh
VISION_BENCH_FONT=NotoSansJP-VF.ttf python -I -X utf8 frames.py "$OUT/frames"
B="python -I -X utf8 vision_bench.py"
$B vlm   --server "$LLAMA/llama-server" --model "$QWEN" --mmproj "$QWEN_MMPROJ" --frames "$OUT/frames" --out "$OUT/qwen-en.json" --lang en
$B think --server "$LLAMA/llama-server" --model "$LLM" --out "$OUT/think.json"
$B both  --server "$LLAMA/llama-server" --llm "$LLM" --model "$QWEN" --mmproj "$QWEN_MMPROJ" --frames "$OUT/frames" --out "$OUT/both-qwen.json"
$B f3    --server "$LLAMA/llama-server" --model "$QWEN" --mmproj "$QWEN_MMPROJ" --frames "$OUT/frames" --out "$OUT/f3-qwen.json"
python -I -X utf8 f3_replay.py "$OUT/f3-qwen.json" "$OUT/f3-qwen-replay.json"
```

Replaying the fixture through the Rust tracker needs a temporary, uncommitted test that reads the file and calls
`PerceptionReplayFixture::from_json(..).run(..)` (see the record, section 6); it is not part of the repository.

Settings that matter: temperature 0, seed 1234, `max_tokens` 96, `cache_prompt` false (every call pays full prefill),
`response_format` json_schema (the grammar-constrained model-side schema in `vision_bench.py`), `--parallel 1`,
`-ngl 99`, context 4096, loopback `127.0.0.1` only. Latency is wall time of one `/v1/chat/completions` call including
the base64 PNG upload and image encoding; the sidecar-to-OBS capture latency is not included.

EXP-0004b (single resident multimodal model):

```sh
$B reply --server "$LLAMA/llama-server" --model "$LLM" --out "$OUT/reply-qwen3-4b.json"
$B reply --server "$LLAMA/llama-server" --model "$QWEN" --mmproj "$QWEN_MMPROJ" --out "$OUT/reply-qwen3vl-4b.json"
python -I -X utf8 pair_view.py show "$OUT/reply-qwen3-4b.json" "$OUT/reply-qwen3vl-4b.json" "$OUT/key.json"   # judge blind, write verdicts.json
python -I -X utf8 pair_view.py score "$OUT/key.json" "$OUT/verdicts.json"
$B mux --server "$LLAMA/llama-server" --model "$QWEN" --mmproj "$QWEN_MMPROJ" --frames "$OUT/frames" --out "$OUT/mux-p2.json" --n 30 --parallel 2 --ctx 8192
```
