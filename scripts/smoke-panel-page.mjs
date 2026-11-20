// Headless-browser check of the operation panel page (#335) in a release
// archive.
//
//   node scripts/smoke-panel-page.mjs <renderer/local-avatar/dist>
//
// scripts/smoke-release-archive.mjs already checks the panel's files statically
// (panel.html exists, no remote URL, the secret travels in a header only). This
// script runs the shipped page itself, with nothing but Node and a browser the
// machine already has:
//   * a loopback HTTP server serves the unpacked page and a fake host API that
//     accepts only the per-launch secret in `X-Panel-Token`;
//   * a headless Chrome/Edge/Chromium is started with remote debugging
//     (`AIVTUBER_SMOKE_BROWSER` names another binary) and driven over the
//     DevTools protocol with Node's built-in WebSocket, so no package is
//     installed and nothing is downloaded;
//   * the page is opened with `#token=<secret>` and must reach "接続済み", which
//     only happens when the bundle boots, reads the fragment, sends the secret
//     in the header and parses the state;
//   * the status rows must come from that state;
//   * clicking ミュート must POST /api/mute with the secret and show
//     "ミュートしました", and the next poll must render the muted state.
// Exit 0 when all pass, 1 when a check fails, 2 when no browser was found.
import { spawn, spawnSync } from "node:child_process";
import { existsSync, mkdtempSync, readFileSync, rmSync, statSync } from "node:fs";
import { createServer } from "node:http";
import { tmpdir } from "node:os";
import { extname, join, resolve, sep } from "node:path";

const pageDir = process.argv[2] ? resolve(process.argv[2]) : null;
if (pageDir === null || !existsSync(join(pageDir, "panel.html"))) {
  console.error("usage: node scripts/smoke-panel-page.mjs <renderer/local-avatar/dist>");
  process.exit(2);
}

const TOKEN = "0123456789abcdef".repeat(4);
const win = process.platform === "win32";

if (typeof WebSocket !== "function") {
  console.error("this Node has no global WebSocket; Node 22 or newer is required to drive the browser");
  process.exit(2);
}

let failures = 0;
function assert(condition, message) {
  if (!condition) throw new Error(message);
}

const types = {
  ".html": "text/html; charset=utf-8",
  ".js": "text/javascript; charset=utf-8",
  ".css": "text/css; charset=utf-8",
  ".json": "application/json",
  ".svg": "image/svg+xml",
  ".png": "image/png",
  ".ico": "image/x-icon",
  ".wasm": "application/wasm",
  ".woff2": "font/woff2",
};

// ---- Fake host ------------------------------------------------------------

const requests = [];
let muted = false;

function state() {
  return {
    schema: "local-panel/1",
    uptime_ms: 12_345,
    status: {
      muted,
      audio_error: null,
      avatar_error: null,
      stream_error: null,
      content_queue: 0,
      scheduler_items: 0,
      generation_active: false,
    },
    replies: [],
    avatar_url: null,
  };
}

function json(response, body, status = 200) {
  const payload = JSON.stringify(body);
  response.writeHead(status, { "content-type": "application/json", "content-length": Buffer.byteLength(payload) });
  response.end(payload);
}

const server = createServer((request, response) => {
  const url = new URL(request.url ?? "/", "http://127.0.0.1");
  if (url.pathname.startsWith("/api/")) {
    const presented = request.headers["x-panel-token"];
    const secret = typeof presented === "string" && presented === TOKEN;
    requests.push({ method: request.method, path: url.pathname, secret });
    if (!secret) return json(response, {}, 401);
    if (url.pathname === "/api/state" && request.method === "GET") return json(response, state());
    if (url.pathname === "/api/mute" && request.method === "POST") {
      muted = true;
      return json(response, { ok: true, detail: "muted" });
    }
    if (url.pathname === "/api/stop" && request.method === "POST") {
      return json(response, { ok: true, detail: "stopped 0 items" });
    }
    return json(response, {}, 404);
  }
  const relative = url.pathname === "/" ? "/panel.html" : url.pathname;
  const file = resolve(join(pageDir, decodeURIComponent(relative)));
  // Only files under the page directory, as the app's panel server does.
  if (!file.startsWith(resolve(pageDir) + sep)) {
    response.writeHead(404);
    response.end();
    return;
  }
  if (!existsSync(file) || !statSync(file).isFile()) {
    response.writeHead(404);
    response.end();
    return;
  }
  const body = readFileSync(file);
  response.writeHead(200, { "content-type": types[extname(file).toLowerCase()] ?? "application/octet-stream" });
  response.end(body);
});

// ---- Browser --------------------------------------------------------------

function browserCandidates() {
  const fromEnv = process.env.AIVTUBER_SMOKE_BROWSER;
  if (fromEnv) return [fromEnv];
  if (win) {
    const programFiles = process.env.ProgramFiles ?? "C:\\Program Files";
    const programFilesX86 = process.env["ProgramFiles(x86)"] ?? "C:\\Program Files (x86)";
    const localAppData = process.env.LOCALAPPDATA ?? "";
    return [
      join(programFiles, "Google", "Chrome", "Application", "chrome.exe"),
      join(programFilesX86, "Google", "Chrome", "Application", "chrome.exe"),
      join(localAppData, "Google", "Chrome", "Application", "chrome.exe"),
      join(programFilesX86, "Microsoft", "Edge", "Application", "msedge.exe"),
      join(programFiles, "Microsoft", "Edge", "Application", "msedge.exe"),
    ].filter((candidate) => candidate !== "");
  }
  if (process.platform === "darwin") {
    return [
      "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
      "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
      "/Applications/Chromium.app/Contents/MacOS/Chromium",
    ];
  }
  return [
    "/usr/bin/google-chrome",
    "/usr/bin/google-chrome-stable",
    "/usr/bin/chromium",
    "/usr/bin/chromium-browser",
    "/usr/bin/microsoft-edge",
    "/usr/bin/microsoft-edge-stable",
    "/snap/bin/chromium",
  ];
}

function browser() {
  const found = browserCandidates().find((candidate) => existsSync(candidate));
  if (!found) {
    console.error(
      `no headless browser found (looked at ${browserCandidates().join(", ")}); set AIVTUBER_SMOKE_BROWSER to a Chrome/Edge/Chromium binary`,
    );
    process.exit(2);
  }
  return found;
}

class Devtools {
  constructor(url) {
    this.url = url;
    this.next = 0;
    this.waiting = new Map();
  }

  async open() {
    this.socket = new WebSocket(this.url);
    await new Promise((done, fail) => {
      const timer = setTimeout(() => fail(new Error("the DevTools socket did not open")), 15_000);
      this.socket.addEventListener("open", () => {
        clearTimeout(timer);
        done();
      });
      this.socket.addEventListener("error", () => {
        clearTimeout(timer);
        fail(new Error("the DevTools socket failed"));
      });
    });
    this.socket.addEventListener("message", (event) => {
      const message = JSON.parse(String(event.data));
      const pending = message.id === undefined ? undefined : this.waiting.get(message.id);
      if (pending === undefined) return;
      this.waiting.delete(message.id);
      if (message.error) pending.fail(new Error(`${message.error.message} (${pending.method})`));
      else pending.done(message.result);
    });
  }

  send(method, params = {}, sessionId) {
    const id = ++this.next;
    return new Promise((done, fail) => {
      this.waiting.set(id, { done, fail, method });
      this.socket.send(JSON.stringify({ id, method, params, ...(sessionId === undefined ? {} : { sessionId }) }));
    });
  }

  close() {
    try {
      this.socket?.close();
    } catch {
      // closing a broken socket is not a failure
    }
  }
}

/// Starts the browser and resolves with its DevTools WebSocket URL. `variant`
/// is "new" or "old" headless so a browser without the new mode still works.
function startBrowser(binary, variant, profile) {
  const args = [
    variant === "new" ? "--headless=new" : "--headless",
    "--disable-gpu",
    "--no-first-run",
    "--no-default-browser-check",
    "--disable-extensions",
    "--disable-dev-shm-usage",
    "--no-sandbox",
    "--remote-debugging-port=0",
    `--user-data-dir=${profile}`,
    "--window-size=1280,900",
    "about:blank",
  ];
  const child = spawn(binary, args, { stdio: ["ignore", "ignore", "pipe"], windowsHide: true });
  let stderr = "";
  return new Promise((done, fail) => {
    const timer = setTimeout(() => {
      child.kill();
      fail(new Error(`no DevTools endpoint (${variant} headless): ${stderr.trim().slice(-300)}`));
    }, 20_000);
    child.stderr.on("data", (chunk) => {
      stderr += String(chunk);
      const match = /DevTools listening on (ws:\/\/\S+)/.exec(stderr);
      if (match === null) return;
      clearTimeout(timer);
      done({ child, url: match[1] });
    });
    child.on("exit", (code) => {
      clearTimeout(timer);
      fail(new Error(`the browser exited with ${code}: ${stderr.trim().slice(-300)}`));
    });
  });
}

function stopBrowser(child) {
  if (child !== null && child.exitCode === null) {
    if (win) {
      // The whole tree: a headless browser keeps renderer processes alive.
      spawnSync(join(process.env.SystemRoot ?? "C:\\Windows", "System32", "taskkill.exe"), ["/pid", String(child.pid), "/T", "/F"], {
        stdio: "ignore",
      });
    }
    child.kill("SIGKILL");
  }
}

// ---- Main -----------------------------------------------------------------

const binary = browser();
const profile = mkdtempSync(join(tmpdir(), "aivtuber-panel-smoke-"));
let child = null;
let devtools = null;

try {
  await new Promise((done) => server.listen(0, "127.0.0.1", done));
  const port = server.address().port;
  const pageUrl = `http://127.0.0.1:${port}/panel.html#token=${TOKEN}`;

  let started = null;
  for (const variant of ["new", "old"]) {
    try {
      started = await startBrowser(binary, variant, profile);
      break;
    } catch (error) {
      if (variant === "old") throw error;
    }
  }
  child = started.child;
  devtools = new Devtools(started.url);
  await devtools.open();

  const target = await devtools.send("Target.createTarget", { url: pageUrl });
  const attached = await devtools.send("Target.attachToTarget", { targetId: target.targetId, flatten: true });
  const session = attached.sessionId;

  const evaluate = async (expression) => {
    const result = await devtools.send(
      "Runtime.evaluate",
      { expression, returnByValue: true, awaitPromise: true, userGesture: true },
      session,
    );
    if (result.exceptionDetails !== undefined) {
      throw new Error(`the page threw: ${result.exceptionDetails.text} ${result.exceptionDetails.exception?.description ?? ""}`);
    }
    return result.result.value;
  };

  const waitFor = async (label, expression, accept, timeout = 20_000) => {
    const deadline = Date.now() + timeout;
    let last;
    while (Date.now() < deadline) {
      last = await evaluate(expression);
      if (accept(last)) return last;
      await new Promise((done) => setTimeout(done, 100));
    }
    throw new Error(`${label}: timed out, last value ${JSON.stringify(last)}`);
  };

  const rowsOf = `[...document.querySelectorAll("#status .row")].map((row) => [row.querySelector(".label").textContent, row.querySelector(".value").textContent])`;

  // Each step is reported like the archive smoke's own checks.
  const step = async (name, run) => {
    try {
      const detail = await run();
      console.log(`ok        ${name}${detail === undefined ? "" : ` (${detail})`}`);
    } catch (error) {
      failures += 1;
      console.log(`FAIL      ${name}: ${error.message}`);
    }
  };

  await step("the shipped page boots, reads the secret from the fragment and connects", async () => {
    // The target exists before its document is the panel (the expression runs
    // against whatever is loaded at that instant), so every probe tolerates a
    // document that does not have the element yet.
    const text = await waitFor(
      "connection line",
      `document.getElementById("connection")?.textContent ?? ""`,
      (value) => value === "接続済み",
    );
    assert(requests.some((request) => request.path === "/api/state" && request.secret), "no /api/state was accepted with the secret");
    return text;
  });

  await step("the status rows come from the host state", async () => {
    const rows = await waitFor("status rows", rowsOf, (value) => Array.isArray(value) && value.length === 7);
    const mute = rows.find(([label]) => label === "ミュート");
    assert(mute !== undefined && mute[1] === "いいえ", `the mute row reads ${JSON.stringify(mute)}`);
    return `${rows.length} rows`;
  });

  await step("clicking ミュート sends the secret and reports it", async () => {
    // The page asks for confirmation before muting; a headless page has to say yes.
    await evaluate(`window.confirm = () => true; document.getElementById("mute").click(); true`);
    const text = await waitFor(
      "control result",
      `document.getElementById("control-result")?.textContent ?? ""`,
      (value) => value === "ミュートしました",
    );
    const muteRequests = requests.filter((request) => request.method === "POST" && request.path === "/api/mute");
    assert(muteRequests.length >= 1, "no POST /api/mute reached the host");
    assert(muteRequests.every((request) => request.secret), "a mute request arrived without the panel secret");
    return text;
  });

  await step("the next poll renders the muted state", async () => {
    await waitFor("muted row", rowsOf, (rows) => rows.some(([label, value]) => label === "ミュート" && value.includes("ミュート中")));
    return "polled";
  });

  await step("every request carried the panel secret", async () => {
    const unauthenticated = requests.filter((request) => !request.secret);
    assert(unauthenticated.length === 0, `requests without the secret: ${JSON.stringify(unauthenticated)}`);
    return `${requests.length} requests`;
  });
} catch (error) {
  failures += 1;
  console.log(`FAIL      panel session: ${error.message}`);
} finally {
  devtools?.close();
  stopBrowser(child);
  await new Promise((done) => server.close(done));
  // A browser that is still exiting can hold the profile directory for a
  // moment; the temp directory is not part of the result.
  try {
    rmSync(profile, { recursive: true, force: true });
  } catch {
    // best effort
  }
}

if (failures > 0) {
  console.error(`${failures} panel check(s) failed`);
  process.exit(1);
}
console.log("panel page smoke: all checks passed");
