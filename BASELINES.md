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
  commitments for unified memory, `STWO_GPU_LEAN=1`). Every GPU mode is off by default.
- **Peak memory**: the process's RSS, or its physical footprint on Apple (what iOS counts against
  an app); on Android, RSS plus the GPU driver's allocations (kgsl) at the same moment.

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
