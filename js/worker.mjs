// The web engine's worker: proves one bundled capture with the pipeline compiled to WebAssembly,
// and reports like os_wasm_probe::replay does in the apps (Progress events, then a Report).
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

self.onmessage = async ({ data: { action, threads } }) => {
  const event = (payload) => self.postMessage({ event: payload });
  const since = (start) => (performance.now() - start) / 1000;
  try {
    const started = performance.now();
    event({ event: "phase", phase: "state" });
    const wasm = await pkg.default({ module_or_path: await wasmBytes(WASM) });
    self.postMessage({ memory: wasm.memory });
    await pkg.initThreadPool(threads);
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
        total_s: since(started),
        os_steps: osSteps,
        verified,
        verify_error: verifyError,
        identical: identical && facts && messages,
        proof_bytes: Math.floor((result.proof.length * 3) / 4),
        // The wasm memory only grows, so its size at the end is the run's peak.
        peak: { run_mib: Number(memoryParts[1] || 0), live_peak_mib: Number(memoryParts[3] || 0) },
      },
    });
  } catch (error) {
    self.postMessage({ error: String(error && error.stack ? error.stack : error) });
  }
};
