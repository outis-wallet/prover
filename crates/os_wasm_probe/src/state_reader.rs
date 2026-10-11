//! A state reader over state fetched ahead of time. A read it cannot answer is recorded, so the
//! caller can fetch it and execute again. It fails, or, with `placeholders`, a storage value or a
//! nonce reads as zero and the run carries on, so that one run finds more of what is missing.

use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex};

use blockifier::execution::contract_class::RunnableCompiledClass;
use blockifier::state::cached_state::StateMaps;
use blockifier::state::errors::StateError;
use blockifier::state::global_cache::CompiledClasses;
use blockifier::state::state_api::{StateReader, StateResult};
use blockifier::state::state_reader_and_contract_manager::FetchCompiledClasses;
use blockifier::state::utils::get_compiled_class_hash_v2 as default_get_compiled_class_hash_v2;
use serde::Serialize;
use starknet_api::core::{ClassHash, CompiledClassHash, ContractAddress, Nonce};
use starknet_api::state::StorageKey;
use starknet_types_core::felt::Felt;

/// Reads the prefetched state could not answer.
#[derive(Debug, Default, Serialize)]
pub struct MissingReads {
    pub storage: BTreeSet<(ContractAddress, StorageKey)>,
    pub nonces: BTreeSet<ContractAddress>,
    pub class_hashes: BTreeSet<ContractAddress>,
    pub classes: BTreeSet<ClassHash>,
}

impl MissingReads {
    pub fn is_empty(&self) -> bool {
        self.storage.is_empty()
            && self.nonces.is_empty()
            && self.class_hashes.is_empty()
            && self.classes.is_empty()
    }
}

#[derive(Clone)]
pub struct PrefetchedState {
    pub maps: Arc<StateMaps>,
    pub classes: Arc<HashMap<ClassHash, CompiledClasses>>,
    /// Classes the node reported as not declared.
    pub undeclared_classes: Arc<BTreeSet<ClassHash>>,
    pub missing: Arc<Mutex<MissingReads>>,
    /// Missing storage values and nonces read as zero instead of failing the run. Such a run only
    /// finds reads: its result is thrown away while anything is missing.
    pub placeholders: bool,
}

impl PrefetchedState {
    fn miss(&self, record: impl FnOnce(&mut MissingReads), what: String) -> StateError {
        record(&mut self.missing.lock().expect("missing reads lock"));
        StateError::StateReadError(format!("not prefetched: {what}"))
    }
}

impl StateReader for PrefetchedState {
    fn get_storage_at(
        &self,
        contract_address: ContractAddress,
        key: StorageKey,
    ) -> StateResult<Felt> {
        match self.maps.storage.get(&(contract_address, key)) {
            Some(value) => Ok(*value),
            None if self.placeholders => {
                self.missing.lock().expect("missing reads lock").storage.insert((contract_address, key));
                Ok(Felt::ZERO)
            }
            None => Err(self.miss(
                |missing| {
                    missing.storage.insert((contract_address, key));
                },
                format!("storage {contract_address:?} {key:?}"),
            )),
        }
    }

    fn get_nonce_at(&self, contract_address: ContractAddress) -> StateResult<Nonce> {
        match self.maps.nonces.get(&contract_address) {
            Some(value) => Ok(*value),
            None if self.placeholders => {
                self.missing.lock().expect("missing reads lock").nonces.insert(contract_address);
                Ok(Nonce::default())
            }
            None => Err(self.miss(
                |missing| {
                    missing.nonces.insert(contract_address);
                },
                format!("nonce {contract_address:?}"),
            )),
        }
    }

    fn get_class_hash_at(&self, contract_address: ContractAddress) -> StateResult<ClassHash> {
        match self.maps.class_hashes.get(&contract_address) {
            Some(value) => Ok(*value),
            None => Err(self.miss(
                |missing| {
                    missing.class_hashes.insert(contract_address);
                },
                format!("class hash {contract_address:?}"),
            )),
        }
    }

    fn get_compiled_class(&self, class_hash: ClassHash) -> StateResult<RunnableCompiledClass> {
        Ok(self.get_compiled_classes(class_hash)?.to_runnable())
    }

    fn get_compiled_class_hash(&self, _class_hash: ClassHash) -> StateResult<CompiledClassHash> {
        // Same as the RPC state reader the native prover uses.
        Err(StateError::StateReadError(
            "The rpc state reader does not support get_compiled_class_hash.".to_string(),
        ))
    }

    fn get_compiled_class_hash_v2(
        &self,
        class_hash: ClassHash,
        compiled_class: &RunnableCompiledClass,
    ) -> StateResult<CompiledClassHash> {
        default_get_compiled_class_hash_v2(self, class_hash, compiled_class)
    }
}

impl FetchCompiledClasses for PrefetchedState {
    fn get_compiled_classes(&self, class_hash: ClassHash) -> StateResult<CompiledClasses> {
        if let Some(classes) = self.classes.get(&class_hash) {
            return Ok(classes.clone());
        }
        if self.undeclared_classes.contains(&class_hash) {
            return Err(StateError::UndeclaredClassHash(class_hash));
        }
        Err(self.miss(
            |missing| {
                missing.classes.insert(class_hash);
            },
            format!("class {class_hash:?}"),
        ))
    }

    fn is_declared(&self, class_hash: ClassHash) -> StateResult<bool> {
        // The native prover's SimulatedStateReader: prefetched value, else the RPC reader's
        // answer, which is always true.
        Ok(self.maps.declared_contracts.get(&class_hash).copied().unwrap_or(true))
    }
}
