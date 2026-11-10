// Assembles one local-profile release archive (issue #294 S1) from build
// outputs that already exist. It never builds, never downloads and never
// bundles model weights or voice files.
//
//   node scripts/package-release.mjs --version v0.1.0 --out dist-release \
//     --sherpa-archive-dir DIR --rust-notices FILE [--target-dir target/release] \
//     [--renderer-dist renderer/local-avatar/dist]
//
// Platform is the platform this script runs on (win32-x64 or linux-x64): the
// engine libraries are the ones pinned for that platform in
// scripts/fetch-sherpa-onnx.mjs and in the Rust pins
// (crates/adapters/src/speech_stt/pins.rs), so no other platform is packaged.
//
// Output: <out>/aivtuber-local-<version>-<windows|linux>-x64.<zip|tar.gz>
// The archive has one top-level directory. The engine libraries sit beside the
// executables because the sidecars verify the libraries in their own
// directory (docs/supply-chain.adoc sections 9 and 10).
import { execFileSync } from "node:child_process";
import {
  cpSync,
  existsSync,
  mkdirSync,
  mkdtempSync,
  readdirSync,
  readFileSync,
  rmSync,
  statSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const repo = resolve(dirname(fileURLToPath(import.meta.url)), "..");

const PLATFORMS = {
  "win32-x64": {
    label: "windows-x64",
    exe: ".exe",
    ext: "zip",
    // Pinned in crates/adapters/src/speech_stt/pins.rs.
    libs: ["sherpa-onnx-c-api.dll", "onnxruntime.dll", "onnxruntime_providers_shared.dll"],
    archivePrefix: "sherpa-onnx-v2.0.1-win-x64-shared-MT-Release-lib",
  },
  "linux-x64": {
    label: "linux-x64",
    exe: "",
    ext: "tar.gz",
    libs: ["libsherpa-onnx-c-api.so", "libonnxruntime.so"],
    archivePrefix: "sherpa-onnx-v2.0.1-linux-x64-shared-lib",
  },
};

// Bundled executables. Keep in step with docs/supply-chain.adoc section 11 and
// the release workflow's build steps.
export const BINARIES = [
  "aivtuber-app",
  "aivtuberctl",
  "speech-vad-sidecar",
  "speech-stt-worker",
  "local-tts-sidecar",
  "twitch-chat-ingress",
  "youtube-chat-ingress",
];

// Node scripts shipped for the model fetch step. Paths are repository-relative
// and kept, because the scripts locate each other relative to the repository
// root; the quickstart commands then run unchanged from the unpacked directory.
const OPERATOR_SCRIPTS = [
  "scripts/fetch-local-profile.mjs",
  "scripts/fetch-sherpa-onnx.mjs",
  "scripts/synthesize-starter-audio.mjs",
  "renderer/local-avatar/smoke/fetch-model.mjs",
];

// The renderer's shipped runtime closure (docs/supply-chain.adoc section 8).
const RENDERER_ROOT_DEPS = ["three", "@pixiv/three-vrm"];

function fail(message) {
  console.error(`package-release: ${message}`);
  process.exit(1);
}

function parseArgs(argv) {
  const out = {};
  for (let i = 0; i < argv.length; i += 2) {
    const key = argv[i];
    if (!key?.startsWith("--") || argv[i + 1] === undefined) fail(`bad argument near ${key}`);
    out[key.slice(2)] = argv[i + 1];
  }
  return out;
}

/// Windows ships bsdtar as System32\tar.exe; a GNU tar earlier on PATH (Git
/// Bash) cannot write zip and reads `C:\x` as a remote host.
function tarBinary() {
  return process.platform === "win32" ? join(process.env.SystemRoot ?? "C:\\Windows", "System32", "tar.exe") : "tar";
}

function readJson(path) {
  return JSON.parse(readFileSync(path, "utf8"));
}

function licenseFiles(dir) {
  return readdirSync(dir).filter((name) => /^(licen[sc]e|copying|notice)(\..*)?$/i.test(name));
}

function resolveDep(name, fromDir, stop) {
  for (let dir = fromDir; ; dir = dirname(dir)) {
    const candidate = join(dir, "node_modules", name);
    if (existsSync(join(candidate, "package.json"))) return candidate;
    if (dirname(dir) === dir || dir === stop) return null;
  }
}

/// Licence texts of the renderer's runtime closure (what is bundled into the
/// page): walks dependencies from the two pinned production roots.
function rendererNotices(rendererRoot) {
  const seen = new Map();
  const walk = (dir) => {
    const pkg = readJson(join(dir, "package.json"));
    for (const name of Object.keys({ ...pkg.dependencies, ...pkg.optionalDependencies })) {
      const found = resolveDep(name, dir, dirname(rendererRoot));
      if (found === null || seen.has(found)) continue;
      seen.set(found, readJson(join(found, "package.json")));
      walk(found);
    }
  };
  const rootPkg = readJson(join(rendererRoot, "package.json"));
  for (const name of RENDERER_ROOT_DEPS) {
    if (!rootPkg.dependencies?.[name]) fail(`renderer package.json no longer lists production dependency ${name}`);
    const found = resolveDep(name, rendererRoot, dirname(rendererRoot));
    if (found === null) fail(`renderer node_modules is missing ${name}; run bun install --frozen-lockfile first`);
    seen.set(found, readJson(join(found, "package.json")));
    walk(found);
  }
  const sections = [];
  for (const [dir, pkg] of [...seen].sort((a, b) => a[1].name.localeCompare(b[1].name))) {
    const files = licenseFiles(dir);
    const text = files.length
      ? files.map((f) => readFileSync(join(dir, f), "utf8").trim()).join("\n\n")
      : `(no licence file shipped in the package; declared licence: ${pkg.license ?? "unknown"})`;
    sections.push(`${pkg.name} ${pkg.version} (${pkg.license ?? "unknown"})\n${"-".repeat(60)}\n${text}\n`);
  }
  return sections.join("\n");
}

function main() {
  const args = parseArgs(process.argv.slice(2));
  const platform = PLATFORMS[`${process.platform}-${process.arch}`];
  if (!platform) fail(`unsupported build platform ${process.platform}-${process.arch}; only win32-x64 and linux-x64 are pinned`);
  const version = args.version ?? fail("--version is required");
  if (!/^[A-Za-z0-9._-]+$/.test(version)) fail(`--version ${JSON.stringify(version)} must be [A-Za-z0-9._-]+`);
  const out = resolve(args.out ?? fail("--out is required"));
  const targetDir = resolve(args["target-dir"] ?? join(repo, "target", "release"));
  const sherpaDir = resolve(args["sherpa-archive-dir"] ?? fail("--sherpa-archive-dir is required"));
  const rendererDist = resolve(args["renderer-dist"] ?? join(repo, "renderer", "local-avatar", "dist"));
  const rustNotices = resolve(args["rust-notices"] ?? fail("--rust-notices is required"));

  const name = `aivtuber-local-${version}-${platform.label}`;
  const work = mkdtempSync(join(tmpdir(), "aivtuber-package-"));
  const stage = join(work, name);
  mkdirSync(stage, { recursive: true });

  try {
    // 1. Executables.
    for (const bin of BINARIES) {
      const src = join(targetDir, bin + platform.exe);
      if (!existsSync(src)) fail(`missing built executable ${src}`);
      cpSync(src, join(stage, bin + platform.exe));
    }

    // 2. Engine libraries, only from the verified archive, only the pinned
    // file names. The sidecars re-verify their SHA-256 at every start.
    const archives = existsSync(sherpaDir)
      ? readdirSync(sherpaDir).filter((f) => f.startsWith(platform.archivePrefix) && f.endsWith(".tar.bz2"))
      : [];
    if (archives.length !== 1) fail(`expected exactly one ${platform.archivePrefix}*.tar.bz2 in ${sherpaDir} (made by scripts/fetch-sherpa-onnx.mjs lib)`);
    const extract = join(work, "sherpa");
    mkdirSync(extract);
    execFileSync(tarBinary(), ["-xjf", join(sherpaDir, archives[0]), "-C", extract], { stdio: "inherit" });
    const libDir = join(extract, readdirSync(extract)[0], "lib");
    for (const lib of platform.libs) {
      if (!existsSync(join(libDir, lib))) fail(`engine library ${lib} not found in ${archives[0]}`);
      cpSync(join(libDir, lib), join(stage, lib));
    }

    // 3. Renderer page build. A VRM model is operator-supplied and never shipped.
    if (!existsSync(join(rendererDist, "index.html"))) fail(`renderer build missing: ${rendererDist}/index.html (run bun run build)`);
    const pageDir = join(stage, "renderer", "local-avatar", "dist");
    cpSync(rendererDist, pageDir, { recursive: true });
    const stray = [];
    const scan = (dir) => {
      for (const entry of readdirSync(dir, { withFileTypes: true })) {
        const p = join(dir, entry.name);
        if (entry.isDirectory()) scan(p);
        else if (/\.(vrm|gguf|onnx|bin)$/i.test(entry.name)) stray.push(p);
      }
    };
    scan(pageDir);
    if (stray.length) fail(`refusing to package model-like files in the page build: ${stray.join(", ")}`);

    // 4. Starter reaction pack (descriptors only; audio is generated locally)
    // and the operator fetch/synthesis scripts, in the repository's layout.
    // They need Node 20+ only; until the first-run command (#294 S2) this is
    // how the models are fetched.
    cpSync(join(repo, "examples", "starter-reaction-pack"), join(stage, "examples", "starter-reaction-pack"), {
      recursive: true,
      filter: (src) => !/[\\/]generated([\\/]|$)/.test(src),
    });
    for (const script of OPERATOR_SCRIPTS) {
      mkdirSync(dirname(join(stage, script)), { recursive: true });
      cpSync(join(repo, script), join(stage, script));
    }

    // 5. Licences and notices.
    if (existsSync(join(repo, "LICENSE"))) cpSync(join(repo, "LICENSE"), join(stage, "LICENSE"));
    cpSync(rustNotices, join(stage, "THIRD-PARTY-LICENSES-RUST.html"));
    const engine = readFileSync(join(repo, "packaging", "ENGINE-LIBRARIES-NOTICE.txt"), "utf8");
    const rendererRoot = join(repo, "renderer", "local-avatar");
    writeFileSync(
      join(stage, "THIRD-PARTY-NOTICES.txt"),
      `${engine}\n\n==== Avatar page (renderer/local-avatar bundle) ====\n\n${rendererNotices(rendererRoot)}`,
    );

    // 6. README.
    const readme = readFileSync(join(repo, "packaging", "RELEASE-README.txt"), "utf8")
      .replaceAll("{{VERSION}}", version)
      .replaceAll("{{PLATFORM}}", platform.label);
    writeFileSync(join(stage, "README.txt"), readme);

    // 7. Archive. tar.gz is made reproducible (sorted, fixed owner and mtime);
    // zip is bsdtar's and carries file times.
    mkdirSync(out, { recursive: true });
    const archive = join(out, `${name}.${platform.ext}`);
    rmSync(archive, { force: true });
    if (platform.ext === "zip") {
      execFileSync(tarBinary(), ["-a", "-cf", archive, "-C", work, name], { stdio: "inherit" });
    } else {
      const epoch = process.env.SOURCE_DATE_EPOCH ?? "0";
      execFileSync(
        "tar",
        [
          "--sort=name",
          `--mtime=@${epoch}`,
          "--owner=0",
          "--group=0",
          "--numeric-owner",
          "--use-compress-program=gzip -n",
          "-cf",
          archive,
          "-C",
          work,
          name,
        ],
        { stdio: "inherit" },
      );
    }
    console.log(`packaged ${archive} (${statSync(archive).size} bytes)`);
  } finally {
    rmSync(work, { recursive: true, force: true });
  }
}

if (resolve(process.argv[1] ?? "") === fileURLToPath(import.meta.url)) main();
