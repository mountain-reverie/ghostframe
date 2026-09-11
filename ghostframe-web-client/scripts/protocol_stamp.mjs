// Recomputes ghostframe-client-wasm's GHOSTFRAME_PROTOCOL_STAMP in JS.
// Must stay byte-for-byte equivalent to ../../ghostframe-client-wasm/build.rs:
// same directories, same sort order, same relative-path normalisation, same
// FNV-1a constants. A divergence here makes the staleness guard vacuous, so
// change both or neither.
import { readdirSync, readFileSync, statSync } from 'node:fs';
import { join, relative, sep } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = join(fileURLToPath(new URL('.', import.meta.url)), '..', '..');
const STAMPED_DIRS = ['ghostframe-client-core/src', 'ghostframe-protocol/src'];

const OFFSET = 0xcbf29ce484222325n;
const PRIME = 0x100000001b3n;
const MASK = (1n << 64n) - 1n;

function fnv1a(bytes, hash) {
  for (const b of bytes) {
    hash ^= BigInt(b);
    hash = (hash * PRIME) & MASK;
  }
  return hash;
}

function collect(dir, out) {
  for (const name of readdirSync(dir)) {
    const path = join(dir, name);
    if (statSync(path).isDirectory()) collect(path, out);
    else if (name.endsWith('.rs')) out.push(path);
  }
}

export function protocolStamp() {
  const files = [];
  for (const rel of STAMPED_DIRS) collect(join(ROOT, rel), files);
  // Rust sorts PathBuf, which compares as OS strings — byte order on unix.
  files.sort();

  let hash = OFFSET;
  for (const path of files) {
    const rel = relative(ROOT, path).split(sep).join('/');
    hash = fnv1a(Buffer.from(rel, 'utf8'), hash);
    hash = fnv1a(readFileSync(path), hash);
  }
  return hash.toString(16).padStart(16, '0');
}
