// Minimal PE import-table reader (x64 PE32+), no dependencies. Used by the
// release smoke check (issue #294): a clean Windows has no Visual C++ runtime,
// so what a shipped executable imports decides whether it can start.
//
//   node scripts/pe-imports.mjs <file.exe|file.dll> ...
//
// Returns the DLL names from the regular and delay-load import tables, lowercased.
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { resolve } from "node:path";

export function peImports(path) {
  const b = readFileSync(path);
  if (b.readUInt16LE(0) !== 0x5a4d) throw new Error(`${path}: not a PE file`);
  const pe = b.readUInt32LE(0x3c);
  if (b.readUInt32LE(pe) !== 0x4550) throw new Error(`${path}: bad PE signature`);
  const nsec = b.readUInt16LE(pe + 6);
  const optSize = b.readUInt16LE(pe + 20);
  const opt = pe + 24;
  if (b.readUInt16LE(opt) !== 0x20b) throw new Error(`${path}: not PE32+ (x64)`);
  const dirs = opt + 112;
  const sections = [];
  for (let i = 0; i < nsec; i++) {
    const s = opt + optSize + i * 40;
    sections.push({ va: b.readUInt32LE(s + 12), vsize: b.readUInt32LE(s + 8), raw: b.readUInt32LE(s + 20), rawSize: b.readUInt32LE(s + 16) });
  }
  const off = (rva) => {
    const s = sections.find((x) => rva >= x.va && rva < x.va + Math.max(x.vsize, x.rawSize));
    if (!s) throw new Error(`${path}: RVA ${rva.toString(16)} outside sections`);
    return rva - s.va + s.raw;
  };
  const cstr = (o) => b.toString("latin1", o, b.indexOf(0, o));
  const names = new Set();
  const table = (dirIndex, entrySize, nameField) => {
    const rva = b.readUInt32LE(dirs + dirIndex * 8);
    if (rva === 0) return;
    for (let o = off(rva); ; o += entrySize) {
      const nameRva = b.readUInt32LE(o + nameField);
      if (nameRva === 0) break;
      names.add(cstr(off(nameRva)).toLowerCase());
    }
  };
  table(1, 20, 12); // import directory (name RVA at +12)
  table(13, 32, 4); // delay-load import directory (name RVA at +4)
  return [...names].sort();
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  for (const f of process.argv.slice(2)) console.log(`${f}: ${peImports(f).join(", ")}`);
}
