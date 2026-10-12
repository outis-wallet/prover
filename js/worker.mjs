// The web engine's worker. It proves one bundled capture with the pipeline compiled to WebAssembly,
// and reports like os_wasm_probe::replay does in the apps (Progress events, then a Report); or, live,
// a wallet's transaction, reading the chain through the page (Progress events, then the Result).
// scripts/build-web-engine.sh puts this file at engine/worker.mjs and fills in WASM.
import { buildOsInput, compareWithNative, replayRpc } from "./tx-driver.mjs";
import * as pkg from "../pkg-mt/os_wasm_probe.js";

const WASM = "../pkg-mt/__WASM_FILE__";
// The prover build the scores belong to: the module's hash.
const BUILD = `web-${(WASM.match(/bg\.([0-9a-f]+)\.wasm/) || [])[1] || "unknown"}`;
const CAPTURES = "../captures/";

const fetchOk = async (url) => {
  const response = await fetch(url);
  if (!response.ok) throw new Error(`${url}: HTTP ${response.status}`);
  return response;
};
const fetchJson = async (url) => (await fetchOk(url)).json();

// The module is shipped gzipped (wasm isn't compressed by every host). A host may still have
// sent it with Content-Encoding, in which case the browser has already inflated it.
async function wasmBytes(url) {
  const bytes = new Uint8Array(await (await fetchOk(url)).arrayBuffer());
  if (bytes[0] !== 0x1f || bytes[1] !== 0x8b) return bytes;
  const inflated = new Blob([bytes]).stream().pipeThrough(new DecompressionStream("gzip"));
  return new Uint8Array(await new Response(inflated).arrayBuffer());
}

// The workers this one starts: wasm-bindgen-rayon's thread pool and the memory watcher. Safari can't
// end a worker parked in a wasm atomic wait, where the pool's threads wait for work: ending this
// worker (or the page's reload) left them, and the run's memory they share (2+ GiB), for as long as
// the tab was open, and the next proof in the tab failed. So when a run is over, stop_threads traps
// the pool's threads out of wasm, and their workers are ended here, before the outcome is posted
// (the page ends this worker when it gets it).
const children = [];
self.Worker = class extends Worker {
  constructor(...args) {
    super(...args);
    children.push(this);
  }
};
async function endChildren() {
  try {
    pkg.stop_threads();
    // Time for the threads to wake and leave wasm before their workers are ended.
    await new Promise((resolve) => setTimeout(resolve, 100));
  } catch {}
  children.splice(0).forEach((worker) => worker.terminate());
}

// The page's GPU worker (js/gpu.mjs; builds with feature gpu): the page sends a port to it as
// gpuPort when it wants the striped commitments on the GPU. It attaches to this module's memory
// while this worker's event loop still runs (the proof then calls it synchronously from the
// pool's threads), and detaches before the threads stop. Without it, or if it can't attach,
// every commitment runs on the CPU, as without the feature.
let gpuPort = null;

/**
 * Posts `message` on `port` and resolves with the reply, or with `late` if none comes within
 * `ms` (a GPU worker the page ended, or one stuck in a driver call, never answers).
 */
const request = (port, message, ms, late) =>
  new Promise((resolve) => {
    const timer = setTimeout(() => resolve(late), ms);
    port.onmessage = ({ data }) => {
      clearTimeout(timer);
      resolve(data);
    };
    port.postMessage(message);
  });

/**
 * Attaches the GPU worker behind `port` to `memory`; returns what the page's report shows. A
 * commitment it hasn't finished after `deadlineMs` (default 60 s) goes to the CPU, as does every
 * later one.
 */
async function attachGpu(port, memory, deadlineMs = 0) {
  if (!port) return null;
  if (typeof pkg.gpu_bridge_region !== "function") {
    port.close();
    return { attached: false, error: "this build has no GPU commitment (feature gpu)" };
  }
  const reply = await request(port, { attach: { memory, base: pkg.gpu_bridge_region() } }, 10000, {
    attached: false,
    error: "the GPU worker didn't answer",
  });
  if (!reply.attached) {
    port.close();
    return { attached: false, error: reply.error };
  }
  gpuPort = port;
  pkg.gpu_enable(deadlineMs);
  return { attached: true };
}

/** The commitments the GPU hook saw (see gpu.rs); a report it can't parse doesn't fail the proof. */
function gpuCommitments() {
  try {
    return JSON.parse(pkg.gpu_report());
  } catch (error) {
    return [{ report_error: String(error) }];
  }
}

/** Detaches the GPU worker (before the threads stop); returns the commitments it served. */
async function detachGpu() {
  if (!gpuPort) return null;
  const port = gpuPort;
  gpuPort = null;
  pkg.gpu_disable();
  // No job is running (the proof is over), so the reply is immediate unless the worker is gone.
  const reply = await request(port, { detach: true }, 10000, { lost: true });
  port.close();
  return { lost: !!reply.lost, commitments: gpuCommitments() };
}

// The page's memory figure. This worker is inside long synchronous calls (the OS run, the proof),
// so a worker of its own reads the shared wasm memory's size every 250 ms and posts it, in MiB, on
// the port the page sent. The page never holds the memory: its reference would keep it until the
// page's garbage collector ran.
const WATCHER = `onmessage = ({ data: { memory, port } }) => {
  postMessage("ready");
  setInterval(() => port.postMessage(Math.round(memory.buffer.byteLength / 2 ** 20)), 250);
};`;

function watchMemory(memory, port) {
  const watcher = new Worker(URL.createObjectURL(new Blob([WATCHER], { type: "text/javascript" })));
  watcher.postMessage({ memory, port }, [port]);
  // Running before this worker blocks: a worker may not start while its parent is busy.
  return new Promise((resolve) => { watcher.onmessage = watcher.onerror = resolve; });
}

// The prover's panic message: its panic hook writes it to console.error just before the trap,
// whose own message is only "unreachable". Kept to send with the error.
let panicMessage = "";
const consoleError = console.error.bind(console);
console.error = (...args) => {
  panicMessage = args.join(" ");
  consoleError(...args);
};

/**
 * The page's node, through `port`: each call is posted as `{ id, method, params }` and answered with
 * `{ id, result }` or `{ id, error: { code, message } }`. The page decides where the node is and what
 * it serves from its own cache (the classes).
 */
function portRpc(port) {
  let next = 0;
  const pending = new Map();
  port.onmessage = ({ data: { id, result, error } }) => {
    const call = pending.get(id);
    if (!call) return;
    pending.delete(id);
    if (error) call.reject(Object.assign(new Error(error.message), { code: error.code }));
    else call.resolve(result);
  };
  return (method, params) =>
    new Promise((resolve, reject) => {
      const id = next++;
      pending.set(id, { resolve, reject });
      port.postMessage({ id, method, params });
    });
}

/**
 * Proves a wallet's transaction: `{ transaction, chainId, blockNumber, first }` (see buildOsInput),
 * with the chain read through `rpcPort`. Posts Progress events and then `{ result }`: the proof, its
 * facts and its messages, as starknet_proveTransaction answers, after checking it with the verifier,
 * and how long each step took.
 */
async function proveLive({ transaction, chainId, blockNumber, first }, threads, rpcPort, port, gpuDeadlineMs, event) {
  const since = (start) => (performance.now() - start) / 1000;
  const started = performance.now();
  event({ event: "phase", phase: "load" });
  const wasm = await pkg.default({ module_or_path: await wasmBytes(WASM) });
  await pkg.initThreadPool(threads);
  const gpu = await attachGpu(port, wasm.memory, gpuDeadlineMs);
  if (gpu) event({ event: "gpu", ...gpu });
  const loadS = since(started);

  const inputStarted = performance.now();
  event({ event: "phase", phase: "state" });
  const { session, rounds } = await buildOsInput({
    TxSession: pkg.TxSession,
    rpc: portRpc(rpcPort),
    chainId,
    transaction,
    blockNumber,
    first,
    onTransaction: ({ block, tx_hash }) => event({ event: "transaction", block, tx_hash }),
  });
  const inputS = since(inputStarted);

  event({ event: "phase", phase: "os" });
  let phaseStarted = performance.now();
  session.run_os();
  const osS = since(phaseStarted);

  event({ event: "phase", phase: "prove" });
  phaseStarted = performance.now();
  const resultJson = session.prove();
  const proveS = since(phaseStarted);

  event({ event: "phase", phase: "verify" });
  phaseStarted = performance.now();
  pkg.verify_prove_result(resultJson);
  const verifyS = since(phaseStarted);
  const memoryParts = pkg.memory_report().match(/wasm (\d+) live (\d+) peak (\d+)/) || [];
  const gpuReport = gpu && { ...gpu, ...(await detachGpu()) };
  return {
    ...JSON.parse(resultJson),
    report: {
      build: BUILD,
      threads,
      rounds,
      load_s: loadS,
      input_s: inputS,
      os_s: osS,
      prove_s: proveS,
      verify_s: verifyS,
      total_s: since(started),
      peak: { run_mib: Number(memoryParts[1] || 0), live_peak_mib: Number(memoryParts[3] || 0) },
      gpu: gpuReport,
    },
  };
}

/** Posts `{ error }`, once the threads are stopped. */
async function failed(error) {
  // Safari's stack has no message line (Chrome's starts with it): send the message too. And a
  // panic's message, and the memory then (an allocation that fails aborts without a message).
  const message = String(error);
  const stack = error && error.stack ? String(error.stack) : "";
  let memory = "";
  try {
    memory = `memory: ${pkg.memory_report()} MiB`;
  } catch {}
  const lines = [panicMessage && `panic: ${panicMessage}`, memory, stack.includes(message) ? stack : [message, stack].filter(Boolean).join("\n")];
  try {
    await detachGpu();
  } catch {}
  await endChildren();
  self.postMessage({ error: lines.filter(Boolean).join("\n") });
}

self.onmessage = async ({ data: { action, live, threads, memoryPort, rpcPort, gpuPort: port, gpuDeadlineMs, cpuTimes } }) => {
  const event = (payload) => self.postMessage({ event: payload });
  if (live) {
    try {
      const result = await proveLive(live, threads, rpcPort, port, gpuDeadlineMs, event);
      await endChildren();
      self.postMessage({ result });
    } catch (error) {
      await failed(error);
    }
    return;
  }
  const since = (start) => (performance.now() - start) / 1000;
  try {
    const started = performance.now();
    event({ event: "phase", phase: "state" });
    const wasm = await pkg.default({ module_or_path: await wasmBytes(WASM) });
    if (memoryPort) await watchMemory(wasm.memory, memoryPort);
    await pkg.initThreadPool(threads);
    let gpu = await attachGpu(port, wasm.memory, gpuDeadlineMs);
    if (gpu) event({ event: "gpu", ...gpu });
    // A bench's reference for the GPU's times: the CPU's commitments timed (feature gpu builds).
    if (cpuTimes && !gpu && typeof pkg.gpu_time_cpu_commitments === "function" && pkg.gpu_time_cpu_commitments()) {
      gpu = { attached: false, cpu_times: true };
    }
    const loadS = since(started);

    const inputStarted = performance.now();
    const capture = `${CAPTURES}${action}/`;
    const recorded = (await (await fetchOk(`${capture}rpc-log.jsonl`)).text()).trim().split("\n").map((line) => JSON.parse(line));
    const loadCasm = async (classHash) => (await fetchJson(`${CAPTURES}classes/casm-${classHash}.json`)).result;
    const rpc = replayRpc(recorded, loadCasm);
    const transaction = (await fetchJson(`${capture}request.json`)).params.transaction;
    let rounds = 0;
    const { session } = await buildOsInput({
      TxSession: pkg.TxSession,
      rpc,
      chainId: "SN_SEPOLIA",
      transaction,
      // The recordings were made by a prover that prefetches with a simulation.
      simulate: true,
      placeholders: false,
      onTransaction: ({ block, tx_hash }) => event({ event: "transaction", block, tx_hash }),
      onPhase: () => {
        if (rounds++ === 0) event({ event: "phase", phase: "execute" });
      },
    });
    const inputS = since(inputStarted);

    event({ event: "phase", phase: "os" });
    let phaseStarted = performance.now();
    const pie = session.run_os();
    const osS = since(phaseStarted);
    const osSteps = Number((pie.match(/n_steps=(\d+)/) || [])[1] || 0);
    event({ event: "os_steps", steps: osSteps });

    event({ event: "phase", phase: "prove" });
    phaseStarted = performance.now();
    const resultJson = session.prove();
    const proveS = since(phaseStarted);
    const memoryParts = pkg.memory_report().match(/wasm (\d+) live (\d+) peak (\d+)/) || [];

    event({ event: "phase", phase: "verify" });
    phaseStarted = performance.now();
    let verified = true;
    let verifyError = null;
    try {
      pkg.verify_prove_result(resultJson);
    } catch (error) {
      verified = false;
      verifyError = String(error && error.message ? error.message : error);
    }
    const verifyS = since(phaseStarted);

    const result = JSON.parse(resultJson);
    const native = (await fetchJson(`${capture}response.json`)).result;
    const { identical, facts, messages } = compareWithNative(result, native);
    const totalS = since(started);
    const gpuReport = gpu && { ...gpu, ...(gpu.cpu_times ? { commitments: gpuCommitments() } : await detachGpu()) };
    await endChildren();
    self.postMessage({
      report: {
        build: BUILD,
        block: Number(session.block_number()),
        tx_hash: session.tx_hash(),
        threads,
        rounds,
        load_s: loadS,
        input_s: inputS,
        os_s: osS,
        prove_s: proveS,
        verify_s: verifyS,
        total_s: totalS,
        os_steps: osSteps,
        verified,
        verify_error: verifyError,
        identical: identical && facts && messages,
        proof_bytes: Math.floor((result.proof.length * 3) / 4),
        // The wasm memory only grows, so its size at the end is the run's peak.
        peak: { run_mib: Number(memoryParts[1] || 0), live_peak_mib: Number(memoryParts[3] || 0) },
        gpu: gpuReport,
      },
    });
  } catch (error) {
    await failed(error);
  }
};
