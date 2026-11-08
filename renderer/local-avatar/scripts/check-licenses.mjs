// License allowlist gate for the renderer bundle's npm tree (ADR-0003
// decision 8, docs/supply-chain.adoc section 8). No dependencies: it reads the
// installed node_modules/*/package.json after `bun install --frozen-lockfile`.
//
//   Runtime closure (ships in the browser bundle): permissive allowlist only.
//   Build/test closure (everything else installed):  the same allowlist plus
//   the explicit, justified exceptions below.
//
// Exit 1 on any package whose license is missing, unparsable or not allowed.
import { existsSync, readFileSync, readdirSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const modules = join(root, "node_modules");

const ALLOWED = new Set(["MIT", "ISC", "BSD-2-Clause", "BSD-3-Clause", "Apache-2.0", "0BSD"]);

// Build/test tooling only. Each entry needs a reason; none may reach the runtime closure.
const BUILD_ONLY_EXCEPTIONS = [
  {
    prefix: "lightningcss",
    license: "MPL-2.0",
    reason:
      "CSS transformer used by Vite at build time. File-level weak copyleft, used unmodified, " +
      "never linked into or copied to dist/. Revisit if it enters the runtime closure.",
  },
];

const readPkg = (dir) => JSON.parse(readFileSync(join(dir, "package.json"), "utf8"));

function licenseOf(pkg) {
  const raw = pkg.license ?? pkg.licenses;
  if (typeof raw === "string") return raw;
  if (Array.isArray(raw)) return raw.map((l) => l.type ?? l).join(" OR ");
  if (raw && typeof raw === "object") return raw.type ?? "";
  return "";
}

/// Evaluates a small SPDX expression: OR = any allowed, AND = all allowed.
function allowed(expr, allow) {
  const e = expr.trim().replace(/^\((.*)\)$/, "$1");
  if (/\sOR\s/.test(e)) return e.split(/\sOR\s/).some((t) => allowed(t, allow));
  if (/\sAND\s/.test(e)) return e.split(/\sAND\s/).every((t) => allowed(t, allow));
  return allow.has(e.trim());
}

function resolveDep(name, fromDir) {
  for (let dir = fromDir; ; dir = dirname(dir)) {
    const candidate = join(dir, "node_modules", name);
    if (existsSync(join(candidate, "package.json"))) return candidate;
    if (dirname(dir) === dir || dir === dirname(root)) return null;
  }
}

function closure(startDir, seen = new Map()) {
  const pkg = readPkg(startDir);
  for (const name of Object.keys({ ...pkg.dependencies, ...pkg.optionalDependencies })) {
    const dir = resolveDep(name, startDir);
    if (dir === null || seen.has(dir)) continue; // optional deps may be absent on this platform
    seen.set(dir, readPkg(dir));
    closure(dir, seen);
  }
  return seen;
}

const runtime = closure(root);
const installed = new Map();
for (const entry of readdirSync(modules)) {
  if (entry.startsWith(".")) continue;
  const names = entry.startsWith("@") ? readdirSync(join(modules, entry)).map((n) => `${entry}/${n}`) : [entry];
  for (const name of names) installed.set(join(modules, name), readPkg(join(modules, name)));
}

const failures = [];
const rows = [];
for (const [dir, pkg] of installed) {
  const isRuntime = runtime.has(dir);
  const license = licenseOf(pkg);
  const exception = BUILD_ONLY_EXCEPTIONS.find((x) => pkg.name.startsWith(x.prefix) && license === x.license);
  let ok = license !== "" && allowed(license, ALLOWED);
  let note = "";
  if (!ok && exception && !isRuntime) {
    ok = true;
    note = "build-only exception";
  }
  rows.push({ name: pkg.name, version: pkg.version, license: license || "(none)", scope: isRuntime ? "runtime" : "build/test", note });
  if (!ok) failures.push(`${pkg.name}@${pkg.version}: ${license || "no license field"} (${isRuntime ? "runtime" : "build/test"})`);
}

rows.sort((a, b) => a.scope.localeCompare(b.scope) || a.name.localeCompare(b.name));
for (const r of rows) console.log(`${r.scope.padEnd(10)} ${r.name}@${r.version}  ${r.license}${r.note ? `  [${r.note}]` : ""}`);
console.log(`\n${installed.size} installed packages, ${runtime.size} in the runtime closure.`);
if (runtime.size === 0) {
  console.error("FAIL: empty runtime closure (is node_modules installed?)");
  process.exit(1);
}
if (failures.length > 0) {
  console.error(`\nFAIL: ${failures.length} package(s) outside the license allowlist:\n  ${failures.join("\n  ")}`);
  process.exit(1);
}
console.log(`License check passed (allowlist: ${[...ALLOWED].join(", ")}).`);
