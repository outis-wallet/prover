// starknet_proveTransaction answered by the wasm build in Node (js/node/, single-threaded), live
// against a node: the same driver and session a wallet's page proves with, so a caller that speaks
// the transaction prover's API (the SDK, the spike) can submit a proof made the client's way.
// The transaction is never sent to the node: its reads are found by running it (placeholders).
//   NODE=http://127.0.0.1:9545/rpc/v0_10 PORT=3013 node js/serve.mjs
import http from "node:http";
import { createRequire } from "node:module";
import { buildOsInput, liveRpc } from "./tx-driver.mjs";

const require = createRequire(import.meta.url);
const { TxSession } = require("./node/os_wasm_probe.js");
const NODE = process.env.NODE ?? "http://127.0.0.1:9545/rpc/v0_10";
const PORT = Number(process.env.PORT ?? 3013);
const CHAIN_ID = process.env.CHAIN_ID ?? "SN_SEPOLIA";
const log = (...parts) => console.log(new Date().toISOString().slice(11, 19), ...parts);

async function prove({ block_id, transaction }) {
  const started = Date.now();
  const blockNumber = block_id?.block_number;
  const { session, rounds } = await buildOsInput({
    TxSession, rpc: liveRpc(NODE), chainId: CHAIN_ID, transaction, blockNumber, log,
  });
  log(`input built in ${((Date.now() - started) / 1000).toFixed(1)} s (${rounds} rounds); ${session.run_os()}`);
  const proving = Date.now();
  const result = JSON.parse(session.prove());
  log(`proved in ${((Date.now() - proving) / 1000).toFixed(1)} s, ${result.proof.length} chars`);
  return result;
}

let busy = false;
http.createServer((request, response) => {
  let body = "";
  request.on("data", (chunk) => (body += chunk));
  request.on("end", async () => {
    let id = null;
    const reply = (message) => {
      const data = JSON.stringify({ jsonrpc: "2.0", id, ...message });
      response.writeHead(200, { "content-type": "application/json", "content-length": Buffer.byteLength(data) });
      response.end(data);
    };
    try {
      const call = JSON.parse(body);
      id = call.id ?? null;
      if (call.method !== "starknet_proveTransaction") return reply({ error: { code: -32601, message: `not served: ${call.method}` } });
      if (busy) return reply({ error: { code: -32000, message: "busy: one proof at a time" } });
      busy = true;
      try {
        reply({ result: await prove(call.params) });
      } finally {
        busy = false;
      }
    } catch (error) {
      log(`failed: ${error.message}`);
      reply({ error: { code: -32603, message: String(error.message ?? error) } });
    }
  });
}).listen(PORT, "127.0.0.1", () => log(`proving with the wasm build on :${PORT}, reading ${NODE}`));
