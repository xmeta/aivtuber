"""Download the pinned third-party assets into $TURN_BENCH_DIR/dl (never into the repository).

Run as:  TURN_BENCH_DIR=<dir> python -I -X utf8 fetch_assets.py
Files with a known sha256 are verified; a mismatch aborts. The Common Voice archive is large (about 221 MB).
Network hosts (GitHub release assets in particular) sometimes return HTTP 504: the downloader retries.
"""
import hashlib, os, shutil, sys, tarfile, time, urllib.request

TB = os.environ["TURN_BENCH_DIR"]
HF = "https://huggingface.co"
CV_REV = "ae911de250fe9375d08e2daa2105b671d659d446"  # fsicoli/common_voice_22_0 (third-party mirror, card: cc0-1.0)
ST_REV = "f766f81d3cfdf7737ac64aad813d91bbfd56bf93"  # pipecat-ai/smart-turn-v3
RZ_REV = "291488c8151be24d7da4bf7af26e533fad96e407"  # reazon-research/reazonspeech-k2-v2
ASSETS = [
    # (url, dest relative to dl/, sha256 or None)
    (f"{HF}/datasets/fsicoli/common_voice_22_0/resolve/{CV_REV}/audio/ja/test/ja_test_0.tar", "cv22ja/ja_test_0.tar",
     "40cd0b6d2a9d35cf9c866f93cac15db6facdff04fac7131f940a936640958f4c"),
    (f"{HF}/datasets/fsicoli/common_voice_22_0/resolve/{CV_REV}/transcript/ja/test.tsv", "cv22ja/test.tsv",
     "11b2ee424381fe08d9a9eabaca1e138866b009f06da4846dafac1bc79f26ce8f"),
    ("https://raw.githubusercontent.com/snakers4/silero-vad/v6.2.3/src/silero_vad/data/silero_vad.onnx",
     "models/vad/silero_vad_master.onnx", "1a153a22f4509e292a94e67d6f9b85e8deb25b4988682b7e174c65279d8788e3"),
    ("https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/silero_vad.onnx",
     "models/vad/sherpa_silero_vad.onnx", "9e2449e1087496d8d4caba907f23e0bd3f78d91fa552479bb9c23ac09cbb1fd6"),
    (f"{HF}/pipecat-ai/smart-turn-v3/resolve/{ST_REV}/smart-turn-v3.2-cpu.onnx",
     "models/smartturn/smart-turn-v3.2-cpu.onnx", "2bb026316b14a660486a75b1733cd3fbab8c2fd0314dc9af7be49f8cca967e4f"),
    ("https://raw.githubusercontent.com/pipecat-ai/pipecat/main/src/pipecat/audio/turn/smart_turn/_whisper_features.py",
     "ref/_whisper_features.py", "139b047d10dee42ad5b4894489012e515a882d8f5e2bffe8ad27b30728fc9968"),
    (f"{HF}/reazon-research/reazonspeech-k2-v2/resolve/{RZ_REV}/tokens.txt", "models/reazon-k2-v2/tokens.txt",
     "2c3ac659818a48a0c04010e0593bbc4d7c8a24a054340b01131499c05fd52def"),
    (f"{HF}/reazon-research/reazonspeech-k2-v2/resolve/{RZ_REV}/encoder-epoch-99-avg-1.int8.onnx",
     "models/reazon-k2-v2/encoder-epoch-99-avg-1.int8.onnx", "2c7bd08a8a99f9ddd0d9e458456577b1f6279214e51426f114f9eced44c54e1d"),
    (f"{HF}/reazon-research/reazonspeech-k2-v2/resolve/{RZ_REV}/decoder-epoch-99-avg-1.int8.onnx",
     "models/reazon-k2-v2/decoder-epoch-99-avg-1.int8.onnx", "8f0bff94d38797b03b762634ed03211a8e303d06cc4603cdd0cf4199d6eb1485"),
    (f"{HF}/reazon-research/reazonspeech-k2-v2/resolve/{RZ_REV}/joiner-epoch-99-avg-1.int8.onnx",
     "models/reazon-k2-v2/joiner-epoch-99-avg-1.int8.onnx", "49cc7ea1d3d35a40a27442db5e89996da64bf0e683a903dce76e99e57a12e4de"),
]
OPTIONAL = [  # only for validate_st.py (about 480 MB; dataset main branch, license cc-by-4.0)
    (f"{HF}/datasets/pipecat-ai/smart-turn-data-v3.2-test/resolve/main/data/train-00000-of-00010.parquet",
     "st_test/shard0.parquet", "a87c75806b814ee7379998b6f9dc65a6433c01bfec2875e62c5d1ccd2b37257a"),
]


def sha(p):
    h = hashlib.sha256()
    with open(p, "rb") as f:
        for b in iter(lambda: f.read(1 << 20), b""):
            h.update(b)
    return h.hexdigest()


def get(url, rel, want):
    dest = os.path.join(TB, "dl", rel)
    os.makedirs(os.path.dirname(dest), exist_ok=True)
    if os.path.exists(dest) and (want is None or sha(dest) == want):
        print("have", rel)
        return
    for i in range(8):
        try:
            with urllib.request.urlopen(urllib.request.Request(url, headers={"User-Agent": "turn-bench"}), timeout=120) as r, open(dest, "wb") as f:
                shutil.copyfileobj(r, f)
            break
        except Exception as e:  # noqa: BLE001
            print("retry", i, e)
            time.sleep(5)
    got = sha(dest)
    if want and got != want:
        sys.exit(f"sha256 mismatch for {rel}: {got} != {want}")
    print("ok", rel, got)


if __name__ == "__main__":
    for a in ASSETS + (OPTIONAL if "--with-vendor-shard" in sys.argv else []):
        get(*a)
    x = os.path.join(TB, "dl", "cv22ja", "x")
    if not os.path.isdir(x):
        os.makedirs(x)
        with tarfile.open(os.path.join(TB, "dl", "cv22ja", "ja_test_0.tar")) as t:
            t.extractall(x, filter="data")
    print("done")
