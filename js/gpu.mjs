// The page's side of the GPU commitment (a build with feature gpu, scripts/build-wasm.sh web-gpu).
// The GPU worker (gpu/gpu.worker.mjs, from proving's crates/stwo_gpu_spike/web) owns the WebGPU
// device and the compiled kernels, so the page creates it once and keeps it; each proof gets a
// port to it, which the engine's worker (worker.mjs) attaches to its wasm memory before the proof
// and detaches after. The page never holds the memory.
//
//   import { startGpu } from "../engine/gpu.mjs";
//   const gpu = await startGpu();              // once; gpu.ready, gpu.adapter, gpu.error
//   const gpuPort = gpu.ready ? gpu.port() : undefined;
//   worker.postMessage({ action, threads, gpuPort }, gpuPort ? [gpuPort] : []);
//
// Without WebGPU (or an adapter) gpu.ready is false, and the proof runs on the CPU: send no port.
// scripts/build-web-engine.sh puts this file at engine/gpu.mjs and the GPU worker at gpu/.
//
// The GPU worker keeps what its commitments reuse between proofs (the device buffers of the largest
// tree so far, the twiddles, its transfer rings, the plans of the last trees: about 1.7 GiB of video
// memory for an Outis proof): the page's later proofs start with them. gpu.release() frees them,
// for a page done proving for a while (the next proof makes them again).

/**
 * Starts the GPU worker and resolves when it has a device (or failed to get one). `configure`
 * goes to the worker first (each key optional): `kick_ms` (Firefox's poll kicks), and the
 * commitments' `compute_twiddles` (compute the twiddles in the worker rather than read the
 * prover's), `commit_chunk` (its transfer rings' chunk in bytes, 32 MiB), `upload_ring` and
 * `readback_ring` (their depths, 3 and 3), `max_gpu_mib` (a commitment needing more video memory
 * goes to the CPU), `keep` (false: every commitment frees what it used), `max_plans` (the trees
 * whose plans are kept, 16), `streamed` (false: stripe by stripe, without overlapping its
 * transfers), `upload` ("write": writeBuffer instead of the ring), `serial` and `timestamps`
 * (diagnostics: each phase timed alone; the GPU's own time), and `keep_warm_ms` (keep-warm, see
 * below; absent or 0: off, the default). A worker with no device after `timeoutMs` counts as
 * failed (a wedged driver can leave requestAdapter pending).
 *
 * Keep-warm (`keep_warm_ms: N`, off by default on every adapter): a discrete GPU lowers its clocks
 * during the prover's CPU phases, and each commitment then computes at the clocks it climbs back
 * to. With keep-warm on, the GPU worker keeps one tiny dispatch in flight for N ms after the
 * proof's attach, its start and each commitment (about 3000 is a proof's longest CPU phase between
 * two commitments), pauses it during a commitment and stops it when the proof detaches. It costs
 * the power of a GPU at full clocks for most of a proof (about 100 W on an RTX 3080), and the GPU
 * is the page's too: an option for the page to offer (e.g. on a desktop with a discrete GPU). Its
 * dispatches are in the report (`gpu.warm`; per commitment `warm`, `warm_dispatches`).
 */
export async function startGpu({ url = new URL("../gpu/gpu.worker.mjs", import.meta.url), configure, timeoutMs = 20000 } = {}) {
  const worker = new Worker(url, { type: "module" });
  if (configure) worker.postMessage({ configure });
  const ready = await new Promise((resolve) => {
    setTimeout(() => resolve({ ready: false, error: `the GPU worker had no device after ${timeoutMs} ms` }), timeoutMs);
    worker.addEventListener("message", ({ data }) => resolve(data), { once: true });
    worker.addEventListener("error", (event) => resolve({ ready: false, error: event.message || "the GPU worker failed to start" }), {
      once: true,
    });
  });
  if (!ready.ready) worker.terminate();
  return {
    ...ready,
    worker,
    /** A port to the GPU worker, for one proof: send it to the engine's worker as `gpuPort`. */
    port() {
      const channel = new MessageChannel();
      worker.postMessage({ connect: channel.port1 }, [channel.port1]);
      return channel.port2;
    },
    /** Frees what the commitments keep between proofs (not while a proof is attached). */
    release() {
      worker.postMessage({ release: true });
    },
    /**
     * Sets the worker's options again (as `configure` above; between proofs): e.g. keep-warm on
     * or off. The commitment's options not given keep their values; `keep_warm_ms` not given is
     * off (and switching it off stops any warming at once).
     */
    configure(config) {
      worker.postMessage({ configure: config });
    },
    terminate() {
      worker.terminate();
    },
  };
}
