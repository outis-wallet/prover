use starknet_patricia_storage::errors::SerializationError;
use thiserror::Error;

/// The variants of starknet_transaction_prover's `ProofProviderError` the pure code uses.
#[derive(Debug, Error)]
pub enum ProofProviderError {
    #[error("Invalid state diff: {0}")]
    InvalidStateDiff(String),
    #[error(transparent)]
    SerializationError(#[from] SerializationError),
    #[error("Invalid RPC proof response: {0}")]
    InvalidProofResponse(String),
    #[error("Block commitment error: {0}")]
    BlockCommitmentError(String),
}
