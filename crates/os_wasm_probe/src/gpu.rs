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
