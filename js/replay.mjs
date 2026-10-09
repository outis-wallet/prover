// Proves a capture with the wasm build, in Node (scripts/build-wasm.sh node first):
//   node js/replay.mjs captures/pay --prove [--trace]
// It serves the recorded RPC answers, builds the virtual OS input, runs the OS and, with --prove,
// proves it and compares the proof with the capture's native one.
import fs from "node:fs";
import path from "node:path";
import { createRequire } from "node:module";
import { buildOsInput, compareWithNative, replayRpc } from "./tx-driver.mjs";

const require = createRequire(import.meta.url);
const probe = require("./node/os_wasm_probe.js"); // scripts/build-wasm.sh node
const { TxSession } = probe;
if (process.argv.includes("--trace")) probe.init_logging("info");
const memory = (stage) => console.log(`MEM ${stage}: ${probe.memory_report()}`);

const capture = process.argv[2];
// Outputs go to OUT, default ./replay-out (the capture itself is kept read-only).
const out = process.env.OUT ?? "replay-out";
fs.mkdirSync(out, { recursive: true });
const recorded = fs.readFileSync(path.join(capture, "rpc-log.jsonl"), "utf8").trim().split("\n").map((line) => JSON.parse(line));
const transaction = JSON.parse(fs.readFileSync(path.join(capture, fs.readdirSync(capture).find((f) => f.startsWith("request-"))))).params.transaction;

const served = [];
const rpc = replayRpc(
  recorded,
  (classHash) => JSON.parse(fs.readFileSync(path.join(capture, `casm-${classHash}.json`))).result,
  (method, params, match) => {
    served.push(method);
    if (method === "starknet_getStorageProof") compareProofParams(match.request.params, params);
  },
);

function compareProofParams(recordedParams, ours) {
  const norm = (p) => JSON.stringify({
    block: p.block_id.block_number,
    classes: p.class_hashes.map(BigInt).sort().map(String),
    contracts: p.contract_addresses.map(BigInt).sort().map(String),
    keys: p.contracts_storage_keys.map((c) => [String(BigInt(c.contract_address)), c.storage_keys.map(BigInt).sort().map(String)]).sort(),
  });
  console.log(`storage proof params ${norm(recordedParams) === norm(ours) ? "match" : "DIFFER"} the recording`);
}

const started = Date.now();
const { session, osInputJson } = await buildOsInput({ TxSession, rpc, chainId: "SN_SEPOLIA", transaction, log: console.log });
const built = Date.now();
memory("input built");
fs.writeFileSync(path.join(out, "virtual-os-input.wasm.json"), osInputJson);
console.log(`input built in ${((built - started) / 1000).toFixed(2)} s from ${served.length} calls: ${[...new Set(served)].join(", ")}`);
console.log(`${session.run_os()} in ${((Date.now() - built) / 1000).toFixed(2)} s`);
memory("virtual OS done");

if (process.argv.includes("--prove")) {
  const proveStarted = Date.now();
  const result = JSON.parse(session.prove());
  memory("proved");
  const seconds = ((Date.now() - proveStarted) / 1000).toFixed(1);
  fs.writeFileSync(path.join(out, "response.wasm.json"), JSON.stringify(result));
  const native = JSON.parse(fs.readFileSync(path.join(capture, fs.readdirSync(capture).find((f) => f.startsWith("response-"))))).result;
  const { identical, facts, messages } = compareWithNative(result, native);
  console.log(`proved in ${seconds} s: proof ${result.proof.length} chars, ${identical ? "IDENTICAL" : "DIFFERENT"} to the native proof; proof_facts ${facts ? "match" : "DIFFER"}; l2_to_l1_messages ${messages ? "match" : "DIFFER"}; wasm memory ${(process.memoryUsage().rss / 2 ** 30).toFixed(2)} GiB rss`);
}
