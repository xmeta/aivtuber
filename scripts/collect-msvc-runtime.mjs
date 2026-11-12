// Collects the Visual C++ runtime DLLs that the pinned Windows llama.cpp zips
// import but do not ship (issue #294), from the Visual Studio redistributable
// folder of the machine this runs on. Windows only; no download.
//
//   node scripts/collect-msvc-runtime.mjs <out-dir>
//
// The names come from packaging/local-profile-pins.json (llama.windows_runtime_dlls).
// Microsoft permits app-local deployment of these files:
//   https://learn.microsoft.com/cpp/windows/redistributing-visual-cpp-files
//   https://learn.microsoft.com/cpp/windows/determining-which-dlls-to-redistribute
// The newest installed redistributable is used (the runtime is backward
// compatible, so the newest satisfies a program built with an older toolset).
// <out-dir> receives the DLLs and REDIST-VERSION.txt (version, source folder,
// SHA-256 of each file), which scripts/package-release.mjs puts in the archive.
import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import { cpSync, existsSync, mkdirSync, readdirSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const repo = resolve(dirname(fileURLToPath(import.meta.url)), "..");

function fail(message) {
  console.error(`collect-msvc-runtime: ${message}`);
  process.exit(1);
}

if (process.platform !== "win32") fail("Windows only");
const out = process.argv[2] ? resolve(process.argv[2]) : fail("usage: node scripts/collect-msvc-runtime.mjs <out-dir>");
const names = JSON.parse(readFileSync(join(repo, "packaging", "local-profile-pins.json"), "utf8")).llama.windows_runtime_dlls;
if (!Array.isArray(names) || names.length === 0) fail("pins file lists no windows_runtime_dlls");

const vswhere = join(process.env["ProgramFiles(x86)"] ?? "C:\\Program Files (x86)", "Microsoft Visual Studio", "Installer", "vswhere.exe");
if (!existsSync(vswhere)) fail(`vswhere not found at ${vswhere}`);
const installs = execFileSync(vswhere, ["-products", "*", "-requires", "Microsoft.VisualStudio.Component.VC.Tools.x86.x64", "-property", "installationPath"], { encoding: "utf8" })
  .split(/\r?\n/)
  .filter(Boolean);
if (installs.length === 0) fail("vswhere found no Visual Studio with the C++ x64 tools");

const versionKey = (v) => v.split(".").map(Number);
const cmp = (a, b) => {
  const [x, y] = [versionKey(a), versionKey(b)];
  for (let i = 0; i < Math.max(x.length, y.length); i++) if ((x[i] ?? 0) !== (y[i] ?? 0)) return (x[i] ?? 0) - (y[i] ?? 0);
  return 0;
};

// Candidates: <VS>\VC\Redist\MSVC\<version>\x64\Microsoft.VC14x.<CRT|OpenMP>
const found = new Map();
for (const install of installs) {
  const base = join(install, "VC", "Redist", "MSVC");
  if (!existsSync(base)) continue;
  for (const version of readdirSync(base).filter((v) => /^\d+\.\d+\.\d+/.test(v))) {
    const x64 = join(base, version, "x64");
    if (!existsSync(x64)) continue;
    for (const folder of readdirSync(x64).filter((f) => /^Microsoft\.VC\d+\.(CRT|OpenMP)$/.test(f))) {
      for (const name of names) {
        const path = join(x64, folder, name);
        const prior = found.get(name);
        if (existsSync(path) && (!prior || cmp(version, prior.version) > 0)) found.set(name, { path, version });
      }
    }
  }
}
const missing = names.filter((n) => !found.has(n));
if (missing.length) fail(`not found in the Visual Studio redistributable folder: ${missing.join(", ")}`);

mkdirSync(out, { recursive: true });
const lines = [
  "Microsoft Visual C++ runtime, deployed app-local (not installed system-wide).",
  "Source: the Visual C++ Redistributable files of the Visual Studio build machine,",
  "copied unmodified under Microsoft's redistribution terms:",
  "  https://learn.microsoft.com/cpp/windows/redistributing-visual-cpp-files",
  "Purpose: the pinned llama.cpp Windows build imports these and does not ship them.",
  "",
];
for (const name of names) {
  const { path, version } = found.get(name);
  cpSync(path, join(out, name));
  const sha = createHash("sha256").update(readFileSync(path)).digest("hex");
  lines.push(`${name}  redist ${version}  sha256 ${sha}`, `  from ${path}`);
  console.log(`${name} <- ${path} (redist ${version})`);
}
writeFileSync(join(out, "REDIST-VERSION.txt"), `${lines.join("\n")}\n`);
