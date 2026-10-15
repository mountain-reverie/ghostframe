import fs from 'fs';
const buf = fs.readFileSync(process.argv[2]);
const {instance} = await WebAssembly.instantiate(buf, {});
const e = instance.exports;
const mem = () => new Uint8Array(e.memory.buffer);
function run(k, r, len, lost, iters) {
  const p = e.in_ptr(); const m = mem();
  for (let i = 0; i < k*len; i++) m[p+i] = (i*131 + 7) & 255;
  let t0 = performance.now(); for (let i=0;i<iters;i++) e.rs_encode(k,r,len); let te=(performance.now()-t0)/iters;
  let ok; t0 = performance.now(); for (let i=0;i<iters;i++) ok=e.rs_decode(k,r,len,lost); let td=(performance.now()-t0)/iters;
  console.log(`k=${k} r=${r} len=${len} lost=${lost}: encode ${(te*1000).toFixed(0)} us, decode ${(td*1000).toFixed(0)} us, restored_ok=${ok}`);
}
run(10,1,128,1,2000); run(10,2,1180,2,500); run(12,3,1180,3,300); run(40,8,1180,8,100); run(200,40,1180,40,20); run(200,40,1180,1,20);
