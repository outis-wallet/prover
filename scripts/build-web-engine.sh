#!/bin/sh
# The web engine for a page that proves in the browser (outis.cash/prover): its worker
# (engine/), the threaded wasm build (pkg-mt/) and the captures (captures/), in the layout the
# worker reads. Build the wasm first (scripts/build-wasm.sh web).
#
# Usage: scripts/build-web-engine.sh <out dir>, e.g. outis/wallet/apps/prover/public
# (needs python3 and gzip). Only engine/, pkg-mt/ and captures/ are replaced in the out dir.
# CAPTURES=0 leaves the captures out: a wallet proves its own transactions (the worker's live mode).
# Pages served with it need COOP: same-origin and COEP: require-corp (threads need shared memory).
#
# The GPU commitment: PKG=pkg-mt-gpu (scripts/build-wasm.sh web-gpu) takes the build with feature
# gpu, and GPU_WEB=<proving>/crates/stwo_gpu_spike/web (built with its ./build.sh gpu) adds the
# page's GPU worker, as gpu/ (gpu.worker.mjs and its module); the page starts it with
# engine/gpu.mjs.
set -e
[ -n "$1" ] || { echo "usage: scripts/build-web-engine.sh <out dir>" >&2; exit 1; }
mkdir -p "$1"
OUT=$(cd "$1" && pwd)
cd "$(dirname "$0")/.."
PKG=${PKG:-pkg-mt}
[ -f "$PKG/os_wasm_probe_bg.wasm" ] || { echo "No $PKG/: run scripts/build-wasm.sh web first." >&2; exit 1; }
rm -rf "$OUT/engine" "$OUT/pkg-mt" "$OUT/captures" "$OUT/gpu"
mkdir -p "$OUT/engine" "$OUT/pkg-mt"

# The wasm module: no name section (debug symbols), gzipped, named by its hash so it can be cached
# forever.
tmp=$(mktemp -d)
python3 - "$PKG/os_wasm_probe_bg.wasm" "$tmp/stripped.wasm" <<'PY'
import sys
def leb(data, i):
    result = shift = 0
    while True:
        byte = data[i]; i += 1; result |= (byte & 0x7F) << shift; shift += 7
        if byte < 0x80: return result, i
data = open(sys.argv[1], "rb").read()
out = bytearray(data[:8]); i = 8
while i < len(data):
    start = i; section = data[i]; i += 1; size, i = leb(data, i)
    if section == 0:
        name_length, j = leb(data, i)
        if data[j:j + name_length] == b"name":
            i += size
            continue
    out += data[start:i + size]; i += size
open(sys.argv[2], "wb").write(out)
PY
gzip -9 -n -c "$tmp/stripped.wasm" > "$tmp/module.wasm.gz"
hash=$(sha256sum "$tmp/module.wasm.gz" | cut -c1-12)
wasm_file="os_wasm_probe_bg.$hash.wasm.gz"
mv "$tmp/module.wasm.gz" "$OUT/pkg-mt/$wasm_file"
rm -rf "$tmp"

# The JS glue, and wasm-bindgen-rayon's worker helper, which imports its package as a directory
# ('../../..'): static hosts serve no module for a directory, so point it at the file.
cp "$PKG/os_wasm_probe.js" "$OUT/pkg-mt/"
cp -r "$PKG/snippets" "$OUT/pkg-mt/"
sed -i.bak "s|import('../../..')|import('../../../os_wasm_probe.js')|" "$OUT"/pkg-mt/snippets/*/src/workerHelpers.js
rm -f "$OUT"/pkg-mt/snippets/*/src/workerHelpers.js.bak

# The engine.
sed "s|__WASM_FILE__|$wasm_file|" js/worker.mjs > "$OUT/engine/worker.mjs"
cp js/tx-driver.mjs js/gpu.mjs js/progress-watcher.mjs "$OUT/engine/"

# The GPU worker, in the layout its script imports its module from (pkg/gpu/).
if [ -n "$GPU_WEB" ]; then
  [ -f "$GPU_WEB/pkg/gpu/stwo_gpu_web_worker_bg.wasm" ] || { echo "No $GPU_WEB/pkg/gpu/: run its build.sh gpu first." >&2; exit 1; }
  mkdir -p "$OUT/gpu/pkg"
  cp "$GPU_WEB/gpu.worker.mjs" "$OUT/gpu/"
  cp -r "$GPU_WEB/pkg/gpu" "$OUT/gpu/pkg/"
  rm -f "$OUT"/gpu/pkg/gpu/*.d.ts "$OUT"/gpu/pkg/gpu/package.json
fi

[ "${CAPTURES:-1}" = 0 ] && { du -sh "$OUT/pkg-mt/$wasm_file"; exit 0; }

# The captures (already trimmed to what the replay reads), with fixed names, and the classes once.
mkdir -p "$OUT/captures/classes"
for capture in captures/*/; do
  action=$(basename "$capture")
  [ "$action" = classes ] && continue
  mkdir -p "$OUT/captures/$action"
  cp "$capture"/request-*.json "$OUT/captures/$action/request.json"
  cp "$capture"/response-*.json "$OUT/captures/$action/response.json"
  cp "$capture/rpc-log.jsonl" "$OUT/captures/$action/"
done
cp captures/classes/casm-*.json "$OUT/captures/classes/"

du -sh "$OUT/pkg-mt/$wasm_file" "$OUT/captures"
