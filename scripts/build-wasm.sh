#!/bin/sh
# The wasm builds of the prover, with their wasm-bindgen glue (wasm-bindgen-cli 0.2.126):
#   scripts/build-wasm.sh node   single-threaded, for Node: js/node/ (js/replay.mjs uses it)
#   scripts/build-wasm.sh web    threaded (Web Workers sharing the module's memory, up to 4 GiB),
#                                for browsers: pkg-mt/. Pages need COOP/COEP (shared memory).
# Needs the cairo-lang venv active (see README).
set -e
cd "$(dirname "$0")/.."
COMMON='--cfg getrandom_backend="wasm_js" -C target-feature=+simd128 -C link-arg=-zstack-size=16777216'
case "${1:-web}" in
  node)
    RUSTFLAGS="$COMMON" CARGO_TARGET_DIR=target-wasm \
      cargo build --release --target wasm32-unknown-unknown -p os_wasm_probe --lib
    wasm-bindgen --target nodejs --out-dir js/node target-wasm/wasm32-unknown-unknown/release/os_wasm_probe.wasm
    ;;
  web)
    THREADS='-C target-feature=+atomics,+bulk-memory,+mutable-globals
      -C link-arg=--shared-memory -C link-arg=--import-memory -C link-arg=--max-memory=4294967296
      -C link-arg=--export=__wasm_init_tls -C link-arg=--export=__tls_size
      -C link-arg=--export=__tls_align -C link-arg=--export=__tls_base'
    RUSTFLAGS="$COMMON $(echo $THREADS)" CARGO_TARGET_DIR=target-wasm-mt \
      cargo build --release --target wasm32-unknown-unknown -Z build-std=panic_abort,std \
      -p os_wasm_probe --lib --features threads
    wasm-bindgen --target web --remove-name-section --out-dir pkg-mt \
      target-wasm-mt/wasm32-unknown-unknown/release/os_wasm_probe.wasm
    ;;
  *) echo "usage: scripts/build-wasm.sh [node|web]" >&2; exit 1 ;;
esac
