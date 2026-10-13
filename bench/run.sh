#!/usr/bin/env bash
# The prover's baseline: proves the captures natively in each mode, a few times, interleaved, and
# appends one JSON line per proof to bench/results/<version>/<device>.jsonl (version: git describe),
# then prints the medians. Run it on a quiet machine after `scripts/build-native.sh --features gpu`
# (or without the feature for cpu only). See BASELINES.md.
#
# usage: bench/run.sh [--modes cpu,gpu,full,lean] [--captures register,deposit,pay,withdraw]
#                     [--threads N] [--reps R] [--device LABEL] [--out FILE] [--bin PATH]
#   modes: cpu   no GPU
#          gpu   the striped commitments on the GPU (OUTIS_GPU=1)
#          full  gpu + composition, decommitment, out-of-domain samples, FRI quotients on the GPU
#          lean  gpu, lean commitments for unified memory (STWO_GPU_LEAN=1)
#   threads: default the prover's own (one per physical core); reps: default 3; device: a short
#   label for the machine (default the host name), e.g. rtx3080-i9 or m5pro.
# A full-mode run first proves once unrecorded, so its constraint kernels are compiled (a first
# proof on a machine declines them while they compile). On a GPU that also drives a display, run
# this under whatever guard keeps other GPU jobs away.
set -euo pipefail
cd "$(dirname "$0")/.."

modes=cpu captures=register,deposit,pay,withdraw threads="" reps=3 device="" out="" bin=target/release/os_native_probe
while [ $# -gt 0 ]; do
  case "$1" in
    --modes) modes=$2; shift 2 ;;
    --captures) captures=$2; shift 2 ;;
    --threads) threads=$2; shift 2 ;;
    --reps) reps=$2; shift 2 ;;
    --device) device=$2; shift 2 ;;
    --out) out=$2; shift 2 ;;
    --bin) bin=$2; shift 2 ;;
    *) echo "bench/run.sh: unknown option $1" >&2; exit 2 ;;
  esac
done
[ -x "$bin" ] || { echo "No $bin: build it first (scripts/build-native.sh --features gpu)" >&2; exit 1; }
version=$(git describe --tags --always --dirty 2>/dev/null || echo unknown)
commit=$(git rev-parse --short HEAD 2>/dev/null || echo unknown)
device=${device:-$(hostname -s)}
out=${out:-bench/results/$version/$device.jsonl}
mkdir -p "$(dirname "$out")"
case "$(uname -s)" in
  Darwin) cpu=$(sysctl -n machdep.cpu.brand_string); os="macOS $(sw_vers -productVersion)" ;;
  *) cpu=$(grep -m1 'model name' /proc/cpuinfo | cut -d: -f2- | sed 's/^ *//'); os="$(uname -s) $(uname -r)" ;;
esac

mode_env() {
  case "$1" in
    cpu) echo "" ;;
    gpu) echo "OUTIS_GPU=1" ;;
    full) echo "OUTIS_GPU=1 OUTIS_GPU_COMPOSITION=1 OUTIS_GPU_DECOMMIT=1 OUTIS_GPU_OOD=1 OUTIS_GPU_FRI=1" ;;
    lean) echo "OUTIS_GPU=1 STWO_GPU_LEAN=1" ;;
    *) echo "bench/run.sh: unknown mode $1" >&2; exit 2 ;;
  esac
}

# One proof: its log in a temporary file, one JSON line appended to $out.
prove() { # mode capture rep
  local log
  log=$(mktemp)
  # shellcheck disable=SC2046
  env -u OUTIS_GPU -u STWO_GPU_LEAN -u OUTIS_GPU_COMPOSITION -u OUTIS_GPU_DECOMMIT -u OUTIS_GPU_OOD -u OUTIS_GPU_FRI REPLAY_JSON=1 ${threads:+RAYON_NUM_THREADS=$threads} \
    $(mode_env "$1") "$bin" replay "captures/$2" > "$log" 2>&1 || true
  [ "$3" = warmup ] || python3 - "$log" "$out" "$version" "$commit" "$device" "$os" "$cpu" "$1" "$2" "$3" <<'PY'
import json, re, sys, datetime
log, out, version, commit, device, os_, cpu, mode, capture, rep = sys.argv[1:]
text = open(log, errors="replace").read()
line = {"version": version, "commit": commit, "date": datetime.datetime.now().isoformat(timespec="seconds"),
        "device": device, "os": os_, "cpu": cpu, "mode": mode, "capture": capture, "rep": int(rep)}
m = re.search(r"^report-json: (.*)$", text, re.M)
if m:
    r = json.loads(m.group(1))
    peak = r.get("peak", {})
    line.update(threads=r["threads"], prove_s=round(r["prove_s"], 3), total_s=round(r["total_s"], 3),
                verify_s=round(r["verify_s"], 3), identical=r["identical"], verified=r["verified"],
                peak_run_mib=peak.get("run_mib"), peak_rss_mib=peak.get("rss_mib"))
else:
    line.update(error=(text.strip().splitlines() or ["no output"])[-1][:300])
gpu = re.search(r"GPU commitment on (.*?) \(ready", text)
line["gpu"] = gpu.group(1) if gpu else None
line["gpu_commitments"] = len(re.findall(r"^commitment #\d+: .*: GPU ", text, re.M))
comp = re.search(r"GPU: (\d+) components", text)
if comp: line["gpu_components"] = int(comp.group(1))
open(out, "a").write(json.dumps(line) + "\n")
status = "IDENTICAL" if line.get("identical") else ("DIFFERENT" if "identical" in line else "ERROR")
print(f"{mode:5} {capture:9} rep {rep}: prove {line.get('prove_s', '-')} s, {status}, "
      f"peak {line.get('peak_run_mib', '-')} MiB")
PY
  rm -f "$log"
}

IFS=, read -r -a mode_list <<< "$modes"
IFS=, read -r -a capture_list <<< "$captures"
echo "bench: $version ($commit) on $device: $cpu, $os -> $out"
for mode in "${mode_list[@]}"; do
  if [ "$mode" = full ]; then prove full pay warmup; fi
done
for rep in $(seq 1 "$reps"); do
  for capture in "${capture_list[@]}"; do
    for mode in "${mode_list[@]}"; do prove "$mode" "$capture" "$rep"; done
  done
done

python3 - "$out" "$version" <<'PY'
import json, statistics, sys, collections
out, version = sys.argv[1:]
rows = [json.loads(l) for l in open(out) if l.strip()]
rows = [r for r in rows if r.get("version") == version and "prove_s" in r]
groups = collections.defaultdict(list)
for r in rows: groups[(r["mode"], r["capture"], r["threads"])].append(r)
print(f"\nmedians in {out}:")
print(f"{'mode':5} {'capture':9} {'threads':>7} {'n':>2} {'prove s':>8} {'peak MiB':>8}  identical")
for (mode, capture, threads), rs in sorted(groups.items()):
    print(f"{mode:5} {capture:9} {threads:7} {len(rs):2} {statistics.median(r['prove_s'] for r in rs):8.2f} "
          f"{statistics.median(r['peak_run_mib'] or 0 for r in rs):8.0f}  {all(r['identical'] and r['verified'] for r in rs)}")
PY
