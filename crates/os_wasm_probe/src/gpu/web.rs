//! The striped commitments on the GPU, in browsers (the threaded build with feature `gpu`).
//!
//! stwo's `CommitmentTreeProver::new_striped` tries a hook first (`set_striped_commit_hook`); the
//! one registered here sends the commitment to the page's GPU worker, a separate module that owns
//! a WebGPU device, through the bridge (`stwo-gpu-bridge`): a job table in this module's shared
//! memory, where the calling thread blocks (`memory.atomic.wait32`) until the GPU worker is done.
//! The worker reads the coefficients from this memory and writes the outputs into it: the Vecs
//! this hook allocated, stwo's own columns and layers, which become the tree. The proof is then
//! byte for byte the CPU's (the worker runs the same commitment as stwo, checked bit for bit).
//!
//! On any failure (no GPU worker attached, a lost device, a shape the worker refuses) the hook
//! gives the polynomials back and stwo commits them on the CPU.
//!
//! The page creates the GPU worker once; `js/worker.mjs` attaches it to this module's memory
//! (`gpu_bridge_region`) before the proof, enables the hook (`gpu_enable`) and detaches it after.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use stwo::core::ColumnVec;
use stwo::core::fields::m31::BaseField;
use stwo::core::poly::circle::CanonicCoset;
use stwo::core::vcs::blake2_hash::Blake2sHash;
use stwo::core::vcs_lifted::blake2_merkle::Blake2sM31MerkleChannel;
use stwo::prover::backend::Column;
use stwo::prover::backend::simd::SimdBackend;
use stwo::prover::backend::simd::column::BaseColumn;
use stwo::prover::poly::BitReversedOrder;
use stwo::prover::poly::circle::{CircleCoefficients, CircleEvaluation, part_domain};
use stwo::prover::poly::twiddles::TwiddleTree;
use stwo::prover::vcs_lifted::prover::MerkleProverLifted;
use stwo::prover::{CommitmentTreeProver, Poly, set_striped_commit_hook};
use stwo_gpu_bridge::{self as bridge, commit as d, op};
use wasm_bindgen::prelude::*;

type Tree = CommitmentTreeProver<SimdBackend, Blake2sM31MerkleChannel>;
type Polys = ColumnVec<CircleCoefficients<SimdBackend>>;

/// `new_striped`'s `MIN_STRIPED_LOG_SIZE`: columns of smaller domains are kept whole.
const MIN_STRIPED_LOG_SIZE: u32 = 14;
/// stwo's `FUSED_LEVELS` and `LOG_N_LANES` (`prover/backend/simd/blake2s_lifted.rs`): the levels
/// `build_layers_sparse` leaves empty when it hashes tile by tile.
const FUSED_LEVELS: u32 = 5;
const LOG_N_LANES: u32 = 4;

/// The commitments of the proof so far, one JSON object each (see `gpu_report`).
static REPORT: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// How long a commitment may take on the GPU worker before it goes to the CPU (ms, as f64
/// bits; see `gpu_enable`).
static DEADLINE_MS: AtomicU64 = AtomicU64::new(0);
/// `gpu_enable`'s default deadline: a commitment takes under a second on a desktop GPU.
const DEFAULT_DEADLINE_MS: f64 = 60_000.0;

/// The bridge's job table in this module's memory, its header written: what the GPU worker
/// attaches to, with the memory.
#[wasm_bindgen]
pub fn gpu_bridge_region() -> u32 {
    bridge::region_address()
}

/// Sends the striped commitments to the GPU worker from now on (each one falls back to the CPU
/// if no worker is attached). A commitment the worker hasn't finished after `deadline_ms`
/// (0: 60 s) goes to the CPU, and so does every later one: the worker is taken for lost (the
/// page may have ended it).
#[wasm_bindgen]
pub fn gpu_enable(deadline_ms: f64) {
    let deadline_ms = if deadline_ms > 0.0 { deadline_ms } else { DEFAULT_DEADLINE_MS };
    DEADLINE_MS.store(deadline_ms.to_bits(), Ordering::Relaxed);
    set_striped_commit_hook::<SimdBackend, Blake2sM31MerkleChannel>(Some(Arc::new(commit)));
}

/// Commits on the CPU again.
#[wasm_bindgen]
pub fn gpu_disable() {
    set_striped_commit_hook::<SimdBackend, Blake2sM31MerkleChannel>(None);
}

/// The commitments the hook saw since the last call, as a JSON array: per commitment its shape,
/// where it ran (`gpu`, or the reason it went to the CPU) and its times in ms.
#[wasm_bindgen]
pub fn gpu_report() -> String {
    let entries = std::mem::take(&mut *REPORT.lock().unwrap_or_else(|e| e.into_inner()));
    format!("[{}]", entries.join(","))
}

/// Absolute ms, comparable across workers (the bridge's slot times use the same clock).
fn now() -> f64 {
    js_sys::Date::now()
}

/// The lowest level stwo's `build_layers_sparse` keeps for a stripe's subtree of height
/// `stripe_height` over columns of these log sizes: a replica of its private `fusable` (every
/// column has at least 16 values, the subtree is at least 9 high, the largest column fills it).
/// The GPU worker checks the layers' lengths against its own layout, and refuses on a difference.
fn min_stored_level(log_sizes: &[u32], stripe_height: u32) -> u32 {
    let (min, max) = (log_sizes.iter().min().unwrap(), log_sizes.iter().max().unwrap());
    let fusable = *min >= LOG_N_LANES
        && stripe_height >= LOG_N_LANES + FUSED_LEVELS
        && *max == stripe_height;
    if fusable { FUSED_LEVELS } else { 0 }
}

/// The hook: `new_striped(polys, log_blowup, twiddles, lifting_log_size)` on the GPU, or the
/// polynomials back.
fn commit(
    polys: Polys,
    log_blowup: u32,
    twiddles: &TwiddleTree<SimdBackend>,
    lifting_log_size: u32,
) -> Result<Tree, Polys> {
    let started = now();
    let log_sizes: Vec<u32> = polys.iter().map(|poly| poly.log_size()).collect();
    let shape = format!(
        "\"columns\":{},\"max_log_size\":{},\"log_blowup\":{log_blowup},\"lifting_log_size\":\
         {lifting_log_size},\"coeffs_mib\":{:.1}",
        log_sizes.len(),
        log_sizes.iter().max().unwrap_or(&0),
        log_sizes.iter().map(|&l| 4u64 << l).sum::<u64>() as f64 / (1 << 20) as f64,
    );
    let declined = |reason: String| {
        let reason = json_escape(&reason);
        record(format!(
            "{{{shape},\"ran\":\"cpu\",\"reason\":\"{reason}\",\"hook_ms\":{:.1}}}",
            now() - started
        ));
    };
    if !bridge::ready() {
        declined("no GPU worker".into());
        return Err(polys);
    }

    // The outputs, as new_striped leaves them: each column's stripe 0 (or its whole evaluation),
    // and the tree's layers, root first (empty below the lowest level stwo keeps).
    let whole = |log_size: u32| log_size + log_blowup < MIN_STRIPED_LOG_SIZE;
    let mut columns: Vec<BaseColumn> = log_sizes
        .iter()
        .map(|&log_size| {
            let len = if whole(log_size) { 1 << (log_size + log_blowup) } else { 1 << log_size };
            // Safety: the GPU worker writes every value before the column is read.
            unsafe { BaseColumn::uninitialized(len) }
        })
        .collect();
    let min_stored = min_stored_level(&log_sizes, lifting_log_size - log_blowup);
    let mut layers: Vec<Vec<Blake2sHash>> = (0..=lifting_log_size)
        .map(|j| {
            let level = lifting_log_size - j;
            if level >= min_stored { Vec::with_capacity(1 << j) } else { Vec::new() }
        })
        .collect();

    // Their pages faulted in now, by all the threads: the GPU worker's copies into them then run
    // at memory speed instead of faulting page by page on its one thread.
    touch_pages(&mut columns, &mut layers);

    let mut desc = vec![0u32; d::descriptor_words(polys.len(), lifting_log_size as usize)];
    desc[d::D_VERSION] = d::DESCRIPTOR_VERSION;
    desc[d::D_N_COLUMNS] = polys.len() as u32;
    desc[d::D_LOG_BLOWUP] = log_blowup;
    desc[d::D_LIFTING_LOG_SIZE] = lifting_log_size;
    desc[d::D_TWIDDLES_PTR] = twiddles.twiddles.as_ptr() as u32;
    desc[d::D_TWIDDLES_LEN] = twiddles.twiddles.len() as u32;
    for (i, (poly, column)) in polys.iter().zip(&mut columns).enumerate() {
        let at = d::HEADER_WORDS + i * d::COLUMN_WORDS;
        desc[at + d::C_COEFFS] = poly.coeffs.as_slice().as_ptr() as u32;
        desc[at + d::C_LOG_SIZE] = poly.log_size();
        desc[at + d::C_OUT] = column.as_mut_slice().as_mut_ptr() as u32;
        desc[at + d::C_OUT_WORDS] = column.len() as u32;
    }
    let layers_at = d::HEADER_WORDS + polys.len() * d::COLUMN_WORDS;
    for (j, layer) in layers.iter_mut().enumerate() {
        let at = layers_at + j * d::LAYER_WORDS;
        if layer.capacity() > 0 {
            desc[at + d::L_PTR] = layer.as_mut_ptr() as u32;
            desc[at + d::L_HASHES] = 1 << j;
        }
    }
    let allocated = now();

    let args = [desc.as_ptr() as u32, (desc.len() * 4) as u32];
    let deadline_ms = f64::from_bits(DEADLINE_MS.load(Ordering::Relaxed));
    let completion = match bridge::call_with_deadline(op::COMMIT_STRIPED, &args, now, deadline_ms) {
        Ok(completion) => completion,
        Err(error) => {
            if error.job_may_be_running() {
                // The worker may still write the outputs and the stats: never free them. (It
                // only reads the coefficients and twiddles, which go back to stwo.)
                std::mem::forget(columns);
                std::mem::forget(layers);
                std::mem::forget(desc);
            }
            declined(error.to_string());
            return Err(polys);
        }
    };
    for (j, layer) in layers.iter_mut().enumerate() {
        if layer.capacity() > 0 {
            // Safety: the GPU worker wrote the layer's hashes (the job is done, and `call`'s
            // SeqCst load of the slot's state orders its writes before these reads).
            unsafe { layer.set_len(1 << j) };
        }
    }
    drop(polys);
    let polynomials = columns
        .into_iter()
        .zip(&log_sizes)
        .map(|(values, &log_size)| {
            let domain = CanonicCoset::new(log_size + log_blowup).circle_domain();
            let mut evals = CircleEvaluation::<SimdBackend, BaseField, BitReversedOrder>::new(
                part_domain(domain, domain.log_size() - values.len().ilog2(), 0),
                values,
            );
            evals.domain = domain;
            Poly::new(None, evals)
        })
        .collect();
    let tree = CommitmentTreeProver { polynomials, commitment: MerkleProverLifted { layers } };

    // Written by the GPU worker, through a pointer the compiler can't follow.
    let word = |at: usize| unsafe { std::ptr::read_volatile(desc.as_ptr().add(at)) } as u64;
    let stat = |k: usize| f64::from_bits(word(d::D_STATS + 2 * k) | word(d::D_STATS + 2 * k + 1) << 32);
    // The GPU worker's times (see the bridge's `commit::ST_*`): the upload, the compute and the
    // readback overlap (commit_ms is the whole of them), so each is that side's own work; in the
    // worker's serial mode (serial: 1) each phase is waited for, and is its time alone. The GPU's
    // own time (gpu_*) when the worker times it (its timestamps option), 0 otherwise.
    let mib = |k: usize| stat(k) / (1 << 20) as f64;
    record(format!(
        "{{{shape},\"ran\":\"gpu\",\"hook_ms\":{:.1},\"alloc_ms\":{:.1},\"wait_ms\":{:.1},\
         \"wake_ms\":{:.2},\"serve_ms\":{:.1},\"twiddles_ms\":{:.1},\"twiddles_computed\":{},\
         \"twiddles_from_prover\":{},\"setup_ms\":{:.1},\"plans_kept\":{},\"commit_ms\":{:.1},\
         \"upload_ms\":{:.1},\"upload_wait_ms\":{:.1},\"compute_ms\":{:.1},\"readback_ms\":{:.1},\
         \"readback_wait_ms\":{:.1},\"tail_ms\":{:.1},\"destroy_ms\":{:.1},\"streamed\":{},\
         \"groups\":{},\"serial\":{},\"upload_mib\":{:.1},\"readback_mib\":{:.1},\"gpu_mib\":{:.1},\
         \"held_mib\":{:.1},\"worker_ms\":{:.1},\"gpu_upload_ms\":{:.1},\"gpu_compute_ms\":{:.1},\
         \"gpu_readback_ms\":{:.1},\"gpu_busy_ms\":{:.1},\"gpu_span_ms\":{:.1}}}",
        now() - started,
        allocated - started,
        completion.returned - allocated,
        completion.wake_ms(),
        completion.serve_ms(),
        stat(d::ST_TWIDDLES),
        stat(d::ST_TWIDDLES_COMPUTED) as u32,
        stat(d::ST_TWIDDLES_FROM_PROVER) as u32,
        stat(d::ST_SETUP),
        stat(d::ST_PLANS_KEPT) as u32,
        stat(d::ST_COMMIT),
        stat(d::ST_UPLOAD),
        stat(d::ST_UPLOAD_WAIT),
        stat(d::ST_COMPUTE),
        stat(d::ST_READBACK),
        stat(d::ST_READBACK_WAIT),
        stat(d::ST_TAIL),
        stat(d::ST_DESTROY),
        stat(d::ST_STREAMED) as u32,
        stat(d::ST_GROUPS) as u32,
        stat(d::ST_SERIAL) as u32,
        mib(d::ST_UPLOAD_BYTES),
        mib(d::ST_READBACK_BYTES),
        mib(d::ST_GPU_BYTES),
        mib(d::ST_HELD_BYTES),
        stat(d::ST_TOTAL),
        stat(d::ST_GPU_UPLOAD),
        stat(d::ST_GPU_COMPUTE),
        stat(d::ST_GPU_READBACK),
        stat(d::ST_GPU_BUSY),
        stat(d::ST_GPU_SPAN),
    ));
    Ok(tree)
}

/// Writes a word in every 4 KiB page of the outputs, in parallel.
fn touch_pages(columns: &mut [BaseColumn], layers: &mut [Vec<Blake2sHash>]) {
    use rayon::prelude::*;
    const PAGE: usize = 4096;
    columns.par_iter_mut().for_each(|column| {
        let words = column.as_mut_slice();
        for i in (0..words.len()).step_by(PAGE / 4) {
            words[i] = BaseField::from_u32_unchecked(0);
        }
    });
    for layer in layers {
        let (base, bytes) = (layer.as_mut_ptr() as usize, layer.capacity() * 32);
        (0..bytes.div_ceil(PAGE)).into_par_iter().for_each(|page| {
            // Safety: the byte is inside the layer's allocation; nothing reads it before the GPU
            // worker writes the layer.
            unsafe { ((base + page * PAGE) as *mut u8).write_volatile(0) };
        });
    }
}

/// `text` as the inside of a JSON string (error messages carry newlines and quotes).
fn json_escape(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '"' => escaped.push_str("\\\""),
            '\\' => escaped.push_str("\\\\"),
            '\n' => escaped.push_str("\\n"),
            c if (c as u32) < 0x20 => escaped.push_str(&format!("\\u{:04x}", c as u32)),
            c => escaped.push(c),
        }
    }
    escaped
}

fn record(entry: String) {
    REPORT.lock().unwrap_or_else(|e| e.into_inner()).push(entry);
}

/// Times the striped commitments stwo makes on the CPU (its "Striped commitment" span) into the
/// report, `{"ran":"cpu","cpu_ms":...}` each: the reference for the GPU's times, in a run without
/// the GPU. A tracing subscriber that enables that span only, so nothing else pays for it.
/// Returns false if a subscriber was already set.
#[wasm_bindgen]
pub fn gpu_time_cpu_commitments() -> bool {
    use tracing_subscriber::filter::filter_fn;
    use tracing_subscriber::prelude::*;
    let only = filter_fn(|meta| meta.is_span() && meta.name() == CPU_SPAN);
    tracing_subscriber::registry().with(CpuTimes.with_filter(only)).try_init().is_ok()
}

/// `new_striped`'s span around its own (CPU) commitment.
const CPU_SPAN: &str = "Striped commitment";

struct CpuTimes;

/// When the span was entered.
struct Entered(f64);

impl<S> tracing_subscriber::Layer<S> for CpuTimes
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    fn on_enter(&self, id: &tracing::span::Id, ctx: tracing_subscriber::layer::Context<'_, S>) {
        if let Some(span) = ctx.span(id) {
            span.extensions_mut().replace(Entered(now()));
        }
    }

    fn on_exit(&self, id: &tracing::span::Id, ctx: tracing_subscriber::layer::Context<'_, S>) {
        if let Some(span) = ctx.span(id)
            && let Some(Entered(at)) = span.extensions_mut().remove::<Entered>()
        {
            record(format!("{{\"ran\":\"cpu\",\"cpu_ms\":{:.1}}}", now() - at));
        }
    }
}
