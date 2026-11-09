"""Download the pinned third-party assets into $STT_BENCH_DIR/dl (never into the repository).

Run as:  STT_BENCH_DIR=<dir> python -I -X utf8 fetch_assets.py [--with-gpu] [--only substring ...]
Every file has a pinned sha256; a mismatch aborts. Total download is roughly 7 GB.
Hosts sometimes answer HTTP 5xx: the downloader retries.
Archives are extracted into a fresh directory next to the archive (zip members are checked
against path traversal, tar members use the "data" extraction filter).
"""
import hashlib, os, shutil, sys, tarfile, time, urllib.request, zipfile

WORK = os.environ["STT_BENCH_DIR"]
HF = "https://huggingface.co"
CV_REV = "ae911de250fe9375d08e2daa2105b671d659d446"   # fsicoli/common_voice_22_0 (third-party mirror, card: cc0-1.0)
WCPP_REV = "5359861c739e955e79d9a303bcbc70fb988958b1"  # ggerganov/whisper.cpp (model files, card: mit)
KOTOBA_REV = "e3a0cf6a62b95911703cfb97d819292e058f12c3"  # kotoba-tech/kotoba-whisper-v2.0-ggml (apache-2.0)
RZ_REV = "291488c8151be24d7da4bf7af26e533fad96e407"   # reazon-research/reazonspeech-k2-v2 (apache-2.0)
SHERPA_RELEASE = "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models"

ASSETS = [  # (url, dest relative to dl/, sha256)
    (f"{HF}/datasets/fsicoli/common_voice_22_0/resolve/{CV_REV}/audio/ja/test/ja_test_0.tar", "cv22ja/ja_test_0.tar",
     "40cd0b6d2a9d35cf9c866f93cac15db6facdff04fac7131f940a936640958f4c"),
    (f"{HF}/datasets/fsicoli/common_voice_22_0/resolve/{CV_REV}/transcript/ja/test.tsv", "cv22ja/test.tsv",
     "11b2ee424381fe08d9a9eabaca1e138866b009f06da4846dafac1bc79f26ce8f"),
    # whisper.cpp prebuilt CPU binaries: release v1.9.5 has no assets, so the nightly b5454 published
    # the same day is used (it contains v1.9.5 plus later commits; see the record).
    ("https://github.com/ggml-org/whisper.cpp/releases/download/b5454/whisper-bin-x64.zip", "whispercpp/whisper-bin-x64.zip",
     "6ba69e3482d7826214f90a6a9c84ca07782aec1e1d0c6a7c30c994fd5d816ccb"),
    (f"{HF}/ggerganov/whisper.cpp/resolve/{WCPP_REV}/ggml-small.bin", "whispercpp/ggml-small.bin",
     "1be3a9b2063867b937e64e2ec7483364a79917e157fa98c5d94b5c1fffea987b"),
    (f"{HF}/ggerganov/whisper.cpp/resolve/{WCPP_REV}/ggml-medium-q5_0.bin", "whispercpp/ggml-medium-q5_0.bin",
     "19fea4b380c3a618ec4723c3eef2eb785ffba0d0538cf43f8f235e7b3b34220f"),
    (f"{HF}/ggerganov/whisper.cpp/resolve/{WCPP_REV}/ggml-large-v3-turbo-q5_0.bin", "whispercpp/ggml-large-v3-turbo-q5_0.bin",
     "394221709cd5ad1f40c46e6031ca61bce88931e6e088c188294c6d5a55ffa7e2"),
    (f"{HF}/ggerganov/whisper.cpp/resolve/{WCPP_REV}/ggml-large-v3-turbo.bin", "whispercpp/ggml-large-v3-turbo.bin",
     "1fc70f774d38eb169993ac391eea357ef47c88757ef72ee5943879b7e8e2bc69"),
    (f"{HF}/kotoba-tech/kotoba-whisper-v2.0-ggml/resolve/{KOTOBA_REV}/ggml-kotoba-whisper-v2.0.bin", "whispercpp/ggml-kotoba-whisper-v2.0.bin",
     "eff70a8a236e731abba774ba71e1f6d0fce53302137208c32207e694e0bf4546"),
    (f"{HF}/kotoba-tech/kotoba-whisper-v2.0-ggml/resolve/{KOTOBA_REV}/ggml-kotoba-whisper-v2.0-q5_0.bin", "whispercpp/ggml-kotoba-whisper-v2.0-q5_0.bin",
     "4a3b92192b5d3578ff854a5876213e2e27af0c2d357492c2d14271e82c303658"),
    (f"{HF}/reazon-research/reazonspeech-k2-v2/resolve/{RZ_REV}/tokens.txt", "k2v2/tokens.txt",
     "2c3ac659818a48a0c04010e0593bbc4d7c8a24a054340b01131499c05fd52def"),
    (f"{HF}/reazon-research/reazonspeech-k2-v2/resolve/{RZ_REV}/encoder-epoch-99-avg-1.int8.onnx", "k2v2/encoder-epoch-99-avg-1.int8.onnx",
     "2c7bd08a8a99f9ddd0d9e458456577b1f6279214e51426f114f9eced44c54e1d"),
    (f"{HF}/reazon-research/reazonspeech-k2-v2/resolve/{RZ_REV}/decoder-epoch-99-avg-1.int8.onnx", "k2v2/decoder-epoch-99-avg-1.int8.onnx",
     "8f0bff94d38797b03b762634ed03211a8e303d06cc4603cdd0cf4199d6eb1485"),
    (f"{HF}/reazon-research/reazonspeech-k2-v2/resolve/{RZ_REV}/joiner-epoch-99-avg-1.int8.onnx", "k2v2/joiner-epoch-99-avg-1.int8.onnx",
     "49cc7ea1d3d35a40a27442db5e89996da64bf0e683a903dce76e99e57a12e4de"),
    (f"{HF}/reazon-research/reazonspeech-k2-v2/resolve/{RZ_REV}/encoder-epoch-99-avg-1.onnx", "k2v2/encoder-epoch-99-avg-1.onnx",
     "ecdb0b771e16104aaf8e579cb3c1e32fbd589eb641c5946d82b615bd366c5f96"),
    (f"{HF}/reazon-research/reazonspeech-k2-v2/resolve/{RZ_REV}/decoder-epoch-99-avg-1.onnx", "k2v2/decoder-epoch-99-avg-1.onnx",
     "58b18211ae06265466bfa17172dab574df94f76c8bcb61a3640c28ba860e4124"),
    (f"{HF}/reazon-research/reazonspeech-k2-v2/resolve/{RZ_REV}/joiner-epoch-99-avg-1.onnx", "k2v2/joiner-epoch-99-avg-1.onnx",
     "d38a81d1191c9ed6de6a1719503692e07e3e973e2364adde0abae5eaaded1174"),
    (f"{SHERPA_RELEASE}/sherpa-onnx-nemo-parakeet-tdt_ctc-0.6b-ja-35000-int8.tar.bz2", "sherpa/parakeet-ja.tar.bz2",
     "4b0a800ef29f4f4c8667339bf6f60d5bfdc2852ddc9dc5741aea65b6f8d1306b"),
    # NOTE: the "...-int8-2025-09-09" archive of the same name family is a Cantonese fine-tune (its README points at
    # ASLP-lab/WSYue-ASR) and transcribes Japanese as Chinese; it is deliberately not used.
    (f"{SHERPA_RELEASE}/sherpa-onnx-sense-voice-zh-en-ja-ko-yue-int8-2024-07-17.tar.bz2", "sherpa/sensevoice-2024.tar.bz2",
     "7d1efa2138a65b0b488df37f8b89e3d91a60676e416f515b952358d83dfd347e"),
    (f"{SHERPA_RELEASE}/sherpa-onnx-zipformer-ja-en-reazonspeech-2025-01-17.tar.bz2", "sherpa/zipformer-ja-en.tar.bz2",
     "dc03758608c0280e2cbcaac4597467ffcf846ae0b06436f1706738a11da86f5d"),
    # Newer Japanese-capable sherpa-onnx models (the catalogue also lists Moonshine ja, which is under the non-commercial
    # Moonshine Community License and was not run, and Cohere / FunASR-Nano / Dolphin-base variants that were not run).
    (f"{SHERPA_RELEASE}/sherpa-onnx-nemotron-3.5-asr-streaming-0.6b-560ms-int8-2026-06-11.tar.bz2",
     "sherpa2/sherpa-onnx-nemotron-3.5-asr-streaming-0.6b-560ms-int8-2026-06-11.tar.bz2",
     "c6bf5e0df765f9d5b43bc9e0536d4b4b3e7d40bdf5ecf13e45f134c51c05ae3a"),
    (f"{SHERPA_RELEASE}/sherpa-onnx-nemotron-3.5-asr-streaming-0.6b-160ms-int8-2026-06-11.tar.bz2",
     "sherpa2/sherpa-onnx-nemotron-3.5-asr-streaming-0.6b-160ms-int8-2026-06-11.tar.bz2",
     "a81909a1780d84cff16d73c15e13e67d9d81d8839faf14870d507d8499f7a61a"),
    (f"{SHERPA_RELEASE}/sherpa-onnx-dolphin-small-ctc-multi-lang-int8-2025-04-02.tar.bz2",
     "sherpa2/sherpa-onnx-dolphin-small-ctc-multi-lang-int8-2025-04-02.tar.bz2",
     "7346b00ffe42396fff5c96d11892f0d4d72ac11418d47c5a8ada2b0f6ac8d27b"),
    (f"{SHERPA_RELEASE}/sherpa-onnx-omnilingual-asr-1600-languages-300M-ctc-v2-int8-2026-02-05.tar.bz2",
     "sherpa2/sherpa-onnx-omnilingual-asr-1600-languages-300M-ctc-v2-int8-2026-02-05.tar.bz2",
     "951b32409aade32bd525310bb39e9666773ba3fc611a39e817f620936d76c631"),
    (f"{SHERPA_RELEASE}/sherpa-onnx-qwen3-asr-0.6B-int8-2026-03-25.tar.bz2",
     "sherpa2/sherpa-onnx-qwen3-asr-0.6B-int8-2026-03-25.tar.bz2",
     "393f8a14e2f5fb96746aaab342997a40641001fbd5bf9592a080a8329178ee96"),
    ("https://alphacephei.com/vosk/models/vosk-model-small-ja-0.22.zip", "vosk/vosk-model-small-ja-0.22.zip",
     "efa092d280153a77615e9e0c7d7283e93e600de3d19d3bec686c57ef19d52eac"),
    ("https://alphacephei.com/vosk/models/vosk-model-ja-0.22.zip", "vosk/vosk-model-ja-0.22.zip",
     "af620db815ef716f90355f911092ba0c7454d91d793ecdd8dee2df6342e0e9cc"),
]

OPTIONAL_GPU = [  # only for the separately reported GPU run (about 0.7 GB); needs an NVIDIA driver that runs CUDA 12.4 binaries
    ("https://github.com/ggml-org/whisper.cpp/releases/download/b5454/whisper-bin-win-cuda-12.4.0-x64.zip",
     "whispercpp/whisper-bin-win-cuda-12.4.0-x64.zip", "afef0b881c500958921c3f5523b50e59ee2ec9b6f5cbd25b324c51ed308a957a"),
]


def sha(p):
    h = hashlib.sha256()
    with open(p, "rb") as f:
        for b in iter(lambda: f.read(1 << 22), b""):
            h.update(b)
    return h.hexdigest()


def get(url, rel, want):
    dest = os.path.join(WORK, "dl", rel)
    os.makedirs(os.path.dirname(dest), exist_ok=True)
    if os.path.exists(dest) and (want is None or sha(dest) == want):
        print("have", rel, sha(dest), flush=True)
        return
    for i in range(8):
        try:
            req = urllib.request.Request(url, headers={"User-Agent": "stt-bench"})
            with urllib.request.urlopen(req, timeout=120) as r, open(dest + ".part", "wb") as f:
                shutil.copyfileobj(r, f, 1 << 22)
            os.replace(dest + ".part", dest)
            break
        except Exception as e:  # noqa: BLE001
            print("retry", i, e, flush=True)
            time.sleep(5)
    got = sha(dest)
    if want and got != want:
        sys.exit(f"sha256 mismatch for {rel}: {got} != {want}")
    print("ok", rel, got, "(UNPINNED)" if want is None else "", flush=True)


def extract(rel, sub):
    src = os.path.join(WORK, "dl", rel)
    out = os.path.join(WORK, "dl", sub)
    if os.path.isdir(out):
        return
    os.makedirs(out)
    if rel.endswith(".zip"):
        with zipfile.ZipFile(src) as z:
            for m in z.namelist():  # reject absolute / traversing members
                if m.startswith(("/", "\\")) or ".." in m.replace("\\", "/").split("/"):
                    sys.exit(f"unsafe zip member {m}")
            z.extractall(out)
    else:
        with tarfile.open(src) as t:
            t.extractall(out, filter="data")


if __name__ == "__main__":
    only = sys.argv[sys.argv.index("--only") + 1:] if "--only" in sys.argv else None
    for url, rel, want in ASSETS + (OPTIONAL_GPU if "--with-gpu" in sys.argv else []):
        if only and not any(o in rel for o in only):
            continue
        get(url, rel, want)
    for rel, sub in [("cv22ja/ja_test_0.tar", "cv22ja/x"), ("whispercpp/whisper-bin-x64.zip", "whispercpp/bin"),
                     ("sherpa/parakeet-ja.tar.bz2", "sherpa/parakeet-ja"), ("sherpa/sensevoice-2024.tar.bz2", "sherpa/sensevoice-2024"),
                     ("sherpa/zipformer-ja-en.tar.bz2", "sherpa/zipformer-ja-en"),
                     ("whispercpp/whisper-bin-win-cuda-12.4.0-x64.zip", "whispercpp/bin-cuda"),
                     ("vosk/vosk-model-small-ja-0.22.zip", "vosk/small"), ("vosk/vosk-model-ja-0.22.zip", "vosk/big")]:
        if os.path.exists(os.path.join(WORK, "dl", rel)):
            extract(rel, sub)
    for url, rel, want in ASSETS:  # sherpa2/: one extraction directory per archive, dl/sherpa2/x/<archive name>
        if rel.startswith("sherpa2/") and os.path.exists(os.path.join(WORK, "dl", rel)):
            extract(rel, "sherpa2/x/" + os.path.basename(rel)[: -len(".tar.bz2")])
    print("done")
