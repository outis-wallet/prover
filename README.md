# Outis prover

Proves a Starknet transaction where its private inputs are: on a phone, a desktop or in a
browser, instead of on a proving server. It builds the transaction's virtual OS input from RPC
reads, runs the Starknet OS, and proves the run with StarkWare's prover. The proof is
byte-identical to the one StarkWare's transaction prover builds for the same transaction.

It runs natively (a command-line replay, and a C entry point the Outis Prover apps link) and as
WebAssembly (threaded, on Web Workers; outis.cash/prover). Status: testing. No releases yet.

## Layout

| Path | What it is |
|---|---|
| `crates/os_wasm_probe` | The pipeline. `TxSession` (wasm and native): the transaction's execution and storage proofs from RPC reads, the virtual OS input, the OS run, the proof. `os_native_probe`: the native command line. `replay`: the C entry point `tx_prove_replay` with progress events, for the apps. |
| `js/tx-driver.mjs` | The JavaScript side of `TxSession`: the RPC reads and the miss loop, over any async RPC. |
| `js/replay.mjs` | Proves a capture with the wasm build in Node. |
| `js/worker.mjs` | The web engine's worker: proves a bundled capture in a browser and reports progress and the result (outis.cash/prover). |
| `captures/` | Four Privacy Pool transactions on Starknet Sepolia (register, deposit, pay, withdraw): the recorded RPC answers, the prove request and the native prover's response, the reference for byte identity. The contract classes are in `captures/classes`, symlinked from each capture (on Windows, enable git's `core.symlinks`). |
| `wasm-stubs/c-kzg` | An empty `c-kzg`: the C library has no wasm32 build, and the proving path never calls it. |
| `scripts/` | `build-native.sh`, `build-wasm.sh [node\|web]`, `build-web-engine.sh <out>` (the worker, the wasm and the captures, for a page that proves in the browser). |

## Upstream

The prover and the sequencer come from StarkWare's repositories, through two forks whose
`outis` branches are upstream's revision plus our commits (`Cargo.toml` pins both):

- [outis-wallet/sequencer](https://github.com/outis-wallet/sequencer): `PRIVACY-0.14.4-RC.0`
  plus one commit, small wasm32 portability fixes.
- [outis-wallet/proving](https://github.com/outis-wallet/proving): `2b495a36`, the revision that
  sequencer release pins, plus the provably.fast frontier, our striped prover (peak memory about
  2.5 GiB instead of about 5.9 GiB on the provably.fast benchmark), and wasm32 and Android fixes.
  Its `OUTIS.md` says what each commit is and whose.

## Building

- Rust: `rust-toolchain.toml` pins `nightly-2026-01-15` (the threaded wasm build needs
  `-Z build-std`).
- Python, for the Starknet OS program, which compiles at build time: a venv with
  `cairo-lang==0.14.3a3`, active while building:
  `uv venv && uv pip install cairo-lang==0.14.3a3 && . .venv/bin/activate`.
- For wasm: `wasm-bindgen-cli` 0.2.126 (`cargo install wasm-bindgen-cli --version 0.2.126`).

```
scripts/build-native.sh
target/release/os_native_probe replay captures/pay      # proves, and compares with the reference

scripts/build-wasm.sh node
node js/replay.mjs captures/pay --prove                 # the same, in wasm (single-threaded)

scripts/build-wasm.sh web                               # pkg-mt/, for browsers (COOP/COEP)
scripts/build-web-engine.sh ../outis/wallet/apps/prover/public   # outis.cash/prover's engine
```

The Outis Prover apps (iOS and Android) link the pipeline as a static library built from this
repository (`--crate-type staticlib` for `aarch64-apple-ios`, `aarch64-apple-ios-sim` and
`aarch64-linux-android`) and bundle the captures.

## Measured

Native, `os_native_probe replay` on the four captures (x86-64, 36 threads): proving 9.6 to
10.6 s, peak RSS about 2.8 GiB, every proof verified and byte-identical to the reference.

## Credits

StarkWare, for the sequencer and the prover. The contributors to the provably.fast challenge
(Starknet Innovation), whose frontier our prover builds on. ryun1, for the public description of
the striped approach.

## License

Apache-2.0 (`LICENSE`), as upstream.
