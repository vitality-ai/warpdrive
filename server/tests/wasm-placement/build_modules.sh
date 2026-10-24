#!/usr/bin/env bash
# Regenerate fixtures/*.wasm from modules/*.rs (needs: rustup target add wasm32-unknown-unknown)
set -euo pipefail
cd "$(dirname "$0")"
mkdir -p fixtures
for f in modules/*.rs; do
  n=$(basename "$f" .rs)
  rustc --edition 2021 --target wasm32-unknown-unknown -C opt-level=2 -C panic=abort \
        --crate-type=cdylib "$f" -o "fixtures/$n.wasm"
  echo "built fixtures/$n.wasm ($(wc -c <fixtures/$n.wasm) bytes)"
done
