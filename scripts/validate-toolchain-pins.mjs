import { readFileSync } from "node:fs";
import { resolve } from "node:path";

const root = resolve(import.meta.dirname, "..");
const read = (path) => readFileSync(resolve(root, path), "utf8");

const rustToolchain = read("rust-toolchain.toml");
const packageJson = JSON.parse(read("package.json"));
const ci = read(".github/workflows/ci.yml");

const rustMatch = rustToolchain.match(/^channel\s*=\s*"([^"]+)"$/m);
if (!rustMatch) {
  throw new Error("rust-toolchain.toml must pin an explicit channel");
}
const rustVersion = rustMatch[1];

const packageManager = packageJson.packageManager;
const bunMatch =
  typeof packageManager === "string"
    ? packageManager.match(/^bun@([^\s]+)$/)
    : null;
if (!bunMatch) {
  throw new Error("package.json packageManager must pin bun@<version>");
}
const bunVersion = bunMatch[1];

const exactDependency = (version) =>
  typeof version === "string" &&
  /^\d+\.\d+\.\d+(?:[-+][0-9A-Za-z.-]+)?$/.test(version);

for (const section of ["dependencies", "devDependencies"]) {
  for (const [name, version] of Object.entries(packageJson[section] ?? {})) {
    if (!exactDependency(version)) {
      throw new Error(
        `${section}.${name} must use an exact version, got ${JSON.stringify(version)}`,
      );
    }
  }
}

const rustPins = [...ci.matchAll(/toolchain:\s*([^\s#]+)/g)].map(
  (match) => match[1],
);
if (rustPins.length === 0 || rustPins.some((pin) => pin !== rustVersion)) {
  throw new Error(
    `CI Rust pins ${JSON.stringify(rustPins)} do not match rust-toolchain.toml ${rustVersion}`,
  );
}

const bunPins = [...ci.matchAll(/bun-version:\s*([^\s#]+)/g)].map(
  (match) => match[1],
);
if (bunPins.length === 0 || bunPins.some((pin) => pin !== bunVersion)) {
  throw new Error(
    `CI Bun pins ${JSON.stringify(bunPins)} do not match packageManager ${bunVersion}`,
  );
}

for (const command of [
  "bun install --frozen-lockfile",
  "cargo clippy --locked --workspace --all-targets -- -D warnings",
  "cargo test --locked --workspace",
  "cargo build --locked --workspace",
]) {
  if (!ci.includes(command)) {
    throw new Error(`CI is missing reproducibility gate: ${command}`);
  }
}

const parityScript = read("scripts/validate-rust-schema-parity.mjs");
if (!parityScript.includes('"--locked"')) {
  throw new Error("Rust schema parity runner must invoke cargo with --locked");
}

console.log(
  `Toolchain pins verified: Rust ${rustVersion}, Bun ${bunVersion}; locked/frozen CI gates present.`,
);
