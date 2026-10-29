//! The striped commitment on the GPU (feature `gpu`, natively): stwo-gpu-spike's [`GpuCommitter`]
//! as stwo's striped-commit hook for `(SimdBackend, Blake2sMerkleChannel)`, the backend and
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
//! Keep-warm (the committer keeping the GPU's clocks up for a window after each commitment, see
//! stwo-gpu-spike's `warm`): off by default, on with `STWO_GPU_KEEP_WARM_MS=<ms>` in the
//! environment or [`set_keep_warm`] (the C entry point `tx_prove_set_gpu_keep_warm`).
//!
//! Each commitment is a tracing event (target `os_wasm_probe::gpu`): GPU or CPU, why, and how
//! long; [`take_commitments`] returns them.
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use stwo::core::poly::circle::CanonicCoset;
use stwo::core::vcs_lifted::blake2_merkle::Blake2sMerkleChannel;
use stwo::prover::backend::simd::SimdBackend;
use stwo::prover::poly::circle::{part_domain, PolyOps};
use stwo::core::fields::m31::BaseField;
use stwo::prover::poly::BitReversedOrder;
use stwo::prover::poly::circle::CircleEvaluation;
use stwo::prover::{
    set_fri_quotients_hook, set_ood_sample_hook, set_striped_commit_hook, set_striped_decommit_hook,
    FriQuotientsRequest, OodSampleGroup, Poly, StripedCommitHook,
};
use stwo_constraint_framework::{set_trace_part_hook, TracePartHook, TracePartRequest};
use stwo_constraint_framework::constraint_hook::set_constraint_eval_hook;
use stwo_gpu_spike::committer::{Declined, GpuCommitter};
use stwo_gpu_spike::constraint::composer::{default_library, ComposerConfig, ComposerStats, GpuComposer};
use stwo_gpu_spike::parts::{PartStats, PartsMode};

/// The process's committer once the GPU was first turned on, or why there is none.
static COMMITTER: OnceLock<Result<Arc<GpuCommitter>, String>> = OnceLock::new();
/// Whether [`set_gpu`] was called; the environment is then not read.
static DECIDED: Mutex<bool> = Mutex::new(false);
/// The hook's calls so far, for `OUTIS_GPU_DECLINE`.
static CALLS: AtomicUsize = AtomicUsize::new(0);
/// The commitments since the last [`take_commitments`], the latest [`MAX_COMMITMENTS`].
static COMMITMENTS: Mutex<Vec<Commitment>> = Mutex::new(Vec::new());
const MAX_COMMITMENTS: usize = 64;
/// The keep-warm window [`set_keep_warm`] set (`None` until called: the environment's).
static KEEP_WARM: Mutex<Option<Option<Duration>>> = Mutex::new(None);

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

/// Sets how long the committer keeps the GPU's clocks up after each commitment, in milliseconds (0:
/// never, the default unless `STWO_GPU_KEEP_WARM_MS` says otherwise), for the commitments from now
/// on: now if there is a committer, or when the GPU is first turned on. A GPU that was turned off
/// is not kept warm again in this process (see [`shutdown`]).
pub fn set_keep_warm(milliseconds: u32) {
    let window = (milliseconds > 0).then(|| Duration::from_millis(milliseconds.into()));
    *KEEP_WARM.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(window);
    if let Some(Ok(committer)) = COMMITTER.get() {
        committer.set_keep_warm(window);
    }
    tracing::info!("GPU keep-warm: {milliseconds} ms");
}

/// Stops the committer's keep-warm thread, if there is a committer: no more GPU work in the
/// background. Call it before the process exits: the driver torn down under a keep-warm submission
/// crashed os_native_probe at exit. Commitments still work after it, without keep-warm.
pub fn shutdown() {
    if let Some(Ok(committer)) = COMMITTER.get() {
        committer.shutdown();
    }
    if let Some(composer) = COMPOSER.get() {
        composer.shutdown();
    }
}

/// Constraint evaluation on the GPU (the composition, see stwo-gpu-spike's `composer`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Composition {
    Off,
    /// The components the GPU takes are evaluated there.
    On,
    /// Evaluated on the GPU and on the CPU, compared; the CPU's result is used.
    Shadow,
}

/// The composition mode [`set_composition`] set (`None` until called: the environment's).
static COMPOSITION: Mutex<Option<Composition>> = Mutex::new(None);
/// The process's composer, once composition was first turned on with the GPU.
static COMPOSER: OnceLock<Arc<GpuComposer>> = OnceLock::new();
/// Whether the GPU is on (its hooks registered).
static GPU_ON: Mutex<bool> = Mutex::new(false);

/// The composition mode: [`set_composition`]'s, else `OUTIS_GPU_COMPOSITION` (`1`, `on`, `true`;
/// `shadow`), else off.
pub fn composition() -> Composition {
    if let Some(mode) = *COMPOSITION.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) {
        return mode;
    }
    match std::env::var("OUTIS_GPU_COMPOSITION").unwrap_or_default().to_ascii_lowercase().as_str() {
        "1" | "on" | "true" => Composition::On,
        "shadow" => Composition::Shadow,
        _ => Composition::Off,
    }
}

/// Sets the composition mode for the proofs from now on (off by default): with the GPU on, the
/// components of every proof's composition the GPU takes are evaluated there (`On`), or there and
/// on the CPU, compared (`Shadow`). Only with the GPU on (see [`set_gpu`]); takes effect now if it
/// is, otherwise when it is turned on.
pub fn set_composition(mode: Composition) {
    *COMPOSITION.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(mode);
    let gpu_on = *GPU_ON.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if let (true, Some(Ok(committer))) = (gpu_on, COMMITTER.get()) {
        apply_composition(committer);
    }
    tracing::info!("GPU composition: {mode:?}");
}

/// The composer's counts since the last call, and its kernels' (ready, compiling, failed).
pub fn take_composition_stats() -> Option<(ComposerStats, (usize, usize, usize))> {
    let composer = COMPOSER.get()?;
    Some((composer.take_stats(), composer.kernel_counts()))
}

/// Waits until no constraint kernel is compiling, at most `timeout` (for tests: a proof never
/// waits for them itself).
pub fn wait_composition_compiled(timeout: Duration) -> bool {
    COMPOSER.get().is_none_or(|composer| composer.wait_compiled(timeout))
}

/// The committer's device and the limits that bound its buffers, e.g. "Apple A14 GPU (Metal): max
/// buffer 256 MiB, max storage binding 256 MiB"; `None` without a committer.
pub fn device_report() -> Option<String> {
    let Some(Ok(committer)) = COMMITTER.get() else { return None };
    let limits = committer.gpu().device.limits();
    Some(format!(
        "{}: max buffer {} MiB, max storage binding {} MiB",
        describe(committer),
        limits.max_buffer_size >> 20,
        limits.max_storage_buffer_binding_size >> 20
    ))
}

/// The device's video memory and this process's bound on it, e.g. "device-local 10240 MiB,
/// driver budget 9500 MiB (using 1200), process bound 4096 MiB (40%), holding 1100 MiB"; `None`
/// without a committer.
pub fn vram_report() -> Option<String> {
    let Some(Ok(committer)) = COMMITTER.get() else { return None };
    let (memory, reserved, bound) = committer.vram_report();
    let mib = |bytes: u64| bytes >> 20;
    let policy = committer.vram_policy();
    let mut report = match memory {
        Some(memory) => format!("device-local {} MiB", mib(memory.local_heap)),
        None => "device memory unknown".to_string(),
    };
    if let Some(memory) = memory {
        if let (Some(budget), Some(usage)) = (memory.budget, memory.usage) {
            report += &format!(", driver budget {} MiB (using {})", mib(budget), mib(usage));
        }
    }
    report += &format!(", process bound {} MiB ({}%)", mib(bound), policy.process_percent);
    if let Some(cap) = policy.cap {
        report += &format!(", composition cap {} MiB", mib(cap));
    }
    if let Some(reserved) = reserved {
        report += &format!(", holding {} MiB", mib(reserved));
    }
    let held = committer.held_bytes();
    report += &format!(
        " (commitment buffers {} MiB, twiddles {}, upload ring {}, readback ring {}; parts {})",
        mib(held.buffers),
        mib(held.twiddles),
        mib(held.upload),
        mib(held.readback),
        mib(committer.parts_held_bytes()),
    );
    Some(report)
}

/// What the commitments keep resident for the GPU's constraint evaluation (and decommitment):
/// the columns' coefficients (the default), or `OUTIS_GPU_RESIDENT=stripes`, their stripes 0 and 1.
fn resident_mode() -> PartsMode {
    match std::env::var("OUTIS_GPU_RESIDENT").as_deref() {
        Ok("stripes") => PartsMode::Resident,
        _ => PartsMode::Coefficients,
    }
}

/// The decommitment on the GPU (stwo's striped decommitment hook, see stwo-gpu-spike's
/// `decommit`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decommit {
    Off,
    /// The opened values the tree's columns don't hold are evaluated on the GPU from the
    /// coefficients the commitments keep.
    On,
    /// Evaluated on the GPU and on the CPU, compared; the CPU's values are used.
    Shadow,
}

/// The decommitment mode [`set_decommit`] set (`None` until called: the environment's).
static DECOMMIT: Mutex<Option<Decommit>> = Mutex::new(None);
/// Shadow mode's counts: (columns compared, values that differ).
static DECOMMIT_SHADOW: Mutex<(u64, u64)> = Mutex::new((0, 0));
/// The decommitment hook's calls so far (for `OUTIS_GPU_DECOMMIT_TAMPER=call:N`).
static DECOMMIT_CALLS: AtomicUsize = AtomicUsize::new(0);

/// The decommitment mode: [`set_decommit`]'s, else `OUTIS_GPU_DECOMMIT` (`1`, `on`, `true`;
/// `shadow`), else off.
pub fn decommit() -> Decommit {
    if let Some(mode) = *DECOMMIT.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) {
        return mode;
    }
    match std::env::var("OUTIS_GPU_DECOMMIT").unwrap_or_default().to_ascii_lowercase().as_str() {
        "1" | "on" | "true" => Decommit::On,
        "shadow" => Decommit::Shadow,
        _ => Decommit::Off,
    }
}

/// Sets the decommitment mode for the proofs from now on (off by default), with the GPU on; the
/// commitments then keep their columns' coefficients until their decommitment. Takes effect now
/// if the GPU is on, otherwise when it is turned on.
pub fn set_decommit(mode: Decommit) {
    *DECOMMIT.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(mode);
    let gpu_on = *GPU_ON.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if let (true, Some(Ok(committer))) = (gpu_on, COMMITTER.get()) {
        apply_decommit(committer);
    }
    tracing::info!("GPU decommitment: {mode:?}");
}

/// The decommitments' counts since the last call, and shadow mode's (columns compared, values
/// that differ).
pub fn take_decommit_stats() -> Option<(stwo_gpu_spike::committer::DecommitStats, (u64, u64))> {
    let Some(Ok(committer)) = COMMITTER.get() else { return None };
    let shadow = std::mem::take(&mut *DECOMMIT_SHADOW.lock().unwrap_or_else(|p| p.into_inner()));
    Some((committer.take_decommit_stats(), shadow))
}

/// The CPU's values of the decommitment (as stwo's `decommit_from_stripes` computes them).
fn cpu_decommit_values(
    columns: &[&CircleEvaluation<SimdBackend, BaseField, BitReversedOrder>],
    wanted: &[Vec<usize>],
) -> Vec<Vec<BaseField>> {
    columns
        .iter()
        .zip(wanted)
        .map(|(column, wanted)| {
            if wanted.is_empty() {
                return vec![];
            }
            let poly = Poly::new(None, (*column).clone());
            let twiddles = SimdBackend::precompute_twiddles(poly.stripe_domain().half_coset);
            let coefficients = poly.stripe_coefficients(&twiddles);
            SimdBackend::eval_at_domain_indices(&coefficients, column.domain, wanted)
        })
        .collect()
}

/// Registers (or removes) the decommitment's hook as [`decommit`] says, on `committer`.
fn apply_decommit(committer: &Arc<GpuCommitter>) {
    let mode = decommit();
    if mode == Decommit::Off {
        set_striped_decommit_hook::<SimdBackend>(None);
        committer.set_decommit(false);
        // Nothing keeps the coefficients any more unless the composition reads them.
        if composition() == Composition::Off
            && matches!(committer.parts_mode(), PartsMode::Resident | PartsMode::Coefficients)
        {
            committer.set_parts_mode(PartsMode::from_env());
            committer.clear_parts();
        }
        apply_ood(committer);
        apply_fri(committer);
        return;
    }
    // The coefficients the decommitments read (composition's mode, or set here).
    if committer.parts_mode() != PartsMode::Coefficients {
        committer.set_parts_mode(PartsMode::Coefficients);
    }
    committer.set_decommit(true);
    // Tests of the proof's checks: `1` changes the first opened value of every decommitment,
    // `call:N` every opened value of the N-th one (from 1, counted over the process).
    // The out-of-domain samples and the FRI quotients read the coefficients kept for the
    // decommitment.
    apply_ood(committer);
    apply_fri(committer);
    let tamper = std::env::var("OUTIS_GPU_DECOMMIT_TAMPER").ok();
    let tamper_call = tamper.as_deref().and_then(|v| v.strip_prefix("call:")?.parse::<usize>().ok());
    let tamper_first = tamper.as_deref() == Some("1");
    let committer = committer.clone();
    set_striped_decommit_hook::<SimdBackend>(Some(Arc::new(move |columns, wanted| {
        let call = DECOMMIT_CALLS.fetch_add(1, Ordering::Relaxed) + 1;
        let mut values = committer.decommit_values(columns, wanted)?;
        match mode {
            Decommit::Shadow => {
                let cpu = cpu_decommit_values(columns, wanted);
                let different: u64 = cpu
                    .iter()
                    .zip(&values)
                    .map(|(a, b)| {
                        a.iter().zip(b).filter(|(x, y)| x.0 != y.0).count() as u64
                            + a.len().abs_diff(b.len()) as u64
                    })
                    .sum();
                if different > 0 {
                    tracing::warn!("decommitment shadow: {different} values differ");
                }
                let mut shadow = DECOMMIT_SHADOW.lock().unwrap_or_else(|p| p.into_inner());
                shadow.0 += wanted.iter().filter(|w| !w.is_empty()).count() as u64;
                shadow.1 += different;
                None
            }
            _ => {
                let bump = |v: &mut BaseField| *v = BaseField::from_u32_unchecked((v.0 + 1) % ((1 << 31) - 1));
                if tamper_first && let Some(first) = values.iter_mut().find(|v| !v.is_empty()) {
                    bump(&mut first[0]);
                }
                if tamper_call == Some(call) {
                    values.iter_mut().flatten().for_each(bump);
                }
                Some(values)
            }
        }
    })));
}

/// The out-of-domain samples on the GPU (stwo's out-of-domain sample hook, see stwo-gpu-spike's
/// `ood`). They read the coefficients kept until the decommitment: only with the decommitment on
/// the GPU (or in its shadow mode).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ood {
    Off,
    /// The samples of the columns kept as coefficients are evaluated on the GPU.
    On,
    /// Evaluated on the GPU and on the CPU, compared; the CPU's values are used.
    Shadow,
}

/// The out-of-domain sample mode [`set_ood`] set (`None` until called: the environment's).
static OOD: Mutex<Option<Ood>> = Mutex::new(None);
/// Shadow mode's counts: (values compared, values that differ).
static OOD_SHADOW: Mutex<(u64, u64)> = Mutex::new((0, 0));
/// The sample hook's calls so far (for `OUTIS_GPU_OOD_TAMPER=call:N`).
static OOD_CALLS: AtomicUsize = AtomicUsize::new(0);

/// The out-of-domain sample mode: [`set_ood`]'s, else `OUTIS_GPU_OOD` (`1`, `on`, `true`;
/// `shadow`), else off.
pub fn ood() -> Ood {
    if let Some(mode) = *OOD.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) {
        return mode;
    }
    match std::env::var("OUTIS_GPU_OOD").unwrap_or_default().to_ascii_lowercase().as_str() {
        "1" | "on" | "true" => Ood::On,
        "shadow" => Ood::Shadow,
        _ => Ood::Off,
    }
}

/// Sets the out-of-domain sample mode for the proofs from now on (off by default), with the GPU
/// and its decommitment on. Takes effect now if the GPU is on, otherwise when it is turned on.
pub fn set_ood(mode: Ood) {
    *OOD.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(mode);
    let gpu_on = *GPU_ON.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if let (true, Some(Ok(committer))) = (gpu_on, COMMITTER.get()) {
        apply_ood(committer);
    }
    tracing::info!("GPU out-of-domain samples: {mode:?}");
}

/// The out-of-domain samples' counts since the last call, and shadow mode's (values compared,
/// values that differ).
pub fn take_ood_stats() -> Option<(stwo_gpu_spike::committer::OodStats, (u64, u64))> {
    let Some(Ok(committer)) = COMMITTER.get() else { return None };
    let shadow = std::mem::take(&mut *OOD_SHADOW.lock().unwrap_or_else(|p| p.into_inner()));
    Some((committer.take_ood_stats(), shadow))
}

/// The CPU's value of a group's column (as stwo's out-of-domain sampling computes it).
fn cpu_ood_value(
    group: &OodSampleGroup<'_, SimdBackend>,
    column: &CircleEvaluation<SimdBackend, BaseField, BitReversedOrder>,
) -> stwo::core::fields::qm31::SecureField {
    let log_blowup = column.domain.log_size() - stwo::prover::backend::Column::len(&column.values).ilog2();
    SimdBackend::subdomain_eval_group(
        CanonicCoset::new(group.log_size),
        log_blowup,
        group.point,
        &[column],
    )[0]
}

/// Registers (or removes) the out-of-domain sample hook as [`ood`] and [`decommit`] say, on
/// `committer`.
fn apply_ood(committer: &Arc<GpuCommitter>) {
    let mode = ood();
    if mode == Ood::Off || decommit() == Decommit::Off {
        set_ood_sample_hook::<SimdBackend>(None);
        return;
    }
    // Tests of the proof's checks: every value the N-th sampling (from 1, counted over the
    // process) answers on the GPU changed.
    let tamper_call = std::env::var("OUTIS_GPU_OOD_TAMPER")
        .ok()
        .and_then(|v| v.strip_prefix("call:")?.parse::<usize>().ok());
    let committer = committer.clone();
    set_ood_sample_hook::<SimdBackend>(Some(Arc::new(move |groups| {
        let call = OOD_CALLS.fetch_add(1, Ordering::Relaxed) + 1;
        let mut answers = committer.ood_values(groups);
        match mode {
            Ood::Shadow => {
                let (mut compared, mut different) = (0u64, 0u64);
                for (group, answers) in groups.iter().zip(&answers) {
                    for (column, answer) in group.columns.iter().zip(answers) {
                        if let Some(value) = answer {
                            compared += 1;
                            if *value != cpu_ood_value(group, column) {
                                different += 1;
                            }
                        }
                    }
                }
                if different > 0 {
                    tracing::warn!("out-of-domain shadow: {different} values differ");
                }
                let mut shadow = OOD_SHADOW.lock().unwrap_or_else(|p| p.into_inner());
                shadow.0 += compared;
                shadow.1 += different;
                groups.iter().map(|group| vec![None; group.columns.len()]).collect()
            }
            _ => {
                if tamper_call == Some(call) {
                    let one = stwo::core::fields::qm31::SecureField::from(1u32);
                    answers.iter_mut().flatten().flatten().for_each(|value| *value += one);
                }
                answers
            }
        }
    })));
}

/// The FRI quotients on the GPU (stwo's FRI quotients hook, see stwo-gpu-spike's `fri`). They
/// read the coefficients kept until the decommitment: only with the decommitment on the GPU (or
/// in its shadow mode).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fri {
    Off,
    /// The quotients are computed on the GPU.
    On,
    /// Computed on the GPU and on the CPU, compared; the CPU's are used.
    Shadow,
}

/// The FRI quotients mode [`set_fri`] set (`None` until called: the environment's).
static FRI: Mutex<Option<Fri>> = Mutex::new(None);
/// Shadow mode's counts: (words compared, words that differ).
static FRI_SHADOW: Mutex<(u64, u64)> = Mutex::new((0, 0));
/// The FRI quotients hook's calls so far (for `OUTIS_GPU_FRI_TAMPER=call:N`).
static FRI_CALLS: AtomicUsize = AtomicUsize::new(0);

/// The FRI quotients mode: [`set_fri`]'s, else `OUTIS_GPU_FRI` (`1`, `on`, `true`; `shadow`),
/// else off.
pub fn fri() -> Fri {
    if let Some(mode) = *FRI.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) {
        return mode;
    }
    match std::env::var("OUTIS_GPU_FRI").unwrap_or_default().to_ascii_lowercase().as_str() {
        "1" | "on" | "true" => Fri::On,
        "shadow" => Fri::Shadow,
        _ => Fri::Off,
    }
}

/// Sets the FRI quotients mode for the proofs from now on (off by default), with the GPU and its
/// decommitment on. Takes effect now if the GPU is on, otherwise when it is turned on.
pub fn set_fri(mode: Fri) {
    *FRI.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(mode);
    let gpu_on = *GPU_ON.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if let (true, Some(Ok(committer))) = (gpu_on, COMMITTER.get()) {
        apply_fri(committer);
    }
    tracing::info!("GPU FRI quotients: {mode:?}");
}

/// The FRI quotients' counts since the last call, and shadow mode's (words compared, words that
/// differ).
pub fn take_fri_stats() -> Option<(stwo_gpu_spike::committer::FriStats, (u64, u64))> {
    let Some(Ok(committer)) = COMMITTER.get() else { return None };
    let shadow = std::mem::take(&mut *FRI_SHADOW.lock().unwrap_or_else(|p| p.into_inner()));
    Some((committer.take_fri_stats(), shadow))
}

/// Registers (or removes) the FRI quotients hook as [`fri`] and [`decommit`] say, on `committer`.
fn apply_fri(committer: &Arc<GpuCommitter>) {
    let mode = fri();
    if mode == Fri::Off || decommit() == Decommit::Off {
        set_fri_quotients_hook::<SimdBackend>(None);
        return;
    }
    // Tests of the proof's checks: a value of the N-th quotients (from 1, counted over the
    // process) the GPU computed changed.
    let tamper_call = std::env::var("OUTIS_GPU_FRI_TAMPER")
        .ok()
        .and_then(|v| v.strip_prefix("call:")?.parse::<usize>().ok());
    let committer = committer.clone();
    set_fri_quotients_hook::<SimdBackend>(Some(Arc::new(move |request: &FriQuotientsRequest<'_, SimdBackend>| {
        let call = FRI_CALLS.fetch_add(1, Ordering::Relaxed) + 1;
        let mut quotients = committer.fri_quotients(request)?;
        match mode {
            Fri::Shadow => {
                let cpu = stwo::prover::pcs::quotient_ops::compute_fri_quotients(
                    request.columns,
                    request.samples,
                    request.random_coeff,
                    request.lifting_log_size,
                    request.twiddles,
                    request.log_blowup_factor,
                );
                let (mut compared, mut different) = (0u64, 0u64);
                for (gpu, cpu) in quotients.values.columns.iter().zip(&cpu.values.columns) {
                    compared += gpu.length as u64;
                    different += gpu
                        .as_slice()
                        .iter()
                        .zip(cpu.as_slice())
                        .filter(|(a, b)| a.0 != b.0)
                        .count() as u64;
                }
                if different > 0 {
                    tracing::warn!("FRI quotients shadow: {different} words differ");
                }
                let mut shadow = FRI_SHADOW.lock().unwrap_or_else(|p| p.into_inner());
                shadow.0 += compared;
                shadow.1 += different;
                None
            }
            _ => {
                if tamper_call == Some(call) {
                    use stwo::prover::backend::Column;
                    let column = &mut quotients.values.columns[0];
                    let value = column.at(0);
                    column.set(0, value + BaseField::from(1u32));
                }
                Some(quotients)
            }
        }
    })));
}

/// Registers (or removes) the composition's hooks as [`composition`] says, on `committer`.
fn apply_composition(committer: &Arc<GpuCommitter>) {
    let mode = composition();
    if mode == Composition::Off {
        set_constraint_eval_hook(None);
        if matches!(committer.parts_mode(), PartsMode::Resident | PartsMode::Coefficients) {
            committer.set_parts_mode(PartsMode::from_env());
            committer.clear_parts();
            if committer.parts_mode() == PartsMode::Off {
                set_trace_part_hook(None);
            }
        }
        return;
    }
    // The columns stay resident on the GPU (stripes 0 and 1), within the budget; stripe 1 serves
    // the components the CPU evaluates.
    // (The video memory they take is bounded by the committer's policy: by default the whole
    // process at most 40% of the device, OUTIS_GPU_PROCESS_VRAM_PERCENT, and
    // OUTIS_GPU_COMPOSITION_VRAM_MIB on what is kept; past it a tree isn't kept and its
    // components are evaluated on the CPU.)
    committer.set_parts_mode(resident_mode());
    set_trace_part_hook(Some(part_hook(committer.clone())));
    let only = std::env::var("OUTIS_GPU_COMPOSITION_ONLY")
        .map(|o| o.split(',').filter(|s| !s.is_empty()).map(str::to_owned).collect())
        .unwrap_or_default();
    let composer = COMPOSER
        .get_or_init(|| {
            GpuComposer::new(
                committer.clone(),
                ComposerConfig {
                    library: default_library(),
                    shadow: mode == Composition::Shadow,
                    only,
                    ..Default::default()
                },
            )
        })
        .clone();
    composer.set_shadow(mode == Composition::Shadow);
    set_constraint_eval_hook(Some(Arc::new(move |job, accum| composer.evaluate(job, accum))));
}

fn apply(enabled: bool) -> Result<Option<String>, String> {
    *GPU_ON.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = enabled;
    if !enabled {
        set_striped_commit_hook::<SimdBackend, Blake2sMerkleChannel>(None);
        set_trace_part_hook(None);
        set_constraint_eval_hook(None);
        set_striped_decommit_hook::<SimdBackend>(None);
        set_ood_sample_hook::<SimdBackend>(None);
        set_fri_quotients_hook::<SimdBackend>(None);
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
    if let Some(window) = *KEEP_WARM.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) {
        committer.set_keep_warm(window);
    }
    let adapter = describe(&committer);
    // The part extension's hook only with a mode (`STWO_GPU_PARTS`, a spike): without one,
    // constraint evaluation is exactly as without the GPU.
    let parts_mode = committer.parts_mode();
    if parts_mode != PartsMode::Off {
        set_trace_part_hook(Some(part_hook(committer.clone())));
        tracing::info!("GPU part extension: {parts_mode:?}");
    }
    // Constraint evaluation on the GPU only with a mode (`OUTIS_GPU_COMPOSITION`,
    // `set_composition`): off by default.
    if composition() != Composition::Off {
        apply_composition(&committer);
    }
    // The decommitment on the GPU only with a mode (`OUTIS_GPU_DECOMMIT`, `set_decommit`).
    if decommit() != Decommit::Off {
        apply_decommit(&committer);
    }
    // The out-of-domain samples on the GPU only with a mode (`OUTIS_GPU_OOD`, `set_ood`) and the
    // decommitment's.
    apply_ood(&committer);
    // The FRI quotients on the GPU only with a mode (`OUTIS_GPU_FRI`, `set_fri`) and the
    // decommitment's.
    apply_fri(&committer);
    set_striped_commit_hook::<SimdBackend, Blake2sMerkleChannel>(Some(hook(committer)));
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

fn hook(committer: Arc<GpuCommitter>) -> Arc<StripedCommitHook<SimdBackend, Blake2sMerkleChannel>> {
    Arc::new(move |polynomials, log_blowup, twiddles, lifting_log_size| {
        let index = CALLS.fetch_add(1, Ordering::Relaxed);
        let columns = polynomials.len();
        let max_log_size = polynomials.iter().map(|poly| poly.log_size()).max().unwrap_or(0);
        let started = Instant::now();
        let (result, declined) = if forced_decline(index) {
            // A tree committed elsewhere: what the GPU kept of earlier trees goes.
            committer.clear_parts();
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

/// Whether the proofs use a GPU hook beyond the commitment (composition, decommitment or
/// out-of-domain samples, in any mode but off): then the prover verifies its own proof before
/// returning it, and proves again without them if that fails (see `TxSession::prove`).
/// `OUTIS_GPU_SELF_VERIFY=0` turns that off, for tests of what the hooks' faults do to a proof.
pub fn self_verify_needed() -> bool {
    let gpu_on = *GPU_ON.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let off_for_tests = std::env::var("OUTIS_GPU_SELF_VERIFY").is_ok_and(|v| v == "0");
    gpu_on
        && !off_for_tests
        && matches!(COMMITTER.get(), Some(Ok(_)))
        && (composition() != Composition::Off
            || decommit() != Decommit::Off
            || ood() != Ood::Off
            || fri() != Fri::Off)
}

/// The modes [`suspend_hooks`] turned off, put back when dropped.
pub struct HooksSuspended {
    composition: Option<Composition>,
    decommit: Option<Decommit>,
    ood: Option<Ood>,
    fri: Option<Fri>,
}

/// Turns the GPU hooks beyond the commitment (composition, decommitment, out-of-domain samples)
/// off until the guard drops; then they are as they were (the modes set, or the environment's).
pub fn suspend_hooks() -> HooksSuspended {
    fn get<T: Copy>(mode: &Mutex<Option<T>>) -> Option<T> {
        *mode.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
    let saved = HooksSuspended {
        composition: get(&COMPOSITION),
        decommit: get(&DECOMMIT),
        ood: get(&OOD),
        fri: get(&FRI),
    };
    set_fri(Fri::Off);
    set_ood(Ood::Off);
    set_decommit(Decommit::Off);
    set_composition(Composition::Off);
    saved
}

impl Drop for HooksSuspended {
    fn drop(&mut self) {
        *COMPOSITION.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = self.composition;
        *DECOMMIT.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = self.decommit;
        *OOD.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = self.ood;
        *FRI.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = self.fri;
        let gpu_on = *GPU_ON.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if let (true, Some(Ok(committer))) = (gpu_on, COMMITTER.get()) {
            // As `apply` does: the composition, then the decommitment (and the samples).
            if composition() != Composition::Off {
                apply_composition(committer);
            }
            apply_decommit(committer);
        }
        tracing::info!(
            "GPU hooks back: composition {:?}, decommitment {:?}, out-of-domain samples {:?}, \
             FRI quotients {:?}",
            composition(),
            decommit(),
            ood(),
            fri()
        );
    }
}

/// A proof's self-verification (see `TxSession::prove`).
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct SelfVerify {
    /// Whether the proof returned verified (the retry's, if there was one).
    pub verified: bool,
    /// Why the first proof was proved again with the hooks off: it failed verification, or the
    /// prover failed; `None` if it verified.
    pub retried: Option<String>,
    /// The verifications' time, and the retry's (its proof and verification).
    pub verify_s: f64,
    pub retry_s: f64,
}

/// The last proof's self-verification, until taken.
static SELF_VERIFY: Mutex<Option<SelfVerify>> = Mutex::new(None);

pub fn record_self_verify(report: SelfVerify) {
    *SELF_VERIFY.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(report);
}

/// The last proof's self-verification since the last call (`None` if it had none).
pub fn take_self_verify() -> Option<SelfVerify> {
    SELF_VERIFY.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).take()
}

/// Drops what the committer kept of an earlier proof's trees for the part extension (a proof
/// cancelled between its commitments and its constraint evaluation leaves them).
pub fn proof_starts() {
    if let Some(Ok(committer)) = COMMITTER.get() {
        committer.clear_parts();
    }
}

/// The part extension's lookups since the last call (see stwo-gpu-spike's `parts`), and the
/// check's counts (`STWO_GPU_PARTS_CHECK=1`).
pub fn take_part_stats() -> Option<(PartStats, PartCheck)> {
    let Some(Ok(committer)) = COMMITTER.get() else { return None };
    let check = std::mem::take(&mut *PART_CHECK.lock().unwrap_or_else(|poisoned| poisoned.into_inner()));
    Some((committer.take_part_stats(), check))
}

/// What `STWO_GPU_PARTS_CHECK=1` found: every column the GPU provided compared, raw word for
/// raw word, with the CPU's evaluation of it (constraint-framework's `trace_part`'s calls).
#[derive(Clone, Copy, Debug, Default)]
pub struct PartCheck {
    pub columns: u64,
    pub words: u64,
    /// Columns with a word that differs.
    pub different_columns: u64,
    pub different_words: u64,
    /// Words `>= P` (not canonical) in the CPU's evaluation, and in the GPU's.
    pub cpu_words_ge_p: u64,
    pub gpu_words_ge_p: u64,
    /// Words equal to P in the CPU's, and in the GPU's.
    pub cpu_words_eq_p: u64,
    pub gpu_words_eq_p: u64,
    /// Columns the GPU didn't provide.
    pub declined: u64,
    /// Columns whose stripe 0 is all zeros (so is their stripe 1), and their words.
    pub zero_columns: u64,
    pub zero_words: u64,
    pub seconds: f64,
}

static PART_CHECK: Mutex<PartCheck> = Mutex::new(PartCheck {
    columns: 0,
    words: 0,
    different_columns: 0,
    different_words: 0,
    cpu_words_ge_p: 0,
    gpu_words_ge_p: 0,
    cpu_words_eq_p: 0,
    gpu_words_eq_p: 0,
    declined: 0,
    zero_columns: 0,
    zero_words: 0,
    seconds: 0.0,
});

fn part_check_enabled() -> bool {
    std::env::var("STWO_GPU_PARTS_CHECK").is_ok_and(|value| value == "1")
}

/// Compares the GPU's answers with the CPU's evaluation, exactly as constraint-framework's
/// `trace_part` computes it for a column without coefficients.
fn check_parts(
    request: &TracePartRequest<'_>,
    answers: &[Option<stwo::prover::backend::simd::column::BaseColumn>],
) {
    const P: u32 = (1 << 31) - 1;
    let started = Instant::now();
    let mut check = PartCheck::default();
    // As trace_part: the committed domain, the first part's twiddles, the part's.
    let committed_log_size = request.columns[0].column.domain.log_size();
    let committed_domain = CanonicCoset::new(committed_log_size).circle_domain();
    let first_part = part_domain(committed_domain, request.log_parts, 0);
    let stripe_twiddles = SimdBackend::precompute_twiddles(first_part.half_coset);
    let twiddles = SimdBackend::precompute_twiddles(request.domain.half_coset);
    for (column, answer) in request.columns.iter().zip(answers) {
        let Some(answer) = answer else {
            check.declined += 1;
            continue;
        };
        assert_eq!(column.column.domain.log_size(), committed_log_size);
        let poly = Poly::new(None, column.column.clone());
        let cpu = poly.stripe_coefficients(&stripe_twiddles).evaluate_with_twiddles(request.domain, &twiddles);
        // The raw words (BaseField is a u32 newtype; `as_slice` doesn't reduce).
        let cpu_words: Vec<u32> = cpu.values.as_slice().iter().map(|value| value.0).collect();
        let gpu_words: Vec<u32> = answer.as_slice().iter().map(|value| value.0).collect();
        check.columns += 1;
        check.words += cpu_words.len() as u64;
        if column.column.values.as_slice().iter().all(|value| value.0 == 0) {
            check.zero_columns += 1;
            check.zero_words += cpu_words.len() as u64;
        }
        check.cpu_words_ge_p += cpu_words.iter().filter(|&&w| w >= P).count() as u64;
        check.gpu_words_ge_p += gpu_words.iter().filter(|&&w| w >= P).count() as u64;
        check.cpu_words_eq_p += cpu_words.iter().filter(|&&w| w == P).count() as u64;
        check.gpu_words_eq_p += gpu_words.iter().filter(|&&w| w == P).count() as u64;
        let different = if cpu_words.len() != gpu_words.len() {
            cpu_words.len().max(gpu_words.len())
        } else {
            cpu_words.iter().zip(&gpu_words).filter(|(a, b)| a != b).count()
        };
        if different > 0 {
            check.different_columns += 1;
            check.different_words += different as u64;
            tracing::warn!(
                "part check: tree {} column of 2^{} differs in {different} words",
                column.tree,
                cpu_words.len().ilog2()
            );
        }
    }
    check.seconds = started.elapsed().as_secs_f64();
    let mut total = PART_CHECK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    total.columns += check.columns;
    total.words += check.words;
    total.different_columns += check.different_columns;
    total.different_words += check.different_words;
    total.cpu_words_ge_p += check.cpu_words_ge_p;
    total.gpu_words_ge_p += check.gpu_words_ge_p;
    total.cpu_words_eq_p += check.cpu_words_eq_p;
    total.gpu_words_eq_p += check.gpu_words_eq_p;
    total.declined += check.declined;
    total.zero_columns += check.zero_columns;
    total.zero_words += check.zero_words;
    total.seconds += check.seconds;
}

/// constraint-framework's part-extension hook on the committer (see stwo-gpu-spike's `parts`).
fn part_hook(committer: Arc<GpuCommitter>) -> Arc<TracePartHook> {
    let check = part_check_enabled();
    Arc::new(move |request: &TracePartRequest<'_>| {
        let columns = request.columns.iter().map(|column| (column.tree, column.column)).collect::<Vec<_>>();
        let answers = committer.trace_part(request.part, request.log_parts, &columns);
        if check {
            check_parts(request, &answers);
        }
        answers
    })
}
