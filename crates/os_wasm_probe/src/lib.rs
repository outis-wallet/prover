//! The transaction prover's pipeline without its network layer, for wasm: the caller fetches
//! state, classes and storage proofs (JSON-RPC) and hands them in; this crate executes the
//! transaction, builds the virtual OS input and runs the virtual OS.

mod committer_utils;
mod errors;
mod executor;
mod memory;
#[cfg(not(target_arch = "wasm32"))]
pub mod replay;
mod state_reader;
mod storage_proofs;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex};

use blockifier::context::{ChainInfo, FeeTokenAddresses};
use blockifier::execution::contract_class::{
    program_hints_to_casm_hints,
    CompiledClassV1,
    RunnableCompiledClass,
};
use blockifier::state::cached_state::StateMaps;
use blockifier::state::contract_class_manager::ContractClassManager;
use blockifier::state::global_cache::CompiledClasses;
use blockifier::state::state_api::StateReader;
use blockifier::state::utils::get_compiled_class_hash_v2;
use cairo_lang_starknet_classes::casm_contract_class::CasmContractClass;
use cairo_lang_utils::bigint::BigUintAsHex;
use cairo_vm::types::relocatable::MaybeRelocatable;
use cairo_vm::vm::runners::cairo_pie::CairoPie;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use shared_execution_objects::central_objects::CentralTransactionExecutionInfo;
use starknet_api::block::{BlockHash, BlockInfo, GasPrice};
use starknet_api::block_hash::block_hash_calculator::BlockHeaderCommitments;
use starknet_api::contract_class::SierraVersion;
use starknet_api::core::{ChainId, ClassHash, CompiledClassHash, ContractAddress, Nonce, OsChainInfo};
use starknet_api::executable_transaction::{AccountTransaction, Transaction};
use starknet_api::rpc_transaction::{RpcInvokeTransaction, RpcTransaction};
use starknet_api::state::{SierraContractClass, StorageKey};
use starknet_api::transaction::fields::{Proof, ProofFacts, Tip, VIRTUAL_SNOS};
use starknet_api::transaction::{InvokeTransaction, MessageToL1, TransactionHash, TransactionHasher};
use starknet_os::commitment_infos::CommitmentInfo;
use starknet_proof_verifier::ProgramOutput;
use starknet_os::io::os_input::{OsBlockInput, OsHints, OsHintsConfig, StarknetOsInput};
use starknet_rust_core::types::StorageProof as RpcStorageProof;
use starknet_types_core::felt::Felt;
use wasm_bindgen::prelude::*;
#[cfg(all(target_arch = "wasm32", feature = "threads"))]
pub use wasm_bindgen_rayon::init_thread_pool;

/// Ends the thread pool's threads, each by trapping out of wasm into its worker's JavaScript. Safari
/// can't end a worker parked in a wasm atomic wait, where the pool's threads wait for work, so their
/// workers (and the run's memory, which they share) outlived the page's `terminate()`. After this
/// the pool is gone: call it once the last proof of this instance is done.
#[cfg(all(target_arch = "wasm32", feature = "threads"))]
#[wasm_bindgen]
pub fn stop_threads() {
    rayon::spawn_broadcast(|_| std::process::abort());
}

use crate::executor::{BaseBlockInfo, BlockHeader, VirtualBlockExecutionData};
use crate::state_reader::PrefetchedState;
use crate::storage_proofs::RpcStorageProofsQuery;

/// Sepolia and mainnet fee tokens (apollo_rpc_execution).
const STRK_FEE_CONTRACT_ADDRESS: &str =
    "0x04718f5a0fc34cc1af16a1cdee98ffb20c31f5cd61d6ab07201858f4287c938d";
const ETH_FEE_CONTRACT_ADDRESS: &str =
    "0x49d36570d4e46f48e99674bd3fcc84644ddd6b96f7c741b1562b82f9e004dc7";

/// Mirror of `VirtualOsBlockInput` in starknet_transaction_prover's runner; serializes to the
/// same JSON as its dump.
#[derive(Deserialize, Serialize)]
pub struct VirtualOsBlockInput {
    contract_state_commitment_info: CommitmentInfo,
    address_to_storage_commitment_info: HashMap<ContractAddress, CommitmentInfo>,
    contract_class_commitment_info: CommitmentInfo,
    chain_info: ChainInfoDump,
    transactions: Vec<(InvokeTransaction, TransactionHash)>,
    tx_execution_infos: Vec<CentralTransactionExecutionInfo>,
    block_info: BlockInfo,
    raw_starknet_version: String,
    initial_reads: StateMaps,
    base_block_hash: BlockHash,
    base_block_header_commitments: BlockHeaderCommitments,
    prev_base_block_hash: BlockHash,
    compiled_classes: BTreeMap<CompiledClassHash, CasmContractClass>,
}

/// `OsChainInfo` serializes its chain ID by name but only deserializes it from hex.
#[derive(Deserialize, Serialize)]
struct ChainInfoDump {
    chain_id: ChainId,
    strk_fee_token_address: ContractAddress,
}

/// Same construction as the transaction prover's `From<VirtualOsBlockInput> for OsHints`.
impl From<VirtualOsBlockInput> for OsHints {
    fn from(input: VirtualOsBlockInput) -> Self {
        let os_block_input = OsBlockInput {
            block_hash_commitments: input.base_block_header_commitments,
            contract_state_commitment_info: input.contract_state_commitment_info,
            address_to_storage_commitment_info: input.address_to_storage_commitment_info,
            contract_class_commitment_info: input.contract_class_commitment_info,
            transactions: input
                .transactions
                .into_iter()
                .map(|(tx, tx_hash)| {
                    Transaction::Account(AccountTransaction::Invoke(
                        starknet_api::executable_transaction::InvokeTransaction { tx, tx_hash },
                    ))
                })
                .collect(),
            tx_execution_infos: input.tx_execution_infos,
            prev_block_hash: input.prev_base_block_hash,
            block_info: input.block_info,
            starknet_version_override: Some(input.raw_starknet_version),
            initial_reads: input.initial_reads,
            declared_class_hash_to_component_hashes: HashMap::new(),
            new_block_hash: input.base_block_hash,
            old_block_number_and_hash: None,
            class_hashes_to_migrate: Vec::new(),
        };
        OsHints {
            os_input: StarknetOsInput {
                os_block_inputs: vec![os_block_input],
                deprecated_compiled_classes: BTreeMap::new(),
                compiled_classes: input.compiled_classes,
            },
            os_hints_config: OsHintsConfig {
                debug_mode: false,
                full_output: false,
                use_kzg_da: false,
                chain_info: OsChainInfo {
                    chain_id: input.chain_info.chain_id,
                    strk_fee_token_address: input.chain_info.strk_fee_token_address,
                },
                public_keys: None,
                rng_seed_salt: None,
            },
        }
    }
}

/// Runs the virtual OS on a dumped (or built) virtual OS input and returns the Cairo PIE.
pub fn run(virtual_os_input_json: &[u8]) -> Result<CairoPie, String> {
    let input: VirtualOsBlockInput =
        serde_json::from_slice(virtual_os_input_json).map_err(|error| error.to_string())?;
    run_input(input)
}

fn run_input(input: VirtualOsBlockInput) -> Result<CairoPie, String> {
    let output =
        starknet_os::runner::run_virtual_os(input.into()).map_err(|error| format!("{error:?}"))?;
    Ok(output.cairo_pie)
}

/// One line describing a PIE: steps, holes, and sha256 of each part, to compare runs.
pub fn summary(pie: &CairoPie) -> String {
    let hash = |bytes: &[u8]| {
        Sha256::digest(bytes).iter().take(8).map(|byte| format!("{byte:02x}")).collect::<String>()
    };
    let json = |value: &dyn erased::Json| hash(&value.to_json());
    format!(
        "n_steps={} n_memory_holes={} memory={} metadata={} additional_data={} resources={}",
        pie.execution_resources.n_steps,
        pie.execution_resources.n_memory_holes,
        hash(&pie.memory.to_bytes(None)),
        json(&pie.metadata),
        json(&pie.additional_data),
        json(&pie.execution_resources),
    )
}

mod erased {
    pub trait Json {
        fn to_json(&self) -> Vec<u8>;
    }
    impl<T: serde::Serialize> Json for T {
        fn to_json(&self) -> Vec<u8> {
            serde_json::to_vec(self).unwrap_or_default()
        }
    }
}

/// Verifies a `starknet_proveTransaction` result (or a whole JSON-RPC response) with the
/// verifier the sequencer uses.
pub fn verify_result(result_json: &str) -> Result<(), String> {
    #[derive(Deserialize)]
    struct ProveResult {
        proof: Proof,
        proof_facts: ProofFacts,
    }
    let mut value: serde_json::Value =
        serde_json::from_str(result_json).map_err(|e| e.to_string())?;
    if let Some(result) = value.get_mut("result") {
        value = result.take();
    }
    let result: ProveResult = serde_json::from_value(value).map_err(|e| e.to_string())?;
    starknet_proof_verifier::verify_proof(result.proof_facts, result.proof)
        .map_err(|e| e.to_string())
}

#[wasm_bindgen]
pub fn verify_prove_result(result_json: &str) -> Result<(), JsError> {
    verify_result(result_json).map_err(|e| JsError::new(&e))
}

#[wasm_bindgen]
pub fn run_virtual_os(virtual_os_input_json: &[u8]) -> Result<String, JsError> {
    let pie = run(virtual_os_input_json).map_err(|error| JsError::new(&error))?;
    Ok(summary(&pie))
}

/// `starknet_simulateTransactions`' `initial_reads`, also used to hand in reads fetched later.
#[derive(Deserialize, Default)]
struct RpcInitialReads {
    #[serde(default)]
    storage: Vec<StorageEntry>,
    #[serde(default)]
    nonces: Vec<NonceEntry>,
    #[serde(default)]
    class_hashes: Vec<ClassHashEntry>,
    #[serde(default)]
    declared_contracts: Vec<DeclaredEntry>,
}

#[derive(Deserialize)]
struct StorageEntry {
    contract_address: ContractAddress,
    key: StorageKey,
    value: Felt,
}

#[derive(Deserialize)]
struct NonceEntry {
    contract_address: ContractAddress,
    nonce: Nonce,
}

#[derive(Deserialize)]
struct ClassHashEntry {
    contract_address: ContractAddress,
    class_hash: ClassHash,
}

#[derive(Deserialize)]
struct DeclaredEntry {
    class_hash: ClassHash,
    is_declared: bool,
}

/// Converts a `CompiledClassV1` back to CASM as the transaction prover's classes provider does.
fn compiled_class_v1_to_casm(class: &CompiledClassV1) -> Result<CasmContractClass, String> {
    let bytecode: Vec<BigUintAsHex> = class
        .program
        .iter_data()
        .map(|maybe_relocatable| match maybe_relocatable {
            MaybeRelocatable::Int(felt) => Ok(BigUintAsHex { value: felt.to_biguint() }),
            MaybeRelocatable::RelocatableValue(relocatable) => {
                Err(format!("bytecode contained a relocatable value: {relocatable:?}"))
            }
        })
        .collect::<Result<Vec<_>, _>>()?;

    Ok(CasmContractClass {
        prime: Felt::prime(),
        compiler_version: String::new(),
        bytecode,
        bytecode_segment_lengths: Some(class.bytecode_segment_felt_sizes().into()),
        hints: program_hints_to_casm_hints(&class.program.shared_program_data.hints_collection)
            .map_err(|e| e.to_string())?,
        pythonic_hints: None,
        entry_points_by_type: (&class.entry_points_by_type).into(),
    })
}

fn js_error(error: impl std::fmt::Display) -> Failure {
    fail(&error.to_string())
}

/// What TxSession's methods fail with: a JS `Error` on wasm. Natively, the message itself:
/// wasm-bindgen's JsError can't be made off wasm (it panics with "cannot call wasm-bindgen
/// imported functions on non-wasm targets"), which would hide every error of a native run.
#[cfg(target_arch = "wasm32")]
pub type Failure = JsError;
#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug)]
pub struct Failure(pub String);

// #[wasm_bindgen] checks the methods' types on every target, so a Failure must turn into a JsValue
// natively too. Nothing calls this off wasm.
#[cfg(not(target_arch = "wasm32"))]
impl From<Failure> for JsValue {
    fn from(failure: Failure) -> JsValue {
        JsValue::from_str(&failure.0)
    }
}

#[cfg(target_arch = "wasm32")]
fn fail(message: &str) -> Failure {
    JsError::new(message)
}
#[cfg(not(target_arch = "wasm32"))]
fn fail(message: &str) -> Failure {
    Failure(message.to_string())
}

/// One transaction's way from RPC data to a virtual OS run.
#[wasm_bindgen]
pub struct TxSession {
    chain_info: ChainInfo,
    header: BlockHeader,
    txs: Vec<(InvokeTransaction, TransactionHash)>,
    maps: StateMaps,
    classes: HashMap<ClassHash, CompiledClasses>,
    undeclared_classes: BTreeSet<ClassHash>,
    contract_class_manager: ContractClassManager,
    execution: Option<(VirtualBlockExecutionData, PrefetchedState)>,
    query: Option<(RpcStorageProofsQuery, Vec<RpcStorageProofsQuery>)>,
    os_input: Option<VirtualOsBlockInput>,
    l2_to_l1_messages: Vec<MessageToL1>,
    pie: Option<CairoPie>,
}

#[wasm_bindgen]
impl TxSession {
    /// `header_json`: a `starknet_getBlockWithTxHashes` result. `rpc_transaction_json`: the
    /// invoke v3 transaction as sent to `starknet_proveTransaction`.
    #[wasm_bindgen(constructor)]
    pub fn new(
        chain_id: &str,
        header_json: &str,
        rpc_transaction_json: &str,
    ) -> Result<TxSession, Failure> {
        let chain_id = ChainId::from(chain_id.to_string());
        let header: BlockHeader = serde_json::from_str(header_json).map_err(js_error)?;
        let RpcTransaction::Invoke(RpcInvokeTransaction::V3(invoke_v3)) =
            serde_json::from_str(rpc_transaction_json).map_err(js_error)?
        else {
            return Err(fail("only invoke v3 transactions are supported"));
        };
        // The prover's input checks (validate_transaction_input with zero-fee validation on).
        if !invoke_v3.proof.is_empty() || !invoke_v3.proof_facts.is_empty() {
            return Err(fail("proof and proof_facts must be empty on input"));
        }
        let bounds = &invoke_v3.resource_bounds;
        if bounds.l1_gas.max_price_per_unit != GasPrice(0)
            || bounds.l2_gas.max_price_per_unit != GasPrice(0)
            || bounds.l1_data_gas.max_price_per_unit != GasPrice(0)
            || invoke_v3.tip != Tip(0)
        {
            return Err(fail("proving is client-side: gas prices and tip must be zero"));
        }
        let tx = InvokeTransaction::V3(invoke_v3.into());
        let tx_hash = tx.calculate_transaction_hash(&chain_id, &tx.version()).map_err(js_error)?;
        let parse_address = |hex: &str| {
            ContractAddress::try_from(Felt::from_hex(hex).expect("constant address"))
                .expect("constant address")
        };
        let chain_info = ChainInfo {
            chain_id,
            fee_token_addresses: FeeTokenAddresses {
                strk_fee_token_address: parse_address(STRK_FEE_CONTRACT_ADDRESS),
                eth_fee_token_address: parse_address(ETH_FEE_CONTRACT_ADDRESS),
            },
            is_l3: false,
        };
        Ok(TxSession {
            chain_info,
            header,
            txs: vec![(tx, tx_hash)],
            maps: StateMaps::default(),
            classes: HashMap::new(),
            undeclared_classes: BTreeSet::new(),
            contract_class_manager: ContractClassManager::start(Default::default()),
            execution: None,
            query: None,
            os_input: None,
            l2_to_l1_messages: Vec::new(),
            pie: None,
        })
    }

    pub fn tx_hash(&self) -> String {
        self.txs[0].1.0.to_hex_string()
    }

    pub fn block_number(&self) -> u64 {
        self.header.block_number.0
    }

    /// Adds state reads in the `initial_reads` format of `starknet_simulateTransactions`.
    pub fn add_reads(&mut self, initial_reads_json: &str) -> Result<(), Failure> {
        let reads: RpcInitialReads = serde_json::from_str(initial_reads_json).map_err(js_error)?;
        self.maps.storage.extend(reads.storage.into_iter().map(|e| ((e.contract_address, e.key), e.value)));
        self.maps.nonces.extend(reads.nonces.into_iter().map(|e| (e.contract_address, e.nonce)));
        self.maps
            .class_hashes
            .extend(reads.class_hashes.into_iter().map(|e| (e.contract_address, e.class_hash)));
        self.maps
            .declared_contracts
            .extend(reads.declared_contracts.into_iter().map(|e| (e.class_hash, e.is_declared)));
        Ok(())
    }

    /// Adds a Cairo 1 class: its `starknet_getCompiledCasm` result and the first three felts of
    /// its Sierra program (`starknet_getClass`), which encode the Sierra version.
    pub fn add_class(
        &mut self,
        class_hash: &str,
        casm_json: &str,
        sierra_program_head_json: &str,
    ) -> Result<(), Failure> {
        let class_hash = ClassHash(Felt::from_hex(class_hash).map_err(js_error)?);
        let casm: CasmContractClass = serde_json::from_str(casm_json).map_err(js_error)?;
        let head: Vec<Felt> = serde_json::from_str(sierra_program_head_json).map_err(js_error)?;
        let sierra_version = SierraVersion::extract_from_program(&head).map_err(js_error)?;
        let compiled = CompiledClassV1::try_from((casm, sierra_version)).map_err(js_error)?;
        self.classes.insert(
            class_hash,
            CompiledClasses::V1(compiled, Arc::new(SierraContractClass::default())),
        );
        Ok(())
    }

    /// Records that the node does not know this class.
    pub fn add_undeclared_class(&mut self, class_hash: &str) -> Result<(), Failure> {
        self.undeclared_classes.insert(ClassHash(Felt::from_hex(class_hash).map_err(js_error)?));
        Ok(())
    }

    /// Executes the transaction. Returns `{"missing": ...}` when a read was not prefetched
    /// (fetch it, add it, call again), else `{"storage_proof_params": [...]}`: the
    /// `starknet_getStorageProof` requests to make.
    pub fn execute(&mut self) -> Result<String, Failure> {
        let state = PrefetchedState {
            maps: Arc::new(self.maps.clone()),
            classes: Arc::new(self.classes.clone()),
            undeclared_classes: Arc::new(self.undeclared_classes.clone()),
            missing: Arc::new(Mutex::new(Default::default())),
        };
        let base_block_info = BaseBlockInfo::new(self.header.clone(), self.chain_info.clone())
            .map_err(|e| fail(&e))?;
        let result = executor::execute(
            base_block_info,
            state.clone(),
            self.contract_class_manager.clone(),
            self.txs.clone(),
        );
        // Classes compiled during a failed run stay cached; drop them so reruns start clean.
        self.contract_class_manager = ContractClassManager::start(Default::default());
        let missing = std::mem::take(&mut *state.missing.lock().expect("missing reads lock"));
        if !missing.is_empty() {
            return Ok(serde_json::json!({ "missing": missing }).to_string());
        }
        let execution = result.map_err(|e| fail(&e))?;
        let query =
            storage_proofs::prepare_query(&execution.executed_class_hashes, &execution.initial_reads);
        let split = storage_proofs::split_query(&query);
        let params: Vec<_> = split
            .iter()
            .map(|q| storage_proofs::request_params(self.header.block_number, q))
            .collect();
        self.execution = Some((execution, state));
        self.query = Some((query, split));
        Ok(serde_json::json!({ "storage_proof_params": params }).to_string())
    }

    /// Takes the `starknet_getStorageProof` results (a JSON array, one per requested params),
    /// builds the virtual OS input and returns it as JSON (the native dump's format).
    pub fn set_storage_proofs(&mut self, storage_proofs_json: &str) -> Result<String, Failure> {
        let (execution, state) =
            self.execution.take().ok_or_else(|| fail("execute first"))?;
        let (query, split) = self.query.take().ok_or_else(|| fail("execute first"))?;
        let proofs: Vec<RpcStorageProof> =
            serde_json::from_str(storage_proofs_json).map_err(js_error)?;
        if proofs.len() != split.len() {
            return Err(fail("one storage proof per request expected"));
        }
        let rpc_proof = storage_proofs::merge_storage_proofs(proofs, &split, &query);

        let runtime =
            tokio::runtime::Builder::new_current_thread().build().map_err(js_error)?;
        let storage_proofs = runtime
            .block_on(storage_proofs::storage_proofs_from_rpc_proof(
                &rpc_proof,
                &query,
                &execution.initial_reads,
                &execution.state_diff,
            ))
            .map_err(js_error)?;

        // Classes, as the prover's classes provider returns them.
        let mut compiled_classes = BTreeMap::new();
        let mut class_hash_to_compiled_class_hash = HashMap::new();
        for class_hash in &execution.executed_class_hashes {
            let compiled_class = state.get_compiled_class(*class_hash).map_err(js_error)?;
            let compiled_class_hash =
                get_compiled_class_hash_v2(&state, *class_hash, &compiled_class).map_err(js_error)?;
            #[allow(unreachable_patterns)]
            let casm = match &compiled_class {
                RunnableCompiledClass::V1(class) => {
                    compiled_class_v1_to_casm(class).map_err(|e| fail(&e))?
                }
                _ => return Err(fail("Cairo 0 classes are not supported")),
            };
            compiled_classes.insert(compiled_class_hash, casm);
            class_hash_to_compiled_class_hash.insert(*class_hash, compiled_class_hash);
        }

        let mut extended_initial_reads = storage_proofs.extended_initial_reads;
        extended_initial_reads.compiled_class_hashes.extend(&class_hash_to_compiled_class_hash);
        // The OS calls `update_cache` with an empty class map, so this must be empty.
        extended_initial_reads.declared_contracts.clear();

        let base = &execution.base_block_info;
        let chain_info = base.block_context.chain_info();
        let input = VirtualOsBlockInput {
            contract_state_commitment_info: storage_proofs
                .commitment_infos
                .contracts_trie_commitment_info,
            address_to_storage_commitment_info: storage_proofs
                .commitment_infos
                .storage_tries_commitment_infos,
            contract_class_commitment_info: storage_proofs
                .commitment_infos
                .classes_trie_commitment_info,
            chain_info: ChainInfoDump {
                chain_id: chain_info.chain_id.clone(),
                strk_fee_token_address: chain_info.fee_token_addresses.strk_fee_token_address,
            },
            transactions: self.txs.clone(),
            tx_execution_infos: execution
                .execution_outputs
                .into_iter()
                .map(|output| output.0.into())
                .collect(),
            block_info: base.block_context.block_info().clone(),
            raw_starknet_version: base.raw_starknet_version.clone(),
            initial_reads: extended_initial_reads,
            base_block_hash: base.base_block_hash,
            base_block_header_commitments: base.base_block_header_commitments.clone(),
            prev_base_block_hash: base.prev_base_block_hash,
            compiled_classes,
        };
        let json = serde_json::to_string(&input).map_err(js_error)?;
        self.os_input = Some(input);
        self.l2_to_l1_messages = execution.l2_to_l1_messages;
        Ok(json)
    }

    /// Runs the virtual OS on the built input; returns the PIE summary.
    pub fn run_os(&mut self) -> Result<String, Failure> {
        let input = self.os_input.take().ok_or_else(|| fail("set_storage_proofs first"))?;
        let pie = run_input(input).map_err(|e| fail(&e))?;
        let pie_summary = summary(&pie);
        self.pie = Some(pie);
        Ok(pie_summary)
    }

    /// Proves the virtual OS run as the transaction prover does; returns the
    /// `starknet_proveTransaction` result JSON (proof, proof_facts, l2_to_l1_messages).
    pub fn prove(&mut self) -> Result<String, Failure> {
        if self.pie.is_none() {
            self.run_os()?;
        }
        let pie = self.pie.take().expect("the virtual OS ran");
        // What built the OS input is of no more use: it goes before the proof's peak.
        self.maps = StateMaps::default();
        self.classes = HashMap::new();
        self.execution = None;
        self.query = None;
        // One proof per session: each leg's precomputes go when the leg ends, and none outlive
        // the proof.
        let output = privacy_prove::privacy_recursive_prove_once(pie)
            .map_err(|e| fail(&format!("prover: {e}")))?;
        let proof = Proof::from(output.proof);
        let proof_facts = ProgramOutput::from(output.output_preimage)
            .try_into_proof_facts(VIRTUAL_SNOS)
            .map_err(js_error)?;
        Ok(serde_json::json!({
            "proof": proof,
            "proof_facts": proof_facts,
            "l2_to_l1_messages": self.l2_to_l1_messages,
        })
        .to_string())
    }
}

impl TxSession {
    /// Cairo steps of the virtual OS run, once `run_os` ran (until `prove` takes the run).
    pub fn os_steps(&self) -> Option<usize> {
        self.pie.as_ref().map(|pie| pie.execution_resources.n_steps)
    }
}

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = console, js_name = error)]
    fn console_error(message: &str);
    #[wasm_bindgen(js_namespace = console, js_name = log)]
    fn console_log(message: &str);
}

/// Writes each formatted tracing event to console.log.
struct ConsoleWriter(Vec<u8>);

impl std::io::Write for ConsoleWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Drop for ConsoleWriter {
    fn drop(&mut self) {
        let line = format!("[{}] {}", memory::report(), String::from_utf8_lossy(&self.0).trim_end());
        console_log(&line);
    }
}

/// Wasm memory size, live heap and peak live heap, in MiB.
#[wasm_bindgen]
pub fn memory_report() -> String {
    memory::report()
}

/// Sends `tracing` events at `level` and above (e.g. "info") to the console. No timestamps: there
/// is no clock on wasm32-unknown-unknown.
#[wasm_bindgen]
pub fn init_logging(level: &str) -> Result<(), JsError> {
    let level: tracing::Level = level.parse().map_err(|e| JsError::new(&format!("{e}")))?;
    tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_max_level(level)
        .with_writer(|| ConsoleWriter(Vec::new()))
        // Span entries mark the prover's phases. Not CLOSE: its timings read the clock.
        .with_span_events(tracing_subscriber::fmt::format::FmtSpan::ENTER)
        .try_init()
        .map_err(|e| JsError::new(&e.to_string()))
}

/// Prints panic messages to the console; a wasm panic otherwise shows only "unreachable".
#[wasm_bindgen(start)]
pub fn start() {
    std::panic::set_hook(Box::new(|info| console_error(&info.to_string())));
}
