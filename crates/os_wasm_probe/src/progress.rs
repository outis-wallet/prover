//! How far a proof has come, for a wallet's progress bar: the striped commitments it has started.
//! A proof makes the same ones in the same order whatever makes them (the CPU or a GPU), so a
//! wallet that timed an earlier proof of the same kind can tell from the count how far along this
//! one is, and how fast it is going.
//!
//! The count is read while the proof runs, from outside it: in a browser by a worker of its own,
//! from the module's shared memory at `progress_address` (the worker proving is inside the proof's
//! synchronous call, as `js/worker.mjs` explains); natively with `tx_prove_progress` (native.rs),
//! from any thread. It is counted at stwo's striped commitment hook, which only builds with the
//! `gpu` feature register (gpu.rs); in other builds it stays 0.

use std::sync::atomic::{AtomicU32, Ordering};

use wasm_bindgen::prelude::*;

/// The striped commitments the proof under way (or the last one) has started.
static COMMITMENTS: AtomicU32 = AtomicU32::new(0);

/// A proof starts: none of its commitments yet, and each one counted from here.
pub(crate) fn proof_starts() {
    COMMITMENTS.store(0, Ordering::Relaxed);
    #[cfg(feature = "gpu")]
    crate::gpu::count_commitments();
}

/// A session opens: a count read before its proof starts is 0, not the last proof's.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn session_opens() {
    COMMITMENTS.store(0, Ordering::Relaxed);
}

/// A striped commitment starts.
#[cfg_attr(not(feature = "gpu"), allow(dead_code))]
pub(crate) fn commitment_starts() {
    COMMITMENTS.fetch_add(1, Ordering::Relaxed);
}

/// The striped commitments the proof under way (or the last one) has started.
pub fn commitments() -> u32 {
    COMMITMENTS.load(Ordering::Relaxed)
}

/// Where the count is in the module's memory: a u32, to be read atomically (`Atomics.load`) from
/// another worker while the proof runs.
#[wasm_bindgen]
pub fn progress_address() -> u32 {
    COMMITMENTS.as_ptr() as usize as u32
}
