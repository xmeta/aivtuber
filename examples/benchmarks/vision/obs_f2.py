"""EXP-0004 F2: does polling GetSourceScreenshot cost OBS dropped, skipped or lagged frames?

Standard library only (a minimal RFC 6455 client for obs-websocket 5). Run it only against an OBS instance you
own, with a recording or stream that YOU started and a live game capture; it never starts or stops an output.

    python -I -X utf8 obs_f2.py --probe                       # reads the Hello message only: version and whether a password is needed
    OBS_WS_PASSWORD=... python -I -X utf8 obs_f2.py --source "Game Capture" --interval 10 --minutes 10 --out run-10s.json
    ... --interval 0 --minutes 10 --out run-off.json          # interval 0 = polling off (baseline); run baseline, 10 s, 2 s, baseline again

The password is read from the environment, never from an argument or a file. Output: per-run GetStats and
GetStreamStatus/GetRecordStatus deltas (output skipped frames, render skipped frames, total frames, average
frame render time, CPU usage) plus screenshot request latency p50/p95. Compare the deltas per minute between
runs; a baseline pair (off, on, off) shows how much run-to-run noise the machine has.
"""
import argparse
import base64
import hashlib
import json
import os
import socket
import struct
import time
import uuid


class WS:
    def __init__(self, host="127.0.0.1", port=4455):
        self.s = socket.create_connection((host, port), timeout=15)
        key = base64.b64encode(os.urandom(16)).decode()
        self.s.sendall((f"GET / HTTP/1.1\r\nHost: {host}:{port}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n"
                        f"Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Protocol: obswebsocket.json\r\n\r\n").encode())
        buf = b""
        while b"\r\n\r\n" not in buf:
            buf += self.s.recv(4096)
        if b" 101 " not in buf.split(b"\r\n", 1)[0]:
            raise RuntimeError("websocket upgrade refused")
        self.buf = buf.split(b"\r\n\r\n", 1)[1]

    def _need(self, n):
        while len(self.buf) < n:
            c = self.s.recv(1 << 16)
            if not c:
                raise ConnectionError("closed")
            self.buf += c

    def recv(self):
        data = b""
        while True:
            self._need(2)
            b0, b1 = self.buf[0], self.buf[1]
            ln, off = b1 & 0x7F, 2
            if ln == 126:
                self._need(4)
                ln, off = struct.unpack(">H", self.buf[2:4])[0], 4
            elif ln == 127:
                self._need(10)
                ln, off = struct.unpack(">Q", self.buf[2:10])[0], 10
            self._need(off + ln)
            data += self.buf[off:off + ln]
            self.buf = self.buf[off + ln:]
            if b0 & 0x80:
                return json.loads(data)

    def send(self, obj):
        p = json.dumps(obj).encode()
        mask = os.urandom(4)
        hdr = bytes([0x81])
        if len(p) < 126:
            hdr += bytes([0x80 | len(p)])
        elif len(p) < 65536:
            hdr += bytes([0x80 | 126]) + struct.pack(">H", len(p))
        else:
            hdr += bytes([0x80 | 127]) + struct.pack(">Q", len(p))
        self.s.sendall(hdr + mask + bytes(b ^ mask[i % 4] for i, b in enumerate(p)))


def hello(ws):
    h = ws.recv()
    assert h["op"] == 0
    return h["d"]


def identify(ws, h, password):
    d = {"rpcVersion": 1}
    a = h.get("authentication")
    if a:
        secret = base64.b64encode(hashlib.sha256((password + a["salt"]).encode()).digest()).decode()
        d["authentication"] = base64.b64encode(hashlib.sha256((secret + a["challenge"]).encode()).digest()).decode()
    ws.send({"op": 1, "d": d})
    r = ws.recv()
    assert r["op"] == 2, r


def request(ws, kind, data=None):
    rid = str(uuid.uuid4())
    ws.send({"op": 6, "d": {"requestType": kind, "requestId": rid, **({"requestData": data} if data else {})}})
    while True:
        m = ws.recv()
        if m["op"] == 7 and m["d"]["requestId"] == rid:
            if not m["d"]["requestStatus"]["result"]:
                raise RuntimeError(f"{kind}: {m['d']['requestStatus']}")
            return m["d"].get("responseData", {})


def pct(xs, p):
    xs = sorted(xs)
    return xs[min(len(xs) - 1, int(round((len(xs) - 1) * p / 100)))] if xs else None


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--probe", action="store_true")
    ap.add_argument("--source")
    ap.add_argument("--interval", type=float, default=10.0)
    ap.add_argument("--minutes", type=float, default=10.0)
    ap.add_argument("--width", type=int, default=768)
    ap.add_argument("--out")
    a = ap.parse_args()
    ws = WS()
    h = hello(ws)
    if a.probe:
        print(json.dumps({"obs": h["obsStudioVersion"], "websocket": h["obsWebSocketVersion"], "password_required": "authentication" in h}))
        return
    identify(ws, h, os.environ.get("OBS_WS_PASSWORD", ""))
    s0 = request(ws, "GetStats")
    out0 = request(ws, "GetStreamStatus")
    rec0 = request(ws, "GetRecordStatus")
    t_end, lat, errors = time.monotonic() + a.minutes * 60, [], 0
    samples = []
    while time.monotonic() < t_end:
        t0 = time.monotonic()
        if a.interval > 0:
            try:
                tt = time.perf_counter()
                request(ws, "GetSourceScreenshot", {"sourceName": a.source, "imageFormat": "jpg", "imageWidth": a.width, "imageCompressionQuality": 70})
                lat.append(time.perf_counter() - tt)
            except Exception:
                errors += 1
        samples.append({"t": round(time.monotonic() - (t_end - a.minutes * 60), 1), **{k: v for k, v in request(ws, "GetStats").items() if k in ("cpuUsage", "activeFps", "averageFrameRenderTime", "renderSkippedFrames", "renderTotalFrames", "outputSkippedFrames", "outputTotalFrames")}})
        time.sleep(max(0.0, (a.interval if a.interval > 0 else 5.0) - (time.monotonic() - t0)))
    s1 = request(ws, "GetStats")
    out1 = request(ws, "GetStreamStatus")
    keys = ("renderSkippedFrames", "renderTotalFrames", "outputSkippedFrames", "outputTotalFrames")
    res = {"interval_s": a.interval, "minutes": a.minutes, "width": a.width, "requests": len(lat), "errors": errors,
           "delta": {k: s1[k] - s0[k] for k in keys},
           "stream_delta": {k: out1.get(k, 0) - out0.get(k, 0) for k in ("outputSkippedFrames", "outputTotalFrames")},
           "recording_active": rec0.get("outputActive"), "stream_active": out0.get("outputActive"),
           "screenshot_latency_s": {"p50": pct(lat, 50), "p95": pct(lat, 95)},
           "avg_frame_render_ms_max": max(s["averageFrameRenderTime"] for s in samples), "cpu_usage_max": max(s["cpuUsage"] for s in samples),
           "samples": samples}
    if a.out:
        open(a.out, "w", encoding="utf-8").write(json.dumps(res, indent=1))
    print(json.dumps({k: v for k, v in res.items() if k != "samples"}, indent=1))


if __name__ == "__main__":
    main()
