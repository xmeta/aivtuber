// Headless end-to-end smoke test (no GPU): the REAL Rust LocalAvatarOutput
// server (crates/app/examples/local_avatar_smoke.rs) serves the built bundle
// plus the pinned sample model; Chromium under SwiftShader loads the launch
// URL. Assertions are state-based and tolerant: software GL renders at ~9 fps
// and spring bones move pixels, so nothing here is timing- or pixel-exact.
//
//   node smoke/smoke.mjs --bin <path to local_avatar_smoke> [--dist dist] [--out smoke/out]
//
// Env: SMOKE_BROWSER_CHANNEL=msedge|chrome  use a system browser instead of
//                                           Playwright's bundled Chromium
//      SMOKE_GPU=1                          do not force SwiftShader
import { spawn } from "node:child_process";
import { cpSync, copyFileSync, mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { createInterface } from "node:readline";
import { fileURLToPath } from "node:url";
import { inflateSync } from "node:zlib";
import { chromium } from "playwright-core";
import { MODEL, ensureModel } from "./fetch-model.mjs";

const here = dirname(fileURLToPath(import.meta.url));
const argv = process.argv.slice(2);
const arg = (name, fallback) => {
  const i = argv.indexOf(name);
  return i >= 0 ? argv[i + 1] : fallback;
};
const bin = arg("--bin");
if (!bin) throw new Error("--bin <local_avatar_smoke executable> is required");
const dist = resolve(arg("--dist", join(here, "..", "dist")));
const outDir = resolve(arg("--out", join(here, "out")));
mkdirSync(outDir, { recursive: true });

const failures = [];
const log = [];
const check = (name, ok, detail = "") => {
  log.push({ name, ok, detail });
  console.log(`${ok ? "ok  " : "FAIL"} ${name}${detail ? ` (${detail})` : ""}`);
  if (!ok) failures.push(name);
};
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
async function until(fn, what, timeoutMs = 30000) {
  const end = Date.now() + timeoutMs;
  for (;;) {
    const value = await fn();
    if (value) return value;
    if (Date.now() > end) throw new Error(`timed out waiting for ${what}`);
    await sleep(100);
  }
}

// ---- minimal PNG decoder (8-bit RGBA, non-interlaced: what Chromium emits) --
function decodePng(buf) {
  let pos = 8;
  let ihdr = null;
  const idat = [];
  while (pos < buf.length) {
    const len = buf.readUInt32BE(pos);
    const type = buf.toString("latin1", pos + 4, pos + 8);
    const body = buf.subarray(pos + 8, pos + 8 + len);
    if (type === "IHDR") ihdr = body;
    else if (type === "IDAT") idat.push(body);
    pos += 12 + len;
  }
  const width = ihdr.readUInt32BE(0);
  const height = ihdr.readUInt32BE(4);
  if (ihdr[8] !== 8 || ihdr[9] !== 6 || ihdr[12] !== 0) {
    throw new Error(`unsupported PNG (depth ${ihdr[8]}, color type ${ihdr[9]}, interlace ${ihdr[12]})`);
  }
  const raw = inflateSync(Buffer.concat(idat));
  const stride = width * 4;
  const px = Buffer.alloc(stride * height);
  for (let y = 0; y < height; y++) {
    const filter = raw[y * (stride + 1)];
    const line = raw.subarray(y * (stride + 1) + 1, (y + 1) * (stride + 1));
    for (let x = 0; x < stride; x++) {
      const a = x >= 4 ? px[y * stride + x - 4] : 0;
      const b = y > 0 ? px[(y - 1) * stride + x] : 0;
      const c = x >= 4 && y > 0 ? px[(y - 1) * stride + x - 4] : 0;
      let v = line[x];
      if (filter === 1) v += a;
      else if (filter === 2) v += b;
      else if (filter === 3) v += (a + b) >> 1;
      else if (filter === 4) {
        const p = a + b - c;
        const pa = Math.abs(p - a), pb = Math.abs(p - b), pc = Math.abs(p - c);
        v += pa <= pb && pa <= pc ? a : pb <= pc ? b : c;
      }
      px[y * stride + x] = v & 255;
    }
  }
  return { width, height, alphaAt: (x, y) => px[y * stride + x * 4 + 3] };
}

function analyze(pngBuf) {
  const { width, height, alphaAt } = decodePng(pngBuf);
  const corners = [alphaAt(0, 0), alphaAt(width - 1, 0), alphaAt(0, height - 1), alphaAt(width - 1, height - 1)];
  let middleOpaque = 0;
  for (let y = Math.floor(height * 0.3); y < Math.floor(height * 0.7); y++) {
    for (let x = Math.floor(width * 0.35); x < Math.floor(width * 0.65); x++) if (alphaAt(x, y) > 0) middleOpaque++;
  }
  return { width, height, corners, middleOpaque };
}

// ---- host process (the real Rust server) -------------------------------------
const pageDir = mkdtempSync(join(tmpdir(), "local-avatar-smoke-"));
cpSync(dist, pageDir, { recursive: true });
copyFileSync(await ensureModel(), join(pageDir, "model.vrm"));

const host = spawn(bin, ["--page-dir", pageDir], { stdio: ["pipe", "pipe", "inherit"] });
const replies = [];
const waiters = [];
createInterface({ input: host.stdout }).on("line", (line) => {
  const w = waiters.shift();
  if (w) w(JSON.parse(line));
  else replies.push(JSON.parse(line));
});
const nextReply = () => (replies.length ? Promise.resolve(replies.shift()) : new Promise((r) => waiters.push(r)));
const hostSend = async (line) => {
  host.stdin.write(`${line}\n`);
  return nextReply();
};
const health = async () => (await hostSend("health")).health;
const cmd = (kind, fields = {}) => JSON.stringify({ kind, generation: 1, at_ms: 0, ...fields });
const ackedAll = async () => {
  const h = await health();
  return h.sent > 0 && h.acked === h.sent ? h : null;
};

let browser;
let exitCode = 1;
try {
  const listening = await nextReply();
  const launchUrl = new URL(listening.url);
  check("host serves on loopback", launchUrl.hostname === "127.0.0.1");

  const channel = process.env.SMOKE_BROWSER_CHANNEL || undefined;
  const gpuArgs = process.env.SMOKE_GPU === "1" ? [] : [
    "--disable-gpu", "--use-gl=angle", "--use-angle=swiftshader", "--enable-unsafe-swiftshader", "--ignore-gpu-blocklist",
  ];
  browser = await chromium.launch({ channel, headless: true, args: gpuArgs });
  const page = await browser.newPage({ viewport: { width: 1280, height: 720 } });
  const problems = [];
  page.on("pageerror", (e) => problems.push(`pageerror: ${e.message}`));
  page.on("console", (m) => {
    const text = m.text();
    if (m.type() === "error" || /Content Security Policy|Refused to/i.test(text)) problems.push(`console ${m.type()}: ${text}`);
  });

  launchUrl.searchParams.set("debug", "1");
  await page.goto(launchUrl.toString());
  const snap = () => page.evaluate(() => window.__avatarDebug?.snapshot());

  // 1. handshake: hello -> first neutral frame -> ready; host answers with reset (seq 1)
  await until(async () => (await snap())?.ready, "renderer ready");
  const first = await snap();
  check("ready arrives", first.ready === true && typeof first.epoch === "string");
  check("epoch is a UUID", /^[0-9a-f-]{36}$/.test(first.epoch), first.epoch);
  check("model hash is the pinned SHA-256", first.modelHash === MODEL.sha256, first.modelHash);
  const h0 = await until(async () => {
    const h = await health();
    return h.state === "Ready" ? h : null;
  }, "host Ready");
  check("host reports Ready", h0.state === "Ready");
  const afterReset = await until(ackedAll, "initial reset acked");
  check("first command of the epoch (reset) acked", afterReset.sent === 1 && afterReset.acked === 1);

  // 2. transparent capture of the neutral avatar
  const neutralPng = await page.screenshot({ omitBackground: true });
  writeFileSync(join(outDir, "neutral.png"), neutralPng);
  const a = analyze(neutralPng);
  check("screenshot corners are fully transparent", a.corners.every((v) => v === 0), a.corners.join(","));
  check("avatar pixels present in the middle", a.middleOpaque > 2000, `${a.middleOpaque} non-transparent px`);

  // 3. scripted sequence; every command acked, renderer values follow
  const near = (v, want) => Math.abs(v - want) < 0.05;
  const settled = async (pred, what) => {
    await until(async () => pred(await snap()), what, 20000);
    check(what, true);
  };
  const run = async (name, line, pred) => {
    const reply = await hostSend(line);
    check(`${name}: accepted by host`, reply.event === "executed", JSON.stringify(reply));
    await until(ackedAll, `${name} acked`);
    check(`${name}: acked`, true);
    await settled(pred, `${name}: renderer values follow`);
  };
  const allZero = (s) =>
    Object.values(s.expressions).every((v) => near(v, 0)) &&
    Object.values(s.visemes).every((v) => near(v, 0)) &&
    near(s.gaze.yaw, 0) && near(s.gaze.pitch, 0);

  await run("reset", cmd("reset"), allZero);
  await run("expression joy", cmd("expression", { preset: "joy", intensity: 1 }), (s) => near(s.expressions.joy, 1));
  writeFileSync(join(outDir, "joy.png"), await page.screenshot({ omitBackground: true }));
  await run("gaze away", cmd("gaze", { target: "away" }), (s) => Math.abs(s.gaze.yaw - 35) < 1.5 && Math.abs(s.gaze.pitch - 10) < 1.5);
  await run("viseme aa", cmd("viseme", { viseme: "aa", weight: 0.8 }), (s) => near(s.visemes.aa, 0.8));
  await run("viseme ih replaces aa", cmd("viseme", { viseme: "ih", weight: 0.5 }), (s) => near(s.visemes.ih, 0.5) && near(s.visemes.aa, 0));
  await run("viseme neutral", cmd("viseme", { viseme: "neutral", weight: 0 }), (s) => Object.values(s.visemes).every((v) => near(v, 0)));
  await run("reset", cmd("reset"), allZero);
  const seqHealth = await health();
  check("no rejects, strays or protocol errors", Object.keys(seqHealth.rejected).length === 0 && seqHealth.stray_acks === 0 && seqHealth.protocol_errors === 0, JSON.stringify(seqHealth));

  // 4. restart: reload = new page = new epoch, neutral, nothing replayed
  await run("expression joy before reload", cmd("expression", { preset: "joy", intensity: 1 }), (s) => near(s.expressions.joy, 1));
  const before = await snap();
  const sentBefore = (await health()).sent;
  await page.reload();
  await until(async () => {
    const s = await snap();
    return s?.ready && s.epoch !== before.epoch ? s : null;
  }, "renderer ready after reload");
  const after = await snap();
  check("reload mints a new epoch", after.epoch !== before.epoch && typeof after.epoch === "string");
  const h1 = await until(async () => {
    const h = await health();
    return h.state === "Ready" && h.reconnects >= 1 && h.acked === h.sent ? h : null;
  }, "host Ready on the new epoch");
  await sleep(1500); // give a (wrongly) replayed command time to show up
  const settledAfter = await snap();
  check("new epoch starts neutral", allZero(settledAfter), JSON.stringify(settledAfter.expressions));
  check("only the new epoch's reset was sent (no replay)", h1.sent === sentBefore + 1 && settledAfter.lastSeq === 1, `sent ${sentBefore} -> ${h1.sent}, lastSeq ${settledAfter.lastSeq}`);
  const finalHealth = await health();
  check("no rejects, strays or protocol errors after reload", Object.keys(finalHealth.rejected).length === 0 && finalHealth.stray_acks === 0 && finalHealth.protocol_errors === 0, JSON.stringify(finalHealth));
  const postPng = await page.screenshot({ omitBackground: true });
  writeFileSync(join(outDir, "after-reload.png"), postPng);
  check("avatar still drawn after reload", analyze(postPng).middleOpaque > 2000);
  check("no page errors or CSP violations", problems.length === 0, [...new Set(problems)].slice(0, 5).join(" | "));
  exitCode = failures.length === 0 ? 0 : 1;
} catch (error) {
  check("smoke run completed", false, String(error?.stack ?? error));
} finally {
  writeFileSync(join(outDir, "smoke-result.json"), JSON.stringify({ log, failures }, null, 2));
  await browser?.close().catch(() => {});
  host.stdin.write("quit\n");
  host.stdin.end();
  setTimeout(() => host.kill(), 2000).unref();
  rmSync(pageDir, { recursive: true, force: true });
}
console.log(failures.length === 0 ? "SMOKE PASSED" : `SMOKE FAILED: ${failures.join("; ")}`);
process.exit(exitCode);
