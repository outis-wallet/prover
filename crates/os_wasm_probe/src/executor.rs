// Virtual block execution, copied from starknet_transaction_prover/src/running/
// virtual_block_executor.rs and blockifier_reexecution's rpc_objects.rs (PRIVACY-0.14.4-RC.0),
// minus the RPC transport.

use std::collections::HashSet;

use blockifier::blockifier::config::TransactionExecutorConfig;
use blockifier::blockifier::transaction_executor::{
    TransactionExecutionOutput,
    TransactionExecutor,
};
use blockifier::blockifier_versioned_constants::VersionedConstants;
use blockifier::bouncer::BouncerConfig;
use blockifier::context::{BlockContext, ChainInfo};
use blockifier::state::cached_state::{CachedState, StateMaps};
use blockifier::state::contract_class_manager::ContractClassManager;
use blockifier::state::state_reader_and_contract_manager::StateReaderAndContractManager;
use blockifier::transaction::account_transaction::ExecutionFlags;
use blockifier::transaction::transaction_execution::Transaction as BlockifierTransaction;
use serde::{Deserialize, Serialize};
use starknet_api::block::{
    BlockHash,
    BlockInfo,
    BlockNumber,
    BlockTimestamp,
    GasPrice,
    GasPricePerToken,
    GasPriceVector,
    GasPrices,
    NonzeroGasPrice,
    StarknetVersion,
};
use starknet_api::block_hash::block_hash_calculator::{concat_counts, BlockHeaderCommitments};
use starknet_api::contract_class::SierraVersion;
use starknet_api::core::{
    ClassHash,
    ContractAddress,
    EventCommitment,
    GlobalRoot,
    ReceiptCommitment,
    StateDiffCommitment,
    TransactionCommitment,
};
use starknet_api::data_availability::L1DataAvailabilityMode;
use starknet_api::transaction::fields::Fee;
use starknet_api::transaction::{InvokeTransaction, MessageToL1, Transaction, TransactionHash};
use starknet_api::versioned_constants_logic::VersionedConstantsTrait;
use starknet_api::StarknetApiError;

use crate::state_reader::PrefetchedState;

/// The transaction prover's embedded bouncer config (its default when none is given): a copy of
/// the sequencer's crates/starknet_transaction_prover/resources/bouncer_config.json, at the
/// revision Cargo.toml pins.
const BOUNCER_CONFIG_JSON: &str = include_str!("../resources/bouncer_config.json");

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct BlockHeader {
    pub block_hash: BlockHash,
    pub parent_hash: BlockHash,
    pub block_number: BlockNumber,
    pub sequencer_address: ContractAddress,
    pub new_root: GlobalRoot,
    pub timestamp: BlockTimestamp,
    pub l1_gas_price: GasPricePerToken,
    pub l1_data_gas_price: GasPricePerToken,
    pub l2_gas_price: GasPricePerToken,
    pub l1_da_mode: L1DataAvailabilityMode,
    pub starknet_version: String,
    pub state_diff_commitment: StateDiffCommitment,
    pub event_commitment: EventCommitment,
    pub receipt_commitment: ReceiptCommitment,
    pub transaction_commitment: TransactionCommitment,
    pub transaction_count: usize,
    pub event_count: usize,
    pub state_diff_length: usize,
}

fn parse_gas_price(gas_price: GasPrice) -> Result<NonzeroGasPrice, String> {
    NonzeroGasPrice::new(gas_price).map_err(|_| format!("gas price parsing failure: {gas_price:?}"))
}

impl TryFrom<BlockHeader> for BlockInfo {
    type Error = String;
    fn try_from(header: BlockHeader) -> Result<BlockInfo, String> {
        Ok(BlockInfo {
            block_number: header.block_number,
            sequencer_address: header.sequencer_address,
            block_timestamp: header.timestamp,
            gas_prices: GasPrices {
                eth_gas_prices: GasPriceVector {
                    l1_gas_price: parse_gas_price(header.l1_gas_price.price_in_wei)?,
                    l1_data_gas_price: parse_gas_price(header.l1_data_gas_price.price_in_wei)?,
                    l2_gas_price: parse_gas_price(header.l2_gas_price.price_in_wei)?,
                },
                strk_gas_prices: GasPriceVector {
                    l1_gas_price: parse_gas_price(header.l1_gas_price.price_in_fri)?,
                    l1_data_gas_price: parse_gas_price(header.l1_data_gas_price.price_in_fri)?,
                    l2_gas_price: parse_gas_price(header.l2_gas_price.price_in_fri)?,
                },
            },
            use_kzg_da: header.l1_da_mode.is_use_kzg_da(),
            starknet_version: header.starknet_version.try_into().map_err(|e| format!("{e}"))?,
        })
    }
}

pub struct BaseBlockInfo {
    pub block_context: BlockContext,
    /// The base block's version string exactly as returned by the RPC.
    pub raw_starknet_version: String,
    pub base_block_hash: BlockHash,
    pub base_block_header_commitments: BlockHeaderCommitments,
    pub prev_base_block_hash: BlockHash,
}

impl BaseBlockInfo {
    /// `BaseBlockInfo::new` with the prover's defaults: latest versioned constants and the
    /// embedded bouncer config.
    pub fn new(mut header: BlockHeader, chain_info: ChainInfo) -> Result<Self, String> {
        let base_block_hash = header.block_hash;
        let prev_base_block_hash = header.parent_hash;
        let base_block_header_commitments = BlockHeaderCommitments {
            transaction_commitment: header.transaction_commitment,
            event_commitment: header.event_commitment,
            receipt_commitment: header.receipt_commitment,
            state_diff_commitment: header.state_diff_commitment,
            concatenated_counts: concat_counts(
                header.transaction_count,
                header.event_count,
                header.state_diff_length,
                header.l1_da_mode,
            ),
        };

        let raw_starknet_version = header.starknet_version.clone();
        header.starknet_version = starknet_version_or_latest(&raw_starknet_version)
            .map_err(|e| format!("Unsupported starknet version in block header: {e}"))?
            .to_string();
        let block_info: BlockInfo = header.try_into()?;
        let mut versioned_constants = VersionedConstants::latest_constants().clone();
        // Disable casm hash migration for virtual block execution.
        versioned_constants.disable_casm_hash_migration();
        // Enable Sierra gas for all Cairo 1 contracts.
        versioned_constants.min_sierra_version_for_sierra_gas = SierraVersion::new(0, 0, 1);

        let mut block_context = BlockContext::new(
            block_info,
            chain_info,
            versioned_constants,
            BouncerConfig::default(),
        );
        // Override with client-side caps, as RpcVirtualBlockExecutor::base_block_info does.
        block_context.bouncer_config = serde_json::from_str(BOUNCER_CONFIG_JSON)
            .map_err(|e| format!("embedded bouncer_config.json: {e}"))?;

        Ok(BaseBlockInfo {
            block_context,
            raw_starknet_version,
            base_block_hash,
            base_block_header_commitments,
            prev_base_block_hash,
        })
    }
}

fn starknet_version_or_latest(version_string: &str) -> Result<StarknetVersion, StarknetApiError> {
    match StarknetVersion::try_from(version_string) {
        Err(StarknetApiError::InvalidStarknetVersion(segments))
            if segments > Vec::<u8>::from(StarknetVersion::LATEST) =>
        {
            Ok(StarknetVersion::LATEST)
        }
        result => result,
    }
}

pub struct VirtualBlockExecutionData {
    pub execution_outputs: Vec<TransactionExecutionOutput>,
    pub initial_reads: StateMaps,
    pub state_diff: StateMaps,
    pub executed_class_hashes: HashSet<ClassHash>,
    pub l2_to_l1_messages: Vec<MessageToL1>,
    pub base_block_info: BaseBlockInfo,
}

/// `VirtualBlockExecutor::execute` over a prefetched state, with validation on (the prover's
/// default) and no strict nonce check.
pub fn execute(
    base_block_info: BaseBlockInfo,
    state: PrefetchedState,
    contract_class_manager: ContractClassManager,
    txs: Vec<(InvokeTransaction, TransactionHash)>,
) -> Result<VirtualBlockExecutionData, String> {
    let (txs, tx_hashes): (Vec<Transaction>, Vec<TransactionHash>) =
        txs.into_iter().map(|(invoke, hash)| (Transaction::Invoke(invoke), hash)).unzip();
    let blockifier_txs = convert_txs(txs, &tx_hashes)?;

    let state_reader_and_contract_manager =
        StateReaderAndContractManager::new(state, contract_class_manager, None);
    let block_state = CachedState::new(state_reader_and_contract_manager);

    let mut transaction_executor = TransactionExecutor::new(
        block_state,
        base_block_info.block_context.clone(),
        TransactionExecutorConfig::default(),
    );

    let execution_results = transaction_executor.execute_txs(&blockifier_txs, None);
    let execution_outputs: Vec<TransactionExecutionOutput> = execution_results
        .into_iter()
        .map(|result| result.map_err(|e| format!("transaction execution error: {e}")))
        .collect::<Result<Vec<_>, _>>()?;

    for (output, tx_hash) in execution_outputs.iter().zip(tx_hashes.iter()) {
        if let Some(revert_error) = &output.0.revert_error {
            return Err(format!("transaction {tx_hash} reverted: {revert_error}"));
        }
    }

    let block_state =
        transaction_executor.block_state.as_mut().ok_or("block state unavailable".to_string())?;
    let initial_reads =
        block_state.get_initial_reads().map_err(|e| format!("initial reads: {e}"))?;
    let state_diff =
        block_state.to_state_diff().map_err(|e| format!("Failed to get state diff: {e}"))?.state_maps;

    let executed_class_hashes = transaction_executor
        .bouncer
        .lock()
        .map_err(|e| format!("bouncer lock: {e}"))?
        .get_executed_class_hashes();

    let mut l2_to_l1_messages = Vec::new();
    for (execution_info, _state_diff) in &execution_outputs {
        l2_to_l1_messages.extend(
            execution_info
                .non_optional_call_infos()
                .flat_map(|call_info| call_info.get_sorted_l2_to_l1_messages()),
        );
    }

    Ok(VirtualBlockExecutionData {
        execution_outputs,
        base_block_info,
        initial_reads,
        state_diff,
        l2_to_l1_messages,
        executed_class_hashes,
    })
}

fn convert_txs(
    txs: Vec<Transaction>,
    tx_hashes: &[TransactionHash],
) -> Result<Vec<BlockifierTransaction>, String> {
    txs.into_iter()
        .zip(tx_hashes.iter().copied())
        .map(|(tx, tx_hash)| {
            let Transaction::Invoke(invoke_tx) = &tx else {
                return Err(format!("only invoke transactions are supported, got {tx:?}"));
            };
            let execution_flags = ExecutionFlags {
                only_query: false,
                charge_fee: invoke_tx.resource_bounds().max_possible_fee(invoke_tx.tip())
                    > Fee(0),
                validate: true,
                strict_nonce_check: false,
            };
            BlockifierTransaction::from_api(tx, tx_hash, None, None, None, execution_flags)
                .map_err(|e| e.to_string())
        })
        .collect()
}
