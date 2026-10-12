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
 * Starts the GPU worker and resolves when it has a device (or failed to get one). `configure` goes
 * to the worker first (each key optional): `kick_ms` (Firefox's poll kicks), and the commitments'
 * `compute_twiddles` (compute the twiddles in the worker rather than read the prover's),
 * `commit_chunk` (its transfer rings' chunk in bytes, 32 MiB), `upload_ring` and `readback_ring`
 * (their depths, 3 and 3), `max_gpu_mib` (a commitment needing more video memory goes to the CPU),
 * `max_plans` (the trees whose plans are kept, by shape, 16), `keep` (false: every commitment frees
 * what it used), `streamed` (false: stripe by stripe, without overlapping its transfers), `upload`
 * ("write": writeBuffer instead of the ring), `serial` and `timestamps` (diagnostics: each phase
 * timed alone; the GPU's own time). A worker with no device after `timeoutMs` counts as failed (a
 * wedged driver can leave requestAdapter pending).
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
    terminate() {
      worker.terminate();
    },
  };
}
