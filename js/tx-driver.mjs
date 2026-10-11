// Builds the virtual OS input for one transaction with the wasm TxSession, or with any session of
// the same methods (the native one in the apps). All network access goes through
// `rpc(method, params)`, so the same code runs against a live node (browser, Node) or a recording.

const SIMULATION_FLAGS = ["RETURN_INITIAL_READS", "SKIP_FEE_CHARGE"];
// A payment read 26 values one round at a time; with placeholders and the reads it is told about
// first, two or three rounds.
const MAX_ROUNDS = 64;

/**
 * The reads to fetch first, in parallel with the header: `{ storage: [[contract, key]], nonces:
 * [contract], class_hashes: [contract] }`, each as hex. What the caller knows the transaction will
 * read (see the wallet's prediction); a read it gets wrong is only a wasted call.
 */
const NO_READS = { storage: [], nonces: [], class_hashes: [] };

/**
 * Builds the transaction's virtual OS input against `blockNumber` (the latest block when it is
 * not given). Reads go through `rpc`, and only reads: the node is never sent the transaction,
 * whose calldata carries the private inputs, unless `simulate` asks the node for everything the
 * transaction reads (one call, the way the native prover prefetches; a recording of one replays
 * it). Otherwise the transaction runs here, from `first` on: with `placeholders`, a value not
 * fetched yet reads as zero and the run goes on, so a round finds many of the missing reads, and
 * only a run that read nothing missing is used.
 *
 * `onTransaction({block, tx_hash})` reports the base block and the transaction's hash once known;
 * `onPhase("execute")` is called before each execution round (the first one ends reading state).
 *
 * The session is `new TxSession(chainId, headerJson, transactionJson)` (the wasm one), or what
 * `openSession(chainId, headerJson, transactionJson)` resolves to: an object with the wasm
 * session's methods (tx_hash, block_number, set_placeholders, add_reads, add_class,
 * add_undeclared_class, execute, set_storage_proofs, run_os, prove), each taking the same
 * arguments and returning, or resolving to, what the wasm method returns (a string, a number for
 * block_number, nothing for the setters) and throwing, or rejecting, on an error. Every call is
 * awaited, and a call may start before the last one resolved (classes are added in parallel).
 * set_storage_proofs may resolve to null instead of the input's JSON (the native session keeps
 * it): `osInputJson` is then null.
 */
export async function buildOsInput({
  TxSession,
  openSession = async (chainId, headerJson, transactionJson) => new TxSession(chainId, headerJson, transactionJson),
  rpc,
  chainId,
  transaction,
  blockNumber,
  first = NO_READS,
  simulate = false,
  placeholders = true,
  log = () => {},
  onPhase = () => {},
  onTransaction = () => {},
}) {
  const fetchReads = (block_id, { storage, nonces, class_hashes }) =>
    Promise.all([
      Promise.all(storage.map(async ([contract_address, key]) => ({
        contract_address, key, value: await rpc("starknet_getStorageAt", { block_id, contract_address, key }),
      }))),
      Promise.all(nonces.map(async (contract_address) => ({
        contract_address, nonce: await rpc("starknet_getNonce", { block_id, contract_address }),
      }))),
      Promise.all(class_hashes.map(async (contract_address) => ({
        contract_address, class_hash: await rpc("starknet_getClassHashAt", { block_id, contract_address }),
      }))),
    ]).then(([storage, nonces, class_hashes]) => ({ storage, nonces, class_hashes }));

  // Every read is pinned to one block, so a new block cannot slip in between.
  const pinned = blockNumber === undefined ? undefined : { block_number: blockNumber };
  const [header, firstReads] = await Promise.all([
    rpc("starknet_getBlockWithTxHashes", { block_id: pinned ?? "latest" }),
    pinned ? fetchReads(pinned, first) : undefined,
  ]);
  const block_id = { block_number: header.block_number };
  const session = await openSession(chainId, JSON.stringify(header), JSON.stringify(transaction));
  if (placeholders) await session.set_placeholders(true);
  const tx_hash = await session.tx_hash();
  log(`block ${header.block_number}, tx ${tx_hash}`);
  onTransaction({ block: header.block_number, tx_hash });

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
      if (error.code === 28) return await session.add_undeclared_class(classHash); // CLASS_HASH_NOT_FOUND
      throw error;
    }
    if (!sierra.sierra_program) throw new Error(`class ${classHash} is Cairo 0, not supported`);
    // Only the first three felts are read: the Sierra version.
    await session.add_class(classHash, JSON.stringify(casm), JSON.stringify(sierra.sierra_program.slice(0, 3)));
  };
  const addReads = async (reads) => {
    await session.add_reads(JSON.stringify(reads));
    await Promise.all(reads.class_hashes.map((entry) => addClass(entry.class_hash)));
  };

  if (simulate) {
    const simulation = await rpc("starknet_simulateTransactions", {
      block_id,
      transactions: [transaction],
      simulation_flags: SIMULATION_FLAGS,
    });
    const initialReads = simulation.initial_reads;
    log(`prefetched ${initialReads.storage.length} storage, ${initialReads.nonces.length} nonces, ${initialReads.class_hashes.length} class hashes`);
    await addReads(initialReads);
  }
  await addReads(firstReads ?? (await fetchReads(block_id, first)));

  for (let round = 1; round <= MAX_ROUNDS; round++) {
    onPhase("execute");
    const result = JSON.parse(await session.execute());
    if (!result.missing) {
      log(`executed in round ${round}; ${result.storage_proof_params.length} storage proof request(s)`);
      const proofs = await Promise.all(result.storage_proof_params.map((params) => rpc("starknet_getStorageProof", params)));
      log("building the virtual OS input from the storage proofs");
      const osInputJson = (await session.set_storage_proofs(JSON.stringify(proofs))) ?? null;
      return { session, osInputJson, storageProofParams: result.storage_proof_params, rounds: round };
    }
    const { storage, nonces, class_hashes, classes } = result.missing;
    log(`round ${round} missed ${storage.length} storage, ${nonces.length} nonces, ${class_hashes.length} class hashes, ${classes.length} classes`);
    const reads = await fetchReads(block_id, { storage, nonces, class_hashes });
    await session.add_reads(JSON.stringify(reads));
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
