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
use stwo::core::vcs_lifted::blake2_merkle::Blake2sM31MerkleChannel;
use stwo::prover::backend::simd::SimdBackend;
use stwo::prover::poly::circle::{part_domain, PolyOps};
use stwo::prover::{set_striped_commit_hook, Poly, StripedCommitHook};
use stwo_constraint_framework::{set_trace_part_hook, TracePartHook, TracePartRequest};
use stwo_gpu_spike::committer::{Declined, GpuCommitter};
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
}

fn apply(enabled: bool) -> Result<Option<String>, String> {
    if !enabled {
        set_striped_commit_hook::<SimdBackend, Blake2sM31MerkleChannel>(None);
        set_trace_part_hook(None);
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
