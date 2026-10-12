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

/**
 * Starts the GPU worker and resolves when it has a device (or failed to get one). `configure`
 * goes to the worker first: transfer sizes in bytes (`upload_chunk`, `upload_window`,
 * `readback_chunk`), `kick_ms` (Firefox's poll kicks), `compute_twiddles` (compute the
 * commitments' twiddles in the worker rather than read the prover's). A worker with no device
 * after `timeoutMs` counts as failed (a wedged driver can leave requestAdapter pending).
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
    terminate() {
      worker.terminate();
    },
  };
}
