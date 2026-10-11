// Fails when a wire constant is retyped in production TypeScript.
//
// The constants live in `ghostframe-protocol` and reach TS through
// `src/protocol_constants.ts`. A hand-copied literal passes every other test
// while the Rust value drifts (and the protocol-stamp hash only covers Rust
// sources), so this sweeps the TS sources for the shapes a retype takes:
//
//   1. a declaration whose NAME is a protocol constant's name, initialised
//      with a numeric literal (`const TILE_SIZE = 32`, `MSG_TYPE = 0x05`, ...);
//   2. a `Codec`-style enum;
//   3. the specific magic numbers that have been retyped before, used in code
//      (the tile edge next to tile-ish identifiers, the CDF53 pass count).
//
// Comments and string literals are stripped first, so prose that mentions
// "14 passes" is fine. A genuine non-protocol use of one of these numbers
// takes an `ALLOW` entry with a reason, so every exception is on the record.
import { describe, it, expect } from 'vitest';
import { readdirSync, readFileSync, statSync } from 'node:fs';
import { join, relative } from 'node:path';
import { protocol } from '../src/protocol_constants';

const SRC = join(__dirname, '..', 'src');

/** Files exempt from the sweep, with the reason. */
const ALLOW: Record<string, string> = {
  'protocol_constants.ts': 'the one place that names the getters',
};

const PROTOCOL_NAME =
  /^(?:[A-Z0-9_]*(?:MSG_TYPE|HEADER_SIZE|TILE_SIZE|TILE_BYTES|SENTINEL|ENVELOPE|DATAGRAM_FLAG|EVICTION_REASON|HELLO_|_CAP_|PASS_COUNT|FULL_PASS_MASK|FULL_MASK)[A-Z0-9_]*|ERR_[A-Z0-9_]+|SUB_[A-Z0-9_]+)$/;

const NUMERIC = /^(?:0[xX][0-9a-fA-F_]+|\d[\d_]*)n?$/;

export interface Hit {
  line: number;
  why: string;
  text: string;
}

/** Replace comments and string/template contents with spaces, keeping
 * line structure so reported line numbers stay right. */
export function stripNonCode(src: string): string {
  let out = '';
  let i = 0;
  const n = src.length;
  while (i < n) {
    const c = src[i];
    const d = src[i + 1];
    if (c === '/' && d === '/') {
      while (i < n && src[i] !== '\n') { out += ' '; i++; }
    } else if (c === '/' && d === '*') {
      out += '  '; i += 2;
      while (i < n && !(src[i] === '*' && src[i + 1] === '/')) {
        out += src[i] === '\n' ? '\n' : ' '; i++;
      }
      out += '  '; i += 2;
    } else if (c === '"' || c === "'" || c === '`') {
      const q = c;
      out += ' '; i++;
      while (i < n && src[i] !== q) {
        if (src[i] === '\\') { out += ' '; i++; }
        out += src[i] === '\n' ? '\n' : ' '; i++;
      }
      out += ' '; i++;
    } else {
      out += c; i++;
    }
  }
  return out;
}

export function scan(source: string): Hit[] {
  const code = stripNonCode(source);
  const raw = source.split('\n');
  const hits: Hit[] = [];
  code.split('\n').forEach((line, idx) => {
    const push = (why: string) => hits.push({ line: idx + 1, why, text: raw[idx].trim() });

    // 1. NAME = numeric literal, for protocol-ish names.
    for (const m of line.matchAll(/\b(?:const|let|var|readonly)\s+([A-Za-z_][A-Za-z0-9_]*)\b[^=;]*=\s*([^;,]+)/g)) {
      if (PROTOCOL_NAME.test(m[1]) && NUMERIC.test(m[2].trim())) {
        push(`retyped constant ${m[1]}`);
      }
    }
    // 2. enum Codec { ... } / const enum Codec.
    if (/\benum\s+Codec\b/.test(line)) push('retyped Codec enum');
    // Codec table as an object literal, e.g. `{ Raw: 4, Solid: 3 }`.
    if (/\b(?:Skip|H264|PalRle|Solid|Raw|Cdf53)\s*:\s*\d/.test(line)) push('retyped Codec table');
    // A codec discriminant or message type passed/stored as a bare number.
    if (/\bonDecodeError\(\s*\d/.test(line) || /\[0\]\s*=\s*(?:0[xX][0-9a-fA-F]+|\d)\b/.test(line)) {
      push('wire discriminant written as a literal');
    }

    // 3. magic numbers that have been retyped before.
    if (/\b(?:tile\w*|dst[XY]|cols|rows|maxTiles)\b[^;]*[*/%]\s*32\b/i.test(line) ||
        /\b32\s*[*/%]\s*\(?\s*(?:tile|ts\b)/i.test(line) ||
        /\bMath\.(?:ceil|floor|min)\([^)]*[/*]\s*32\b/.test(line)) {
      push('tile edge 32 used as a literal');
    }
    if (/(?:<<\s*14\b|\b14\s*<<|\b0x3[fF]{3}\b|\*\s*14\b|\bk\s*<\s*14\b|padStart\(\s*14\b|>=\s*14\b)/.test(line)) {
      push('CDF53 pass count 14 used as a literal');
    }
  });
  return hits;
}

function walk(dir: string, out: string[] = []): string[] {
  for (const name of readdirSync(dir)) {
    const p = join(dir, name);
    if (statSync(p).isDirectory()) walk(p, out);
    else if (p.endsWith('.ts') && !p.endsWith('.d.ts')) out.push(p);
  }
  return out;
}

describe('TypeScript sources do not retype protocol constants', () => {
  it('the detector fires on the shapes it exists to catch', () => {
    // A sweep that never fires reads the same as a clean tree, so prove it
    // fires. Each of these is a real retype that existed in this codebase.
    const cases: Array<[string, string]> = [
      ['export const TILE_SIZE = 32;', 'retyped constant TILE_SIZE'],
      ['export const INPUT_MSG_TYPE = 0x05;', 'retyped constant INPUT_MSG_TYPE'],
      ['const SUB_WHEEL = 0x03;', 'retyped constant SUB_WHEEL'],
      ['export const ERR_INDEX_OOB = 5;', 'retyped constant ERR_INDEX_OOB'],
      ['const EVICTION_REASON_DISPLACED = 1;', 'retyped constant EVICTION_REASON_DISPLACED'],
      ['export const FRAME_HEADER_SIZE = 14;', 'retyped constant FRAME_HEADER_SIZE'],
      ['export const enum Codec { Skip = 0 }', 'retyped Codec enum'],
      ['const T = { Raw: 4, Solid: 3 };', 'retyped Codec table'],
      ['onDecodeError(2, 0xFF, 0xFF, c);', 'wire discriminant written as a literal'],
      ['buf[0] = 0x05;', 'wire discriminant written as a literal'],
      ['const dstX = tileX * 32;', 'tile edge 32 used as a literal'],
      ['const cols = Math.ceil(width / 32);', 'tile edge 32 used as a literal'],
      ['const FULL_PASS_MASK = (1 << 14) - 1;', 'CDF53 pass count 14 used as a literal'],
      ['const FULL_PASS_MASK = 0x3FFF;', 'retyped constant FULL_PASS_MASK'],
      ['while (k < 14 && ok) k++;', 'CDF53 pass count 14 used as a literal'],
    ];
    for (const [src, why] of cases) {
      expect(scan(src).map((h) => h.why), src).toContain(why);
    }
  });

  it('the detector ignores prose, strings and protocol_constants reads', () => {
    const clean = [
      '// 14 passes, const TILE_SIZE = 32 in a comment',
      '/* const SUB_WHEEL = 0x03; */',
      "const msg = 'const TILE_SIZE = 32';",
      'const ts = protocol().tileSize;',
      'const mask = protocol().cdf53FullPassMask;',
      'const FIFO_CAP = 32768;',
    ].join('\n');
    expect(scan(clean)).toEqual([]);
  });

  it('production sources are clean', () => {
    const files = walk(SRC);
    // The walk itself must reach the tree, or "no hits" proves nothing.
    expect(files.length).toBeGreaterThan(10);
    const offenders: string[] = [];
    for (const f of files) {
      const rel = relative(SRC, f);
      if (rel in ALLOW) continue;
      for (const h of scan(readFileSync(f, 'utf8'))) {
        offenders.push(`${rel}:${h.line}: ${h.why}: ${h.text}`);
      }
    }
    expect(offenders, `retyped wire constants (read them from protocol()):\n${offenders.join('\n')}`).toEqual([]);
  });

  it('the allowlist names files that exist', () => {
    const rels = new Set(walk(SRC).map((f) => relative(SRC, f)));
    for (const k of Object.keys(ALLOW)) expect(rels.has(k), k).toBe(true);
  });

  it('sanity: protocol() is available to the sweep', () => {
    expect(protocol().tileSize).toBe(32);
  });
});
