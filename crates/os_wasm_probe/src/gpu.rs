//! The striped commitments on a GPU (feature `gpu`): stwo's `CommitmentTreeProver::new_striped`
//! tries a hook first (`set_striped_commit_hook`), and this module registers one, for the target
//! it is built for:
//! - natively (`gpu/native.rs`): stwo-gpu-spike's `GpuCommitter`, on the machine's GPU through wgpu
//!   (Vulkan, Metal, DX12); off until `OUTIS_GPU=1` or `tx_prove_set_gpu` turns it on;
//! - in browsers (`gpu/web.rs`, the threaded build): the page's WebGPU worker, through the bridge
//!   (stwo-gpu-bridge); `js/worker.mjs` attaches it and enables the hook for a proof.
//!
//! Either way a commitment the GPU declines or fails is made on the CPU, and the GPU's tree is
//! `new_striped`'s bit for bit: the proof is the same with or without the GPU.
#[cfg(not(target_arch = "wasm32"))]
mod native;
#[cfg(not(target_arch = "wasm32"))]
pub use native::*;

#[cfg(target_arch = "wasm32")]
mod web;
#[cfg(target_arch = "wasm32")]
pub use web::*;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use stwo::core::vcs_lifted::blake2_merkle::Blake2sMerkleChannel;
use stwo::prover::backend::simd::SimdBackend;
use stwo::prover::{StripedCommitHook, set_striped_commit_hook};

type CommitHook = Arc<StripedCommitHook<SimdBackend, Blake2sMerkleChannel>>;

/// Whether the counting hook (see `set_commit_hook`) is registered.
static COUNTED: AtomicBool = AtomicBool::new(false);

/// Registers `hook` as the striped commitment's (`None`: the CPU makes every commitment, as with
/// no hook), behind one that counts each commitment as it starts, for the proof's progress
/// (progress.rs). Without a hook of its own it gives the polynomials back as they came, and stwo
/// commits them itself: the proof is the same as with no hook at all.
pub(crate) fn set_commit_hook(hook: Option<CommitHook>) {
    COUNTED.store(true, Ordering::Relaxed);
    set_striped_commit_hook::<SimdBackend, Blake2sMerkleChannel>(Some(Arc::new(
        move |polynomials, log_blowup, twiddles, lifting_log_size| {
            crate::progress::commitment_starts();
            match &hook {
                Some(hook) => hook(polynomials, log_blowup, twiddles, lifting_log_size),
                None => Err(polynomials),
            }
        },
    )));
}

/// Counts the commitments from now on, where nothing registered a hook yet (a proof on the CPU
/// that never had the GPU turned on or off).
pub(crate) fn count_commitments() {
    if !COUNTED.load(Ordering::Relaxed) {
        set_commit_hook(None);
    }
}
