"""Download the pinned third-party assets into $TTS_BENCH_DIR/dl (never into the repository).

Run as:  TTS_BENCH_DIR=<new empty dir> python -I -X utf8 fetch_assets.py [--only substring ...]
Every file has a pinned sha256; a mismatch aborts and the partial file is deleted.
Archives are extracted into a fresh directory next to the archive (tar members use the "data" filter,
zip members are checked against path traversal). The VOICEVOX engine (about 1.9 GB) is a 7z archive:
it is only downloaded here; extract it with a local 7-Zip (see README.md).
"""
import hashlib, os, shutil, sys, tarfile, time, urllib.request, zipfile

WORK = os.environ["TTS_BENCH_DIR"]
HF = "https://huggingface.co"
RZ_REV = "291488c8151be24d7da4bf7af26e533fad96e407"  # reazon-research/reazonspeech-k2-v2 (apache-2.0), the STT used for round-trip CER
SHERPA_TTS = "https://github.com/k2-fsa/sherpa-onnx/releases/download/tts-models"

ASSETS = [  # (url, dest relative to dl/, sha256)
    (f"{HF}/reazon-research/reazonspeech-k2-v2/resolve/{RZ_REV}/tokens.txt", "k2v2/tokens.txt",
     "2c3ac659818a48a0c04010e0593bbc4d7c8a24a054340b01131499c05fd52def"),
    (f"{HF}/reazon-research/reazonspeech-k2-v2/resolve/{RZ_REV}/encoder-epoch-99-avg-1.int8.onnx", "k2v2/encoder-epoch-99-avg-1.int8.onnx",
     "2c7bd08a8a99f9ddd0d9e458456577b1f6279214e51426f114f9eced44c54e1d"),
    (f"{HF}/reazon-research/reazonspeech-k2-v2/resolve/{RZ_REV}/decoder-epoch-99-avg-1.int8.onnx", "k2v2/decoder-epoch-99-avg-1.int8.onnx",
     "8f0bff94d38797b03b762634ed03211a8e303d06cc4603cdd0cf4199d6eb1485"),
    (f"{HF}/reazon-research/reazonspeech-k2-v2/resolve/{RZ_REV}/joiner-epoch-99-avg-1.int8.onnx", "k2v2/joiner-epoch-99-avg-1.int8.onnx",
     "49cc7ea1d3d35a40a27442db5e89996da64bf0e683a903dce76e99e57a12e4de"),
    # sherpa-onnx tts-models release assets (the tag is mutable, the hash is the pin)
    (f"{SHERPA_TTS}/sherpa-onnx-supertonic-3-tts-int8-2026-05-11.tar.bz2", "sherpa/supertonic3-int8.tar.bz2",
     "82fa96f91c4ef8abaae3a14a3f4153facf88bed821d1f7331cec2700f432c427"),
    (f"{SHERPA_TTS}/kokoro-int8-multi-lang-v1_0.tar.bz2", "sherpa/kokoro-int8-multi-lang-v1_0.tar.bz2",
     "72c347230107ef8a31098ec4d468a605244beef5f1ed00db97efa13518800653"),
    (f"{SHERPA_TTS}/kokoro-multi-lang-v1_0.tar.bz2", "sherpa/kokoro-multi-lang-v1_0.tar.bz2",
     "60703707bc05ccd48d60e1eb723f38b07d6e474795a0b6a588d7220400e0f7a4"),
    # Kokoro-82M v1.0 ONNX exports used by kokoro-onnx (thewh1teagle/kokoro-onnx release model-files-v1.1; asset digests as published by GitHub)
    ("https://github.com/thewh1teagle/kokoro-onnx/releases/download/model-files-v1.1/kokoro-v1.0.onnx", "kokoro-onnx/kokoro-v1.0.onnx",
     "beb0d1848dee9a49da392cc3df26958d46cfa35d321edf434f52949153f0df3a"),
    ("https://github.com/thewh1teagle/kokoro-onnx/releases/download/model-files-v1.1/kokoro-v1.0.int8.onnx", "kokoro-onnx/kokoro-v1.0.int8.onnx",
     "ae315a79b623f244700e4afb9246c46a26066782e049ba174bf3ba433970ee9c"),
    ("https://github.com/thewh1teagle/kokoro-onnx/releases/download/model-files-v1.1/voices-v1.0.bin", "kokoro-onnx/voices-v1.0.bin",
     "bca610b8308e8d99f32e6fe4197e7ec01679264efed0cac9140fe9c29f1fbf7d"),
    # VOICEVOX engine 0.26.1, Windows CPU build, run as a separate local HTTP process (not linked)
    ("https://github.com/VOICEVOX/voicevox_engine/releases/download/0.26.1/voicevox_engine-windows-cpu-0.26.1.7z.001",
     "voicevox/voicevox_engine-windows-cpu-0.26.1.7z.001",
     "2143ab794bf7241b1f88704f88bf4fb10ea6cf453035f54e1b3a73aea2c668fc"),
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
    if os.path.exists(dest) and sha(dest) == want:
        print("have", rel, want, flush=True)
        return
    for i in range(8):
        try:
            req = urllib.request.Request(url, headers={"User-Agent": "tts-bench"})
            with urllib.request.urlopen(req, timeout=120) as r, open(dest + ".part", "wb") as f:
                shutil.copyfileobj(r, f, 1 << 22)
            os.replace(dest + ".part", dest)
            break
        except Exception as e:  # noqa: BLE001
            print("retry", i, e, flush=True)
            time.sleep(5)
    got = sha(dest)
    if got != want:
        os.remove(dest)
        sys.exit(f"sha256 mismatch for {rel}: {got} != {want}")
    print("ok", rel, got, flush=True)


def extract(rel):
    src = os.path.join(WORK, "dl", rel)
    out = src[: -len(".tar.bz2")] if rel.endswith(".tar.bz2") else src[: -len(".zip")]
    if os.path.isdir(out):
        return
    os.makedirs(out)
    if rel.endswith(".zip"):
        with zipfile.ZipFile(src) as z:
            for m in z.namelist():
                if m.startswith(("/", "\\")) or ".." in m.replace("\\", "/").split("/"):
                    sys.exit(f"unsafe zip member {m}")
            z.extractall(out)
    else:
        with tarfile.open(src) as t:
            t.extractall(out, filter="data")


if __name__ == "__main__":
    only = sys.argv[sys.argv.index("--only") + 1:] if "--only" in sys.argv else None
    for url, rel, want in ASSETS:
        if only and not any(o in rel for o in only):
            continue
        get(url, rel, want)
        if rel.endswith((".tar.bz2", ".zip")):
            extract(rel)
    print("done")
