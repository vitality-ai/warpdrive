#!/usr/bin/env bash
# Regenerate fixtures/*.wasm from modules/*.rs.
# Needs: rustup target add wasm32-unknown-unknown
# DWARF/debug custom sections (not needed to run) are stripped so fixtures stay small.
set -euo pipefail
cd "$(dirname "$0")"
mkdir -p fixtures
for f in modules/*.rs; do
  n=$(basename "$f" .rs)
  rustc --edition 2021 --target wasm32-unknown-unknown -C opt-level=2 -C panic=abort \
        --crate-type=cdylib "$f" -o "fixtures/$n.wasm"
done
python3 - <<'PY'
import glob, os
def uleb(b,i):
    r=s=0
    while True:
        x=b[i]; i+=1; r|=(x&0x7f)<<s
        if not x&0x80: break
        s+=7
    return r,i
for path in glob.glob('fixtures/*.wasm'):
    d=open(path,'rb').read(); out=bytearray(d[:8]); i=8
    while i<len(d):
        sid=d[i]; j=i+1; size,j=uleb(d,j); end=j+size
        if sid!=0: out+=d[i:end]   # keep non-custom sections only
        i=end
    open(path,'wb').write(out)
    print(f"built {os.path.basename(path)} ({len(out)} bytes)")
PY
