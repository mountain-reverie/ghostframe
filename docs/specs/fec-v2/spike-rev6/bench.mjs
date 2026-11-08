// Wasm speed: node bench.mjs out/<variant>.wasm <mode> [k r lost ...]  -> TSV lines
// columns: variant mode k r lost enc_med_us enc_p95_us dec_med_us dec_p95_us ok
import fs from 'fs';
const [file, modeArg] = process.argv.slice(2);
const mode = Number(modeArg || 0);
const name = file.replace(/^.*\//, '').replace('.wasm', '');
const { instance } = await WebAssembly.instantiate(fs.readFileSync(file), {});
const e = instance.exports;
const WARM = 50, ITERS = 400, LEN = 1180;
const stats = (t) => { t.sort((a, b) => a - b); return [t[t.length >> 1], t[Math.floor(t.length * 0.95)]]; };
const only = name.startsWith('rlc') ? [10, 40] : [10, 40, 200];
e.set_mode(mode);
for (const k of only) for (const r of [2, 8]) for (const lost of [1, r]) {
  if (name.startsWith('base') && lost !== 1) continue;
  const p = e.in_ptr(); const m = new Uint8Array(e.memory.buffer);   // re-view: memory may have grown
  let x = 12345; for (let i = 0; i < k * LEN; i++) { x = (x * 1103515245 + 12345) & 0x7fffffff; m[p + i] = x >> 16; }
  const te = [];
  for (let i = 0; i < WARM + ITERS; i++) { const t0 = performance.now(); e.rs_encode(k, r, LEN); if (i >= WARM) te.push((performance.now() - t0) * 1000); }
  const ok = e.rs_decode(k, r, LEN, lost) === lost;
  const td = [];
  for (let i = 0; i < WARM + ITERS; i++) { const t0 = performance.now(); e.rs_decode(k, r, LEN, lost); if (i >= WARM) td.push((performance.now() - t0) * 1000); }
  const [em, ep] = stats(te), [dm, dp] = stats(td);
  console.log([name, mode, k, r, lost, em.toFixed(1), ep.toFixed(1), dm.toFixed(1), dp.toFixed(1), ok ? 'ok' : 'FAIL'].join('\t'));
}
