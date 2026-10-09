#!/bin/sh
# The native prover: target/release/os_native_probe. Replay a capture with
#   target/release/os_native_probe replay captures/pay
# Needs the cairo-lang venv active (see README): the Starknet OS program compiles at build time.
set -e
cd "$(dirname "$0")/.."
cargo build --release -p os_wasm_probe --bin os_native_probe "$@"
