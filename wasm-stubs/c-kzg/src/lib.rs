//! Stand-in for c-kzg on wasm32, where the C library cannot build. KZG commitments are only
//! needed for L1 blob data availability, which the virtual OS does not use.

use std::fmt;
use std::ops::Deref;

pub const BYTES_PER_FIELD_ELEMENT: usize = 32;
pub const FIELD_ELEMENTS_PER_BLOB: usize = 4096;
pub const BYTES_PER_BLOB: usize = BYTES_PER_FIELD_ELEMENT * FIELD_ELEMENTS_PER_BLOB;

#[derive(Debug)]
pub struct Error;

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("KZG is not available in this wasm build")
    }
}

impl std::error::Error for Error {}

pub struct Blob(());

impl Blob {
    pub fn from_bytes(_bytes: &[u8]) -> Result<Self, Error> {
        Err(Error)
    }
}

pub struct Bytes48([u8; 48]);

impl AsRef<[u8; 48]> for Bytes48 {
    fn as_ref(&self) -> &[u8; 48] {
        &self.0
    }
}

pub struct KzgCommitment(Bytes48);

impl KzgCommitment {
    pub fn to_bytes(&self) -> Bytes48 {
        Bytes48(self.0 .0)
    }
}

impl Deref for KzgCommitment {
    type Target = [u8; 48];
    fn deref(&self) -> &[u8; 48] {
        &self.0 .0
    }
}

pub struct KzgProof(Bytes48);

impl Deref for KzgProof {
    type Target = [u8; 48];
    fn deref(&self) -> &[u8; 48] {
        &self.0 .0
    }
}

pub struct KzgSettings(());

impl KzgSettings {
    pub fn parse_kzg_trusted_setup(_trusted_setup: &str, _precompute: u64) -> Result<Self, Error> {
        Ok(Self(()))
    }

    pub fn blob_to_kzg_commitment(&self, _blob: &Blob) -> Result<KzgCommitment, Error> {
        Err(Error)
    }

    pub fn compute_blob_kzg_proof(
        &self,
        _blob: &Blob,
        _commitment_bytes: &Bytes48,
    ) -> Result<KzgProof, Error> {
        Err(Error)
    }

    pub fn compute_cells_and_kzg_proofs(&self, _blob: &Blob) -> Result<((), Vec<KzgProof>), Error> {
        Err(Error)
    }
}
