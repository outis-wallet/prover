# Baselines

How long the prover takes, and how much memory it needs, per release and device. Every proof
counted here was byte-identical to the native prover's response in the capture and verified.

- **Releases** are tags on `main` (`v0.1.0`, `v0.2.0`, ...). Each pins outis-wallet/proving and
  outis-wallet/sequencer by revision in `Cargo.toml`; the proving fork carries a matching tag
  (`outis-v0.2.0`, ...). Apps pin a release, and their build string names it.
- **Measuring**: `bench/run.sh` proves the captures in each mode, interleaved, three times by
  default, and appends one JSON line per proof to `bench/results/<version>/<device>.jsonl`. A
  change to the prover reruns it on the devices it touches, against the previous release's lines.
  Phones: `scripts/build-android.sh --bin --features gpu` and the same `replay` over adb (Android);
  the TxProver app's GPU panel (iOS), whose copied report is kept as the device's results.
- **Modes**: `cpu` (no GPU); `gpu` (the striped commitments on the GPU, `OUTIS_GPU=1`); `full`
  (also composition, decommitment, out-of-domain samples and FRI quotients on the GPU); `lean` (the
  commitments for unified memory, `STWO_GPU_LEAN=1`). Every GPU mode is off by default. `wasm`
  (since v0.2.1): the wasm build in Node, one thread (`scripts/build-wasm.sh node` first).
- **Peak memory**: the process's RSS, or its physical footprint on Apple (what iOS counts against
  an app); on Android, RSS plus the GPU driver's allocations (kgsl) at the same moment; in the
  wasm mode, the wasm memory (what a browser tab holds for the proof).

## v0.3.2: the proof's progress

The same pins as v0.3.1, and the same proofs: v0.3.2 adds a count of the striped commitments a
proof has started (`src/progress.rs`), read while it runs (`progressPort` in the web engine,
`tx_prove_progress` natively) for the wallets' progress bars. Each capture makes 8, on the CPU and
the GPU. The count is kept at the striped commitment's hook, which with the GPU off gives the
polynomials back for stwo to commit them itself. Not rerun with `bench/run.sh`; checked instead:

- `os_native_probe ffi` (x86-64, feature gpu), the four captures on the CPU and the GPU: every
  proof identical and verified. Pay 10.1 s against v0.3.1's 10.0 s, withdraw 10.5 s against
  10.7 s, one run each.
- The threaded wasm with feature gpu (`2be4a3f9ec40`) in Chrome 152: the four captures identical
  and verified on the CPU and the GPU (all 8 commitments on the GPU), the count posted while each
  proved, also under `worker-src 'self'` (the watcher is a file, not a blob: worker). Its times
  were taken beside another build, so they aren't kept here.

## v0.3.1: the single-threaded wasm's time back near v0.2.1's

outis-wallet/proving `outis-v0.3.1` (`8e5804af`): `outis-v0.3.0` plus one wasm32-only change, so
the native build, the Android and iOS libraries and every proof are unchanged. It answers v0.3.0's
open question, why the wasm build got 5 to 17% slower while the native one did not: once upstream
`e0035ffc` was merged in, LLVM inlined the wasm field multiplication (`mul_doubled_wasm`) into the
FFT's loops, where V8 ran it slower. In a V8 CPU profile of pay, single-threaded, the FFTs with
their multiplications took 74 s against v0.2.1's 55 s. Kept out of line (`#[inline(never)]`), as
v0.2.1's build had it, pay proven alternately with v0.2.1 in Node took 152.9 s against 150.3 s.

Prove time and wasm memory, wasm (Node, 1 thread), v0.2.1 -> v0.3.0 -> v0.3.1. One run each.

| capture | v0.2.1 | v0.3.0 | v0.3.1 | wasm memory |
|---|---|---|---|---|
| register | 151.6 s | 159.1 s | **145.4 s** | 1,932 MiB |
| deposit | 166.1 s | 183.9 s | **170.0 s** | 1,987 MiB |
| pay | 147.7 s | 168.4 s | **154.1 s** | 2,027 MiB |
| withdraw | 150.8 s | 175.7 s | **160.4 s** | 2,027 MiB |

7.6 to 8.7% faster than v0.3.0 on every capture, -4 to +6% against v0.2.1 (whose runs are from
another day; alternated on the same day, pay was within 2%). The wasm memory is unchanged to the
MiB. Native, one run each at 18 threads: 9.67 to 10.75 s, peaks 2,344 to 2,436 MiB, as v0.3.0's
within noise (the native code is the same); the GPU modes not rerun for the same reason.

Results:
- `bench/results/v0.3.1/rtx3080-i9.jsonl`: bench/run.sh, one rep of cpu and wasm, on this
  release's sources before its commit (each line's `note`).
- Not measured: the M5 Pro, the iPhones. The phones prove natively, which this does not change.

In a browser, the threaded build (`scripts/build-wasm.sh web`, what outis.cash/prover and the
wallet run), v0.3.0's engine (`7c812296f645`) and v0.3.1's (`2a8831453eed`) alternately, a fresh
worker per proof (Chrome 152 in the Claude desktop app, cross-origin isolated, same machine). Every
proof verified and byte-identical:

| capture | 16 threads, v0.3.0 -> v0.3.1 | wasm memory |
|---|---|---|
| register | 13.44 -> 13.07 s | 2,031 -> 2,064 MiB |
| deposit | 14.25 -> 14.44 s | 2,096 -> 2,081 MiB |
| pay | 13.63 -> 14.11 s | 2,147 -> 2,132 MiB |
| withdraw | 15.10 -> 15.97 s | 2,148 -> 2,118 MiB |

Pay at one thread: 141.4 -> 144.6 s. So the threaded build was not slowed in v0.3.0 (v0.2.1's pay
took 13.5 s at 16 threads in Chrome 154, in its entry below), and v0.3.1 leaves it as it was. It is
compiled with its own std (`-Z build-std`) and atomics, where LLVM's inlining evidently differs
(not checked in a profile). The fix is the single-threaded build's (Node: `js/replay.mjs`,
`js/serve.mjs`).

## v0.3.0: the revision Sepolia verifies, the same native time and memory

outis-wallet/proving `outis-v0.3.0` (upstream `e0035ffc` merged into `outis-v0.2.1`) and
outis-wallet/sequencer `outis-v0.3.0` (`main-v0.14.4` at `148de445` plus the wasm32 fixes): the
revisions whose proofs Sepolia's gateway verifies since 2026-10-28; v0.2.1's are refused
("Unknown preprocessed root"). The proof changes (more zk-blinding rows in the circuit, one
Blake2s channel not reduced to M31, an out-of-domain-sample proof of work); the captures'
references were proven again by the transaction prover at `148de445` from each capture's own
recording (`scripts/recording-node.py`), and every proof below is byte-identical to them.

Pay, prove time and peak memory, v0.2.1 -> v0.3.0. Medians of 3 runs, wasm 1.

| device | threads | cpu | gpu | full | lean | wasm (Node, 1 thread) |
|---|---|---|---|---|---|---|
| Linux, i9-10980XE (18 cores) + RTX 3080 | 18 | 9.59 -> 9.65 s, 2,365 -> 2,414 MiB | 6.54 -> 6.61 s, 2,772 -> 2,789 MiB | 4.46 -> 4.44 s, 2,769 -> 2,778 MiB | not for discrete GPUs | 148 -> **168 s**, 2,027 -> 2,027 MiB |

Native, across the four captures: prove times within noise in every mode (-14 to +12%, no
direction: cpu -6.1 to +6.1%, gpu -8.5 to +9.0%, full -14 to +12%), peaks within 50 MiB. The wasm
memory is unchanged to the MiB (2,027 / 1,987 / 1,932 / 2,027). The wasm time is not: 5 to 17%
slower on all four captures (pay 148 -> 168 s), and pay proven alternately by both builds in Node
gave v0.2.1 155 and 149 s, v0.3.0 172 and 178 s; natively, at 18 threads, nothing slowed. The
cause and its fix: v0.3.1.

End to end, 2026-10-29: a payment proven by this build's wasm in Node, live against the node
(`js/serve.mjs`, 164.5 s), accepted by Sepolia in
`0x0408c1cbcdcc411cd2d370890da7cb43c2f773f061997dffee8a1c50b9ed3423`.

Results:
- `bench/results/v0.3.0/rtx3080-i9.jsonl`: bench/run.sh, 3 reps of cpu, gpu and full and one of
  wasm, on the candidate built by path from the same code (each line's `note`); compared with
  `v0.2.1/rtx3080-i9.jsonl`.
- Not measured yet: the M5 Pro, the S25+, Chrome (the threaded wasm, unverified on v0.3.0), the
  iPhones.

## v0.2.1: less memory, the same time

outis-wallet/proving `outis-v0.2.1`: v0.2.0's prover plus two memory cuts, both from the
provably.fast frontier, and the browser GPU worker's build fix. Every proof stays byte-identical.
- The Cairo components' lookup data, held from the base trace to the interaction trace across the
  base trace commitment, is stored in the bits the AIR range-checks it to (ryun1's change in
  frontier `66670df7`). About -350 MiB of wasm memory; the browser's peak is that commitment.
- The circuit proof frees the verifier context once its base trace is written, not at the end
  (after realadii's change in frontier `55720f34`). About -200 MiB through the circuit proof.

Pay, prove time and peak memory, v0.2.0 -> v0.2.1. Medians of interleaved runs.

| device | threads | cpu | gpu | full | lean | wasm (Node, 1 thread) |
|---|---|---|---|---|---|---|
| Linux, i9-10980XE (18 cores) + RTX 3080 | 18 | 9.72 -> 9.59 s, 2,681 -> **2,365 MiB** | 6.66 -> 6.54 s, 2,981 -> 2,772 MiB | 4.50 -> 4.46 s, 3,051 -> 2,769 MiB | not for discrete GPUs | 2,380 -> **2,027 MiB** |
| macOS, Apple M5 Pro (64 GB) | 18 | 6.23 -> 6.28 s, 2,417 -> **2,072 MiB** | 4.54 -> 4.55 s, 3,794 -> 3,572 MiB | 3.17 -> 3.12 s, 5,024 -> 4,787 MiB | 4.86 -> 5.00 s, 2,406 -> 2,144 MiB | |
| macOS, Apple M5 Pro | 6 | 10.59 -> 10.55 s, 2,344 -> **1,994 MiB** | 6.79 -> 6.82 s, 3,751 -> 3,512 MiB | 3.88 -> 3.89 s, 5,006 -> 4,771 MiB | 7.02 -> 7.04 s, 2,242 -> **2,030 MiB** | |
| Galaxy S25+ (Snapdragon 8 Elite, 11 GB) | 8 | 21.8 -> 21.0 s, 2,449 -> **2,094 MiB** | 17.0 -> 17.1 s, 4,152 -> 3,921 MiB | | 16.6 -> 16.6 s, 2,918 -> **2,636 MiB** | |
| Chrome 154, RTX 3080 (web engine 5344d5ff61f6) | 16 | pay 13.8 -> 13.5 s; register 2,388 -> **2,035 MiB** | pay 11.1 -> 10.9 s; register 2,384 -> 2,007 MiB | | | |
| iPhone 12 Pro / 17 Pro, TxProver | 6 | pending (the TxProver build on v0.2.1) | | | | |

Across the four captures the peak drops in every mode: -160 to -316 MiB on the RTX desktop, -199
to -373 MiB on the M5 Pro (physical footprint), -200 to -358 MiB on the S25+ (RSS plus kgsl),
-349 to -353 MiB of wasm memory (all four:
2,027 / 1,987 / 1,932 / 2,027 MiB). Prove times are within noise (RTX -2.0 to +0.7%; M5 Pro at
6 threads -0.6 to +0.6%).

Correctness: every capture in every mode (cpu, gpu, full, lean, every hook forced to decline,
shadow with 0 words, values or samples different, and the FRI and decommitment tamper tests, whose
self-verification proves again on the CPU) is byte-identical and verifies on the RTX desktop, the
M5 Pro (Metal) and the S25+ (Adreno).

Results:
- `bench/results/v0.2.1/rtx3080-i9.jsonl`: bench/run.sh on the release commit, 3 reps of cpu, gpu
  and full and one of wasm; compared with `v0.2.0/rtx3080-i9.jsonl`, to which v0.2.0's wasm lines
  were added with bench/run.sh's new wasm mode.
- `bench/results/v0.2.1/m5pro.jsonl` and `v0.2.0/m5pro.jsonl`: both versions alternating rep by
  rep, 3 reps of each mode at 18 and at 6 threads (the candidate built from the same code by path;
  each line's `note` says how).
- Chrome: the local site build with the v0.2.1 engine (outis branch prover-engine-v0.2.1) in
  headless Chrome through gpu-scratch's review script, every proof verified and identical; its
  memory is the page's reported wasm memory, against v0.2.0's engine 6ee471ac0989 on the same
  review site.
- `bench/results/v0.2.1/s25plus.jsonl` and `v0.2.0/s25plus.jsonl`: the phone's run.sh over adb,
  both versions alternating run by run, 2 reps of cpu, gpu and lean per capture, thermally gated
  (status below moderate, battery at most 41.0 C) with a 20 s cool-off; the peak is RSS plus the
  GPU driver's allocations above their start, at the same moment. Phone times move a few percent
  with its temperature.
- The iPhones: added as their runs finish (TxProver on v0.2.1 is installed on the 12 Pro and 17 Pro).

The M5 Pro's v0.2.0 numbers here are bench/run.sh's, alternating with v0.2.1; the v0.2.0 section's
were measured earlier on the merged branches and differ (e.g. cpu at 18 threads 5.2 vs 6.2 s).

## v0.2.0: the GPU modes

Pay (the others are within about 15%), prove time, peak memory. Medians of interleaved runs.

| device | threads | cpu | gpu | full | lean |
|---|---|---|---|---|---|
| Linux, i9-10980XE (18 cores) + RTX 3080 | 18 | 9.7 s, 2.6 GiB | 6.7 s, 2.9 GiB | **4.5 s**, 3.0 GiB (VRAM 3.7 GiB) | not for discrete GPUs |
| macOS, Apple M5 Pro (64 GB) | 18 | 5.2 s, 2.4 GiB | 4.5 s, 3.8 GiB | **3.1 s**, 5.6 GiB | 4.8 s, 2.4 GiB |
| macOS, Apple M5 Pro | 6 | 11.2 s, 2.36 GiB | 7.4 s, 3.8 GiB | **3.8 s**, 5.6 GiB | 7.5 s, **2.32 GiB** |
| iPhone 12 Pro (A14, 6 GB), TxProver | 6 | 37.6 s, 2.37 GiB | | | **31.1 s, 2.14 GiB** |
| Galaxy S25+ (Snapdragon 8 Elite, 11 GB) | 8 | 20.8 s, 2.45 GiB | 18.7 s, 4.2 GiB | | **17.6 s**, 2.99 GiB |
| Chrome 154, RTX 3080 (16 threads, web engine) | 16 | 14.2 s | 10.9 s | | |

The RTX 3080 row is `bench/results/v0.2.0/rtx3080-i9.jsonl` (bench/run.sh, 3 interleaved reps,
the machine quiet), every capture:

| capture | cpu | gpu | full |
|---|---|---|---|
| register | 9.81 s | 6.83 s | 4.69 s |
| deposit | 10.64 s | 7.83 s | 5.51 s |
| pay | 9.72 s | 6.66 s | 4.50 s |
| withdraw | 9.57 s | 6.55 s | 4.36 s |

Notes:
- The other rows were measured on the commits this release merged (outis-wallet/prover
  `gpu-pipeline` and `gpu-lean`, the same code), not yet with `bench/run.sh`. Its result files
  replace them per device as they are rerun.
- The iPhone's lean runs were at thermal state serious (the CPU run's at nominal), so its gain is
  understated. The S25+'s lean holds the GPU's copy of the coefficients (Adreno has no host memory
  import), hence about 0.5 GiB over its CPU peak.
- `full` holds the trace's coefficients on the GPU (about 2 GiB): for desktops and servers, not
  phones. `lean` is for unified memory (Apple, Android), where it reads the host's coefficients in
  place (Apple) and falls back to the CPU on any GPU failure.

## v0.1.0: the CPU prover

`main` at 8ef53a1, as the apps shipped it (one thread per core, no GPU): Linux i9-10980XE, 36
threads, 9.6 to 10.6 s per capture (README); iPhone 12 Pro, 6 threads, 36.4 to 40.8 s, 2.46 GiB.
