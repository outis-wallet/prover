// Builds the virtual OS input for one transaction with the wasm TxSession. All network access
// goes through `rpc(method, params)`, so the same code runs against a live node (browser, Node)
// or a recording.

const SIMULATION_FLAGS = ["RETURN_INITIAL_READS", "SKIP_FEE_CHARGE"];
const MAX_ROUNDS = 20;

// `onTransaction({block, tx_hash})` reports the base block and the transaction's hash once known;
// `onPhase("execute")` is called before each execution round (the first one ends reading state).
export async function buildOsInput({ TxSession, rpc, chainId, transaction, log = () => {}, onPhase = () => {}, onTransaction = () => {} }) {
  const header = await rpc("starknet_getBlockWithTxHashes", { block_id: "latest" });
  // Pin every later read to this block, so a new block cannot slip in between.
  const block_id = { block_number: header.block_number };
  const session = new TxSession(chainId, JSON.stringify(header), JSON.stringify(transaction));
  log(`block ${header.block_number}, tx ${session.tx_hash()}`);
  onTransaction({ block: header.block_number, tx_hash: session.tx_hash() });

  const simulation = await rpc("starknet_simulateTransactions", {
    block_id,
    transactions: [transaction],
    simulation_flags: SIMULATION_FLAGS,
  });
  const initialReads = simulation.initial_reads;
  session.add_reads(JSON.stringify(initialReads));
  log(`prefetched ${initialReads.storage.length} storage, ${initialReads.nonces.length} nonces, ${initialReads.class_hashes.length} class hashes`);

  const addedClasses = new Set();
  const addClass = async (classHash) => {
    if (addedClasses.has(BigInt(classHash))) return;
    addedClasses.add(BigInt(classHash));
    let casm, sierra;
    try {
      [casm, sierra] = await Promise.all([
        rpc("starknet_getCompiledCasm", { class_hash: classHash }),
        rpc("starknet_getClass", { block_id, class_hash: classHash }),
      ]);
    } catch (error) {
      if (error.code === 28) return session.add_undeclared_class(classHash); // CLASS_HASH_NOT_FOUND
      throw error;
    }
    if (!sierra.sierra_program) throw new Error(`class ${classHash} is Cairo 0, not supported`);
    session.add_class(classHash, JSON.stringify(casm), JSON.stringify(sierra.sierra_program.slice(0, 3)));
  };
  await Promise.all(initialReads.class_hashes.map((entry) => addClass(entry.class_hash)));

  for (let round = 1; round <= MAX_ROUNDS; round++) {
    onPhase("execute");
    const result = JSON.parse(session.execute());
    if (!result.missing) {
      log(`executed in round ${round}; ${result.storage_proof_params.length} storage proof request(s)`);
      const proofs = await Promise.all(result.storage_proof_params.map((params) => rpc("starknet_getStorageProof", params)));
      log("building the virtual OS input from the storage proofs");
      const osInputJson = session.set_storage_proofs(JSON.stringify(proofs));
      return { session, osInputJson, storageProofParams: result.storage_proof_params };
    }
    const { storage, nonces, class_hashes, classes } = result.missing;
    log(`round ${round} missed ${storage.length} storage, ${nonces.length} nonces, ${class_hashes.length} class hashes, ${classes.length} classes`);
    const reads = {
      storage: await Promise.all(storage.map(async ([contract_address, key]) => ({
        contract_address, key, value: await rpc("starknet_getStorageAt", { block_id, contract_address, key }),
      }))),
      nonces: await Promise.all(nonces.map(async (contract_address) => ({
        contract_address, nonce: await rpc("starknet_getNonce", { block_id, contract_address }),
      }))),
      class_hashes: await Promise.all(class_hashes.map(async (contract_address) => ({
        contract_address, class_hash: await rpc("starknet_getClassHashAt", { block_id, contract_address }),
      }))),
    };
    session.add_reads(JSON.stringify(reads));
    await Promise.all([...classes, ...reads.class_hashes.map((entry) => entry.class_hash)].map(addClass));
  }
  throw new Error(`execution still missing reads after ${MAX_ROUNDS} rounds`);
}

/**
 * Serves the calls from a recording made by rpc-record-proxy.py: `recorded` is its rpc-log.jsonl,
 * parsed ({request, response} per line). getCompiledCasm comes from `loadCasm(classHash)` (the
 * capture's casm-<hash>.json results). `onCall` also gets the recorded entry that answered.
 */
export function replayRpc(recorded, loadCasm, onCall = () => {}) {
  const same = (a, b) => BigInt(a) === BigInt(b);
  return async (method, params) => {
    if (method === "starknet_getCompiledCasm") {
      onCall(method, params);
      return loadCasm(params.class_hash);
    }
    const match = recorded.find(({ request }) => {
      if (request.method !== method) return false;
      const p = request.params;
      if (method === "starknet_getStorageAt") return same(p.contract_address, params.contract_address) && same(p.key, params.key);
      if (method === "starknet_getNonce" || method === "starknet_getClassHashAt") return same(p.contract_address, params.contract_address);
      if (method === "starknet_getClass") return same(p.class_hash, params.class_hash);
      return true;
    });
    if (!match) throw new Error(`not in the recording: ${method} ${JSON.stringify(params)}`);
    onCall(method, params, match);
    if (match.response.error) throw Object.assign(new Error(match.response.error.message), match.response.error);
    return match.response.result;
  };
}

/**
 * Compares a prove result with the native prover's `starknet_proveTransaction` result: the proof
 * must be identical; proof_facts and l2_to_l1_messages are compared by value.
 */
export function compareWithNative(result, native) {
  const canonical = (messages) => JSON.stringify(messages.map((m) => [BigInt(m.from_address), BigInt(m.to_address), m.payload.map(BigInt)].map(String)));
  return {
    identical: result.proof === native.proof,
    facts: JSON.stringify(result.proof_facts) === JSON.stringify(native.proof_facts),
    messages: canonical(result.l2_to_l1_messages) === canonical(native.l2_to_l1_messages),
  };
}

/** JSON-RPC over fetch. */
export function liveRpc(url, onCall = () => {}) {
  let id = 0;
  return async (method, params) => {
    const response = await fetch(url, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ jsonrpc: "2.0", id: ++id, method, params }),
    });
    const body = await response.json();
    onCall(method, params, body);
    if (body.error) throw Object.assign(new Error(`${method}: ${body.error.message}`), body.error);
    return body.result;
  };
}
