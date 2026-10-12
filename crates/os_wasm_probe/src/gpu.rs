//! The striped commitment on the GPU (feature `gpu`, natively): stwo-gpu-spike's [`GpuCommitter`]
//! as stwo's striped-commit hook for `(SimdBackend, Blake2sM31MerkleChannel)`, the backend and
//! channel of both legs of the proof (the Cairo proof and the circuit proof) and of their
//! preprocessed trees. Every tree `CommitmentTreeProver::new_striped` commits on its striped path
//! goes to the GPU: in a privacy proof, all eight (each leg's preprocessed, base, interaction and
//! composition trees). A tree `new_striped` hands to `new` (columns under 2^4 coefficients, a
//! lifting size too small) and the FRI layers, which stwo's FRI prover hashes itself, stay on the
//! CPU.
//!
//! Off by default. On with `OUTIS_GPU=1` in the environment (read at the first proof, unless
//! [`set_gpu`] decided before), or [`set_gpu`] (the C entry point `tx_prove_set_gpu`). One
//! committer per process, created the first time the GPU is turned on; if that fails (no adapter,
//! no device), the proof stays on the CPU. A commitment the GPU declines or fails is made on the
//! CPU. The GPU's tree is `new_striped`'s bit for bit, so the proof is the same either way.
//!
//! The adapter is wgpu's choice from the environment: `WGPU_BACKEND=vulkan|metal|dx12|gl`,
//! `WGPU_ADAPTER_NAME=<part of the name>` (see stwo-gpu-spike). `OUTIS_GPU_DECLINE=all` or
//! `=0,3,...` (the hook's calls in this process, from 0) makes the hook decline those
//! commitments without asking the GPU, to check the fallback.
//!
//! Each commitment is a tracing event (target `os_wasm_probe::gpu`): GPU or CPU, why, and how
//! long; [`take_commitments`] returns them.
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use stwo::core::vcs_lifted::blake2_merkle::Blake2sM31MerkleChannel;
use stwo::prover::backend::simd::SimdBackend;
use stwo::prover::{set_striped_commit_hook, StripedCommitHook};
use stwo_gpu_spike::committer::{Declined, GpuCommitter};

/// The process's committer once the GPU was first turned on, or why there is none.
static COMMITTER: OnceLock<Result<Arc<GpuCommitter>, String>> = OnceLock::new();
/// Whether [`set_gpu`] was called; the environment is then not read.
static DECIDED: Mutex<bool> = Mutex::new(false);
/// The hook's calls so far, for `OUTIS_GPU_DECLINE`.
static CALLS: AtomicUsize = AtomicUsize::new(0);
/// The commitments since the last [`take_commitments`], the latest [`MAX_COMMITMENTS`].
static COMMITMENTS: Mutex<Vec<Commitment>> = Mutex::new(Vec::new());
const MAX_COMMITMENTS: usize = 64;

/// One striped commitment the hook was asked for.
#[derive(Clone, Debug)]
pub struct Commitment {
    /// The hook's call in this process, from 0.
    pub index: usize,
    pub columns: usize,
    /// The largest column's coefficients log size.
    pub max_log_size: u32,
    pub log_blowup: u32,
    pub lifting_log_size: u32,
    /// Committed on the GPU; otherwise given back to `new_striped` for the CPU.
    pub on_gpu: bool,
    /// Why not on the GPU.
    pub declined: Option<String>,
    /// Time in the hook (the GPU's commitment, or its attempt), in seconds. A declined
    /// commitment's CPU time comes after, in `new_striped`'s "Striped commitment" span.
    pub seconds: f64,
}

/// Turns the GPU commitment on or off for the commitments from now on, in every thread. Turning it
/// on creates the process's committer if there is none yet; returns the adapter, e.g.
/// "NVIDIA GeForce RTX 3080 (Vulkan)", or why there is none (and stays on the CPU).
pub fn set_gpu(enabled: bool) -> Result<Option<String>, String> {
    let mut decided = DECIDED.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    *decided = true;
    apply(enabled)
}

/// `OUTIS_GPU` from the environment, unless [`set_gpu`] decided already: `1`, `true` or `on`
/// turns the GPU on (once per process). Called before each proof.
pub fn apply_env() {
    let mut decided = DECIDED.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if *decided {
        return;
    }
    *decided = true;
    if env_enabled() {
        // A failure is logged, and the proof stays on the CPU.
        let _ = apply(true);
    }
}

/// Whether `OUTIS_GPU` asks for the GPU: `1`, `true` or `on`.
pub fn env_enabled() -> bool {
    std::env::var("OUTIS_GPU")
        .is_ok_and(|value| matches!(value.to_ascii_lowercase().as_str(), "1" | "true" | "on"))
}

/// Stops the committer's keep-warm thread, if there is a committer: no more GPU work in the
/// background. Call it before the process exits: the driver torn down under a keep-warm submission
/// crashed os_native_probe at exit. Commitments still work after it, without keep-warm.
pub fn shutdown() {
    if let Some(Ok(committer)) = COMMITTER.get() {
        committer.shutdown();
    }
}

fn apply(enabled: bool) -> Result<Option<String>, String> {
    if !enabled {
        set_striped_commit_hook::<SimdBackend, Blake2sM31MerkleChannel>(None);
        shutdown();
        tracing::info!("GPU commitment off");
        return Ok(None);
    }
    let committer = COMMITTER.get_or_init(|| {
        let started = Instant::now();
        // wgpu panics, rather than failing, on a WGPU_ADAPTER_NAME no adapter matches.
        let committer = std::panic::catch_unwind(GpuCommitter::new)
            .unwrap_or_else(|panic| Err(panic_message(&panic)))
            .map(Arc::new);
        match &committer {
            Ok(committer) => tracing::info!(
                "GPU committer on {} in {:.2} s",
                describe(committer),
                started.elapsed().as_secs_f64()
            ),
            Err(error) => tracing::warn!("no GPU committer, the commitments stay on the CPU: {error}"),
        }
        committer
    });
    let committer = committer.clone()?;
    let adapter = describe(&committer);
    set_striped_commit_hook::<SimdBackend, Blake2sM31MerkleChannel>(Some(hook(committer)));
    tracing::info!("GPU commitment on: {adapter}");
    Ok(Some(adapter))
}

/// The committer's adapter, as "name (backend)".
fn describe(committer: &GpuCommitter) -> String {
    let info = committer.adapter_info();
    format!("{} ({:?})", info.name, info.backend)
}

fn panic_message(panic: &Box<dyn std::any::Any + Send>) -> String {
    let message = panic
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| panic.downcast_ref::<&str>().copied())
        .unwrap_or("no message");
    format!("panic: {message}")
}

/// The commitments since the last call, in order.
pub fn take_commitments() -> Vec<Commitment> {
    std::mem::take(&mut *COMMITMENTS.lock().unwrap_or_else(|poisoned| poisoned.into_inner()))
}

/// Whether `OUTIS_GPU_DECLINE` names the hook's call `index`.
fn forced_decline(index: usize) -> bool {
    let Ok(decline) = std::env::var("OUTIS_GPU_DECLINE") else {
        return false;
    };
    decline.trim() == "all"
        || decline.split(',').any(|entry| entry.trim().parse::<usize>() == Ok(index))
}

fn hook(committer: Arc<GpuCommitter>) -> Arc<StripedCommitHook<SimdBackend, Blake2sM31MerkleChannel>> {
    Arc::new(move |polynomials, log_blowup, twiddles, lifting_log_size| {
        let index = CALLS.fetch_add(1, Ordering::Relaxed);
        let columns = polynomials.len();
        let max_log_size = polynomials.iter().map(|poly| poly.log_size()).max().unwrap_or(0);
        let started = Instant::now();
        let (result, declined) = if forced_decline(index) {
            (Err(polynomials), Some("OUTIS_GPU_DECLINE".to_string()))
        } else {
            match committer.commit_striped(polynomials, log_blowup, twiddles, lifting_log_size) {
                Ok(tree) => (Ok(tree), None),
                Err((polynomials, reason)) => {
                    let reason = match reason {
                        Declined::NotStriped => "not striped".to_string(),
                        Declined::Gpu(error) => format!("GPU failed: {error}"),
                    };
                    (Err(polynomials), Some(reason))
                }
            }
        };
        let seconds = started.elapsed().as_secs_f64();
        let shape = format!(
            "#{index}: {columns} columns up to 2^{max_log_size}, blowup 2^{log_blowup}, lifted to 2^{lifting_log_size}"
        );
        match &declined {
            None => tracing::info!("commitment {shape}: GPU, {:.3} s", seconds),
            Some(reason) => tracing::warn!("commitment {shape}: CPU ({reason}) after {:.3} s", seconds),
        }
        let mut commitments = COMMITMENTS.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        // The latest only, for a process nobody takes them from (an app's).
        if commitments.len() >= MAX_COMMITMENTS {
            commitments.remove(0);
        }
        commitments.push(Commitment {
            index,
            columns,
            max_log_size,
            log_blowup,
            lifting_log_size,
            on_gpu: declined.is_none(),
            declined,
            seconds,
        });
        drop(commitments);
        result
    })
}
