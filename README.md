# Outis prover

Proves a Starknet transaction where its private inputs are: on a phone, a desktop or in a
browser, instead of on a proving server. It builds the transaction's virtual OS input from RPC
reads, runs the Starknet OS, and proves the run with StarkWare's prover. The proof is
byte-identical to the one StarkWare's transaction prover builds for the same transaction.

It runs natively (a command-line replay, and a C entry point the Outis Prover apps link) and as
WebAssembly (threaded, on Web Workers; [outis.xyz/prover](https://outis.xyz/prover). Status: testing. No releases yet.

## Layout

| Path | What it is |
|---|---|
| `crates/os_wasm_probe` | The pipeline. `TxSession` (wasm and native): the transaction's execution and storage proofs from RPC reads, the virtual OS input, the OS run, the proof. `os_native_probe`: the native command line. `native`: the C interface a wallet app proves a live transaction through (`include/outis_prover.h`): a session per transaction, called method by method with JSON as `js/tx-driver.mjs` calls the wasm one, and cancellable. `replay`: the C entry point `tx_prove_replay` with progress events, for the Outis Prover apps. |
| `js/tx-driver.mjs` | The JavaScript side of `TxSession`: the RPC reads and the miss loop, over any async RPC, with the wasm session or an async one (an app's native session). |
| `js/replay.mjs` | Proves a capture with the wasm build in Node. |
| `js/serve.mjs` | `starknet_proveTransaction` answered by the wasm build in Node, live against a node, as a wallet's page proves (the transaction never sent to the node): for submitting a proof made the client's way through the SDK or the spike. |
| `js/worker.mjs` | The web engine's worker: proves a bundled capture in a browser and reports progress and the result (outis.xyz/prover). |
| `captures/` | Four Privacy Pool transactions on Starknet Sepolia (register, deposit, pay, withdraw): the recorded RPC answers, the prove request and StarkWare's transaction prover's response, the reference for byte identity (since v0.3.0 re-proven by the transaction prover at `148de445` from each capture's own recording, `scripts/recording-node.py`). The contract classes are in `captures/classes`, symlinked from each capture (on Windows, enable git's `core.symlinks`). |
| `wasm-stubs/c-kzg` | An empty `c-kzg`: the C library has no wasm32 build, and the proving path never calls it. |
| `js/gpu.mjs` | A page's GPU worker for the web engine (builds with feature `gpu`): the striped commitments on WebGPU. |
| `scripts/` | `build-native.sh`, `build-wasm.sh [node\|web\|web-gpu]`, `build-web-engine.sh <out>` (the worker, the wasm and the captures, for a page that proves in the browser), `build-android.sh` (the static library for Android apps), `recording-node.py` (a node answering from a capture's recording, to re-prove its transaction with a newer prover). |

## Upstream

The prover and the sequencer come from StarkWare's repositories, through two forks, upstream's revision plus our commits, tagged per release
(`Cargo.toml` pins both):

- [outis-wallet/sequencer](https://github.com/outis-wallet/sequencer): tag `outis-v0.3.0`,
  `main-v0.14.4` at `148de445` (the release branch's move to proving `e0035ffc`, whose proofs
  Sepolia verifies since 2026-10-28) plus two commits of wasm32 portability fixes. Until v0.2.1 it
  was `PRIVACY-0.14.4-RC.0` (tag `outis-v0.2.1`), whose proofs Sepolia refuses.
- [outis-wallet/proving](https://github.com/outis-wallet/proving): tag `outis-v0.3.0`, `e0035ffc`,
  the revision that sequencer pins, merged into our branch: the provably.fast frontier, our striped
  prover (peak memory about 2.5 GiB instead of about 5.9 GiB on the provably.fast benchmark), the
  GPU commitment, and wasm32 and Android fixes. Its `OUTIS.md` says what each commit is and whose.

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
target/release/os_native_probe ffi captures/pay         # the same through the C interface
target/release/os_native_probe ffi captures/pay --cancel-after 3   # cancels the proof 3 s in

scripts/build-native.sh --features gpu                 # with the GPU commitment (wgpu), off until asked:
OUTIS_GPU=1 target/release/os_native_probe replay captures/pay      # commits on the GPU, same proof

scripts/build-wasm.sh node
node js/replay.mjs captures/pay --prove                 # the same, in wasm (single-threaded)

scripts/build-wasm.sh web                               # pkg-mt/, for browsers (COOP/COEP)
scripts/build-web-engine.sh ../outis/wallet/apps/prover/public   # outis.xyz/prover's engine
```

The `gpu` feature commits the proof's striped trees on a GPU, registered as stwo's striped-commit
hook (`src/gpu.rs`, which takes the module for the target). A commitment the GPU declines or fails,
or a machine without a GPU, falls back to the CPU; the proof is byte-identical either way.

**Natively** (`src/gpu/native.rs`), through wgpu (stwo-gpu-spike, from the proving fork):
`OUTIS_GPU=1` in the environment, or `tx_prove_set_gpu` in the C interface, turns it on. The
adapter is wgpu's choice, steered by `WGPU_BACKEND` and `WGPU_ADAPTER_NAME`. Keep-warm (the GPU
kept at full clocks for a window after each commitment, so the next one doesn't start on a GPU that
lowered its clocks) is off by default: `STWO_GPU_KEEP_WARM_MS=<ms>`, or
`tx_prove_set_gpu_keep_warm(ms)`, turns it on.

**In browsers (experimental)** (`src/gpu/web.rs`). `scripts/build-wasm.sh web-gpu` builds the
threaded module with feature `gpu` into `pkg-mt-gpu/`: stwo's striped commitments go to a page's
WebGPU worker, through a job table in the module's shared memory (the worker and the bridge are in
proving's `crates/stwo_gpu_spike/web`, whose README is the design note).

```
scripts/build-wasm.sh web-gpu
(cd <proving>/crates/stwo_gpu_spike/web && ./build.sh gpu)        # the GPU worker's module
PKG=pkg-mt-gpu GPU_WEB=<proving>/crates/stwo_gpu_spike/web scripts/build-web-engine.sh <out>
```

The page starts the GPU worker once (`engine/gpu.mjs`'s `startGpu()`) and sends a port to it
with each proof (`gpuPort` in the message to `engine/worker.mjs`), which attaches it to the
module's memory before the proof and detaches it after. Without a port, or without WebGPU, the
build proves on the CPU.

The Outis Prover apps (iOS and Android) link the pipeline as a static library built from this
repository (`--crate-type staticlib` for `aarch64-apple-ios`, `aarch64-apple-ios-sim` and
`aarch64-linux-android`) and bundle the captures. `scripts/build-android.sh` builds the Android
one (API 28, the NDK's clang; `VENV` and `ANDROID_HOME` as it says), which a wallet app links to
prove its own transactions through `include/outis_prover.h`.

## Measured

Native, `os_native_probe replay` on the four captures (x86-64, 36 threads): proving 9.6 to
10.6 s, peak RSS about 2.8 GiB, every proof verified and byte-identical to the reference.

WebAssembly in Chrome on the same machine (18 cores), 16 threads: proving 13.7 to 15.2 s, wasm
memory about 2.5 GiB, every proof verified and byte-identical. A single thread runs about 1.3x
native. wasm32's system allocator is one dlmalloc behind one spin lock; `memory.rs` keeps small
blocks off it, without which the proof got slower past about 8 threads.

With the GPU commitment (feature `gpu`), in Chrome on the same machine's RTX 3080 through
WebGPU, 16 threads: proving 11.1 to 12.1 s against 13.6 to 14.6 s without (1.2x), the 8 striped
commitments 2.0 to 2.3 s on the GPU against 4.6 to 5.4 s on the CPU, every proof verified and
byte-identical; the wasm memory unchanged, the tab about 200 MiB larger. Firefox 157 with WebGPU
forced on: the same. Details in proving's `crates/stwo_gpu_spike/web/README.md`.

## Credits

StarkWare, for the sequencer and the prover. The contributors to the provably.fast challenge
(Starknet Innovation), whose frontier our prover builds on. ryun1, for the public description of
the striped approach.

## License

Apache-2.0 (`LICENSE`), as upstream.
