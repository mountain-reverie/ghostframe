#!/usr/bin/env python3
# SPDX-License-Identifier: MIT OR Apache-2.0
# Counts unsafe blocks, unsafe fns, unsafe impls and SAFETY comments per crate.
# Usage: python3 -I docs/reviews/data/2026-11-25/count_unsafe.py <repo root>
import os, re, sys, collections
root = sys.argv[1]
blk = re.compile(r'\bunsafe\s*\{')
fndef = re.compile(r'\bunsafe\s+(?:extern\s+"[^"]*"\s+)?fn\s+\w+')
impl = re.compile(r'\bunsafe\s+impl\b')
safety = re.compile(r'//.*\bSAFETY\b', re.I)
safety_doc = re.compile(r'///\s*#\s*Safety')
def is_test(path):
    p = path.replace(root,'')
    return '/tests/' in p or p.endswith('tests.rs') or p.endswith('_tests.rs') or '/benches/' in p
res = collections.defaultdict(lambda: collections.Counter())
perfile = collections.defaultdict(lambda: collections.Counter())
for crate in sorted(os.listdir(root)):
    cdir = os.path.join(root, crate)
    if not os.path.isfile(os.path.join(cdir,'Cargo.toml')) or crate=='target': continue
    for dp, dn, fn in os.walk(cdir):
        dn[:] = [d for d in dn if d not in ('target','node_modules','dist','pkg')]
        for f in fn:
            if not f.endswith('.rs'): continue
            p = os.path.join(dp,f)
            lines = open(p, errors='replace').read().split('\n')
            kind = 'test' if is_test(p) else 'src'
            # crude: within a file, after a line '#[cfg(test)]' followed by 'mod tests' treat remainder as test
            in_test = False
            for i,l in enumerate(lines):
                s = l.strip()
                if s.startswith('#[cfg(test)]') and i+1 < len(lines) and re.match(r'\s*(pub(\(\w+\))?\s+)?mod\s+\w+', lines[i+1]):
                    in_test = True
                k = 'test' if (kind=='test' or in_test) else 'src'
                code = l.split('//')[0] if not s.startswith('//') else ''
                nb = len(blk.findall(code)); nf = len(fndef.findall(code)); ni = len(impl.findall(code))
                c = res[crate]; pf = perfile[(crate, p.replace(root+'/',''))]
                c[k+'_blocks'] += nb; c[k+'_fn'] += nf; c[k+'_impl'] += ni
                pf['blocks'] += nb; pf['fn'] += nf; pf['impl'] += ni
                if safety.search(l): c[k+'_safety'] += 1; pf['safety'] += 1
                if safety_doc.search(l): c[k+'_safetydoc'] += 1; pf['safetydoc'] += 1
                if nb or ni:
                    # documented if SAFETY comment within the 6 preceding lines or same line
                    win = lines[max(0,i-6):i+1]
                    doc = any(safety.search(w) for w in win)
                    if not doc: c[k+'_undoc'] += (nb+ni); pf['undoc'] += nb+ni
print('crate | src blocks | src unsafe fn | src unsafe impl | src SAFETY | src #Safety | src undoc(blk+impl) | test blocks | test SAFETY | test undoc')
for crate,c in res.items():
    if sum(c.values())==0: continue
    print(f"{crate} | {c['src_blocks']} | {c['src_fn']} | {c['src_impl']} | {c['src_safety']} | {c['src_safetydoc']} | {c['src_undoc']} | {c['test_blocks']} | {c['test_safety']} | {c['test_undoc']}")
print()
top = sorted(perfile.items(), key=lambda kv: -kv[1]['undoc'])[:30]
for (crate,p),c in top:
    if c['undoc']: print(f"{p} blocks={c['blocks']} fn={c['fn']} impl={c['impl']} safety={c['safety']} undoc={c['undoc']}")
