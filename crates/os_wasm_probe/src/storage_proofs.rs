// The network-free parts of starknet_transaction_prover/src/running/storage_proofs.rs
// (PRIVACY-0.14.4-RC.0): query construction, proof merging, and commitment infos. The caller
// fetches `starknet_getStorageProof` itself.

use std::collections::{HashMap, HashSet};

use blockifier::state::accessed_keys::AccessedKeys;
use blockifier::state::cached_state::StateMaps;
use indexmap::IndexMap;
use starknet_api::block::BlockNumber;
use starknet_api::core::{ClassHash, ContractAddress, Nonce};
use starknet_api::hash::{HashOutput, StateRoots};
use starknet_os::commitment_infos::{build_state_commitment_infos, StateCommitmentInfos};
use starknet_patricia::patricia_merkle_tree::node_data::inner_node::{
    flatten_preimages,
    Preimage,
    PreimageMap,
};
use starknet_patricia_storage::map_storage::MapStorage;
use starknet_rust_core::types::{
    ContractStorageKeys,
    ContractsProof,
    Felt,
    MerkleNode,
    StorageProof as RpcStorageProof,
};

use crate::committer_utils::{
    commit_state_diff,
    create_facts_db_from_storage_proof,
    state_maps_to_committer_state_diff,
    validate_virtual_os_state_diff,
};
use crate::errors::ProofProviderError;

const MAX_KEYS_PER_REQUEST: usize = 100;

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct RpcStorageProofsQuery {
    pub(crate) class_hashes: Vec<Felt>,
    pub(crate) contract_addresses: Vec<ContractAddress>,
    pub(crate) contract_storage_keys: Vec<ContractStorageKeys>,
}

pub(crate) struct StorageProofs {
    pub(crate) extended_initial_reads: StateMaps,
    pub(crate) commitment_infos: StateCommitmentInfos,
}

fn count_total_keys(query: &RpcStorageProofsQuery) -> usize {
    query.class_hashes.len()
        + query.contract_addresses.len()
        + query.contract_storage_keys.iter().map(|csk| csk.storage_keys.len()).sum::<usize>()
}

enum QueryItem {
    ClassHash(Felt),
    ContractAddress(ContractAddress),
    StorageKey { contract_address: Felt, key: Felt },
}

fn flatten_query(query: &RpcStorageProofsQuery) -> Vec<QueryItem> {
    let mut items = Vec::with_capacity(count_total_keys(query));
    items.extend(query.class_hashes.iter().copied().map(QueryItem::ClassHash));
    items.extend(query.contract_addresses.iter().copied().map(QueryItem::ContractAddress));
    for contract in &query.contract_storage_keys {
        for &key in &contract.storage_keys {
            items.push(QueryItem::StorageKey { contract_address: contract.contract_address, key });
        }
    }
    items
}

fn collect_query(items: &[QueryItem]) -> RpcStorageProofsQuery {
    let mut query = RpcStorageProofsQuery::default();
    for item in items {
        match item {
            QueryItem::ClassHash(h) => query.class_hashes.push(*h),
            QueryItem::ContractAddress(a) => query.contract_addresses.push(*a),
            QueryItem::StorageKey { contract_address, key } => {
                match query.contract_storage_keys.last_mut() {
                    Some(last) if last.contract_address == *contract_address => {
                        last.storage_keys.push(*key);
                    }
                    _ => {
                        query.contract_storage_keys.push(ContractStorageKeys {
                            contract_address: *contract_address,
                            storage_keys: vec![*key],
                        });
                    }
                }
            }
        }
    }
    query
}

/// The queries to send, at most MAX_KEYS_PER_REQUEST keys each, as the native prover splits them.
pub(crate) fn split_query(query: &RpcStorageProofsQuery) -> Vec<RpcStorageProofsQuery> {
    if count_total_keys(query) <= MAX_KEYS_PER_REQUEST {
        return vec![query.clone()];
    }
    flatten_query(query).chunks(MAX_KEYS_PER_REQUEST).map(collect_query).collect()
}

/// `starknet_getStorageProof` params for one query, as starknet_rust sends them.
pub(crate) fn request_params(
    block_number: BlockNumber,
    query: &RpcStorageProofsQuery,
) -> serde_json::Value {
    let contract_addresses: Vec<Felt> =
        query.contract_addresses.iter().map(|a| *a.0.key()).collect();
    serde_json::json!({
        "block_id": { "block_number": block_number.0 },
        "class_hashes": query.class_hashes,
        "contract_addresses": contract_addresses,
        "contracts_storage_keys": query.contract_storage_keys,
    })
}

pub(crate) fn merge_storage_proofs(
    proofs: Vec<RpcStorageProof>,
    split_queries: &[RpcStorageProofsQuery],
    original_query: &RpcStorageProofsQuery,
) -> RpcStorageProof {
    assert_eq!(proofs.len(), split_queries.len(), "proofs/queries length mismatch");
    assert!(!proofs.is_empty(), "cannot merge zero proofs");

    let global_roots = proofs[0].global_roots.clone();
    let mut classes_proof: IndexMap<Felt, MerkleNode> = IndexMap::default();
    let mut contracts_proof =
        ContractsProof { nodes: IndexMap::default(), contract_leaves_data: Vec::new() };

    let addr_to_idx: HashMap<Felt, usize> = original_query
        .contract_storage_keys
        .iter()
        .enumerate()
        .map(|(i, csk)| (csk.contract_address, i))
        .collect();
    let mut contracts_storage_proofs: Vec<IndexMap<Felt, MerkleNode>> =
        vec![IndexMap::default(); original_query.contract_storage_keys.len()];

    for (chunk_query, proof) in split_queries.iter().zip(proofs) {
        classes_proof.extend(proof.classes_proof);
        contracts_proof.nodes.extend(proof.contracts_proof.nodes);
        contracts_proof.contract_leaves_data.extend(proof.contracts_proof.contract_leaves_data);
        for (csk, storage_proof) in
            chunk_query.contract_storage_keys.iter().zip(proof.contracts_storage_proofs)
        {
            let idx = addr_to_idx
                .get(&csk.contract_address)
                .expect("chunk address not in original query");
            contracts_storage_proofs[*idx].extend(storage_proof);
        }
    }

    RpcStorageProof { classes_proof, contracts_proof, contracts_storage_proofs, global_roots }
}

fn rpc_nodes_to_preimage_map<S: std::hash::BuildHasher>(
    nodes: &indexmap::IndexMap<Felt, starknet_rust_core::types::MerkleNode, S>,
) -> PreimageMap {
    nodes.iter().map(|(hash, node)| (HashOutput(*hash), Preimage::from(node))).collect()
}

pub(crate) fn prepare_query(
    executed_class_hashes: &HashSet<ClassHash>,
    initial_reads: &StateMaps,
) -> RpcStorageProofsQuery {
    let class_hashes: Vec<Felt> = executed_class_hashes.iter().map(|ch| ch.0).collect();

    // Sort contract addresses for deterministic ordering (for offline replay mode).
    let mut contract_addresses: Vec<ContractAddress> =
        initial_reads.get_contract_addresses().into_iter().collect();
    contract_addresses.sort();

    // Group storage keys by address, then map over all contract_addresses (which may include
    // addresses with no storage reads) to build the output.
    let mut storage_by_address: HashMap<ContractAddress, Vec<Felt>> = HashMap::new();
    for (address, key) in initial_reads.storage.keys() {
        storage_by_address.entry(*address).or_default().push(*key.0.key());
    }
    let contract_storage_keys: Vec<ContractStorageKeys> = contract_addresses
        .iter()
        .map(|address| ContractStorageKeys {
            contract_address: *address.0.key(),
            storage_keys: storage_by_address.get(address).cloned().unwrap_or_default(),
        })
        .collect();

    RpcStorageProofsQuery { class_hashes, contract_addresses, contract_storage_keys }
}

async fn create_commitment_infos_with_state_changes(
    rpc_proof: &RpcStorageProof,
    query: &RpcStorageProofsQuery,
    extended_initial_reads: &StateMaps,
    state_diff: &StateMaps,
) -> Result<StateCommitmentInfos, ProofProviderError> {
    // Build FactsDb from RPC proofs and execution initial reads.
    let mut facts_db =
        create_facts_db_from_storage_proof(rpc_proof, query, extended_initial_reads)?;

    // Get initial state roots from RPC proof.
    let contracts_trie_root_hash = HashOutput(rpc_proof.global_roots.contracts_tree_root);
    let classes_trie_root_hash = HashOutput(rpc_proof.global_roots.classes_tree_root);
    // Convert the blockifier state maps to committer state diff and validate is stands with
    // the virtual OS assumptions.
    let committer_state_diff = state_maps_to_committer_state_diff(state_diff.clone());
    validate_virtual_os_state_diff(&committer_state_diff)?;

    // Commit state diff using the committer.
    let new_roots = commit_state_diff(
        &mut facts_db,
        contracts_trie_root_hash,
        classes_trie_root_hash,
        committer_state_diff,
    )
    .await?;

    let previous_state_roots = StateRoots { contracts_trie_root_hash, classes_trie_root_hash };

    // Consume the new facts from the committer storage.
    let mut map_storage: MapStorage = facts_db.consume_storage();

    // Get extended initial reads keys.
    let accessed_keys: AccessedKeys = extended_initial_reads.keys().into();

    let mut commitment_infos = build_state_commitment_infos(
        &previous_state_roots,
        &new_roots,
        &mut map_storage,
        &accessed_keys,
    )
    .await
    .map_err(|e| ProofProviderError::BlockCommitmentError(e.to_string()))?;

    // The created commitment infos doesn't have the compiled class hashes,
    // as a result it doesn't have the classes trie commitment info.
    // We complement it with the RPC proof facts.
    let classes_rpc_facts = flatten_preimages(&rpc_nodes_to_preimage_map(&rpc_proof.classes_proof));
    commitment_infos.classes_trie_commitment_info.commitment_facts.extend(classes_rpc_facts);

    Ok(commitment_infos)
}

/// The body of `RpcStorageProofsProvider::get_storage_proofs` after the fetch (state changes
/// included, the prover's default).
pub(crate) async fn storage_proofs_from_rpc_proof(
    rpc_proof: &RpcStorageProof,
    query: &RpcStorageProofsQuery,
    execution_initial_reads: &StateMaps,
    state_diff: &StateMaps,
) -> Result<StorageProofs, ProofProviderError> {
    // Validate that contract_leaves_data matches contract_addresses length.
    let leaves_len = rpc_proof.contracts_proof.contract_leaves_data.len();
    let addresses_len = query.contract_addresses.len();
    if leaves_len != addresses_len {
        return Err(ProofProviderError::InvalidProofResponse(format!(
            "Contract leaves length mismatch: expected {addresses_len} leaves for requested \
             contracts, got {leaves_len}"
        )));
    }

    // Update initial reads with class hashes and nonces from the proof.
    let mut extended_initial_reads = StateMaps::default();
    for (leaf, addr) in
        rpc_proof.contracts_proof.contract_leaves_data.iter().zip(&query.contract_addresses)
    {
        extended_initial_reads.class_hashes.insert(*addr, ClassHash(leaf.class_hash));
        extended_initial_reads.nonces.insert(*addr, Nonce(leaf.nonce));
    }

    // Include storage values from execution.
    extended_initial_reads.storage.extend(&execution_initial_reads.storage);

    let commitment_infos = create_commitment_infos_with_state_changes(
        rpc_proof,
        query,
        &extended_initial_reads,
        state_diff,
    )
    .await?;

    Ok(StorageProofs { extended_initial_reads, commitment_infos })
}
