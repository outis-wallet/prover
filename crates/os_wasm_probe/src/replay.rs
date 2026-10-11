//! The whole TxSession pipeline on a recording made by rpc-record-proxy.py (as replay.mjs does in
//! Node), natively: `os_native_probe replay` and the C entry point `tx_prove_replay` (iOS app).
use std::ffi::{c_char, c_void, CStr, CString};
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::{json, Value};
use starknet_types_core::felt::Felt;

use crate::TxSession;

/// Timings, verdicts and peak memory of one replay.
#[derive(Serialize)]
pub struct Report {
    pub block: u64,
    pub tx_hash: String,
    pub threads: usize,
    pub rounds: usize,
    pub input_s: f64,
    pub os_s: f64,
    pub prove_s: f64,
    pub verify_s: f64,
    pub total_s: f64,
    /// Cairo steps of the virtual OS run.
    pub os_steps: usize,
    pub pie: String,
    pub verified: bool,
    pub verify_error: Option<String>,
    /// Proof and proof_facts equal to the native prover's response in the capture.
    pub identical: bool,
    /// Decoded size of the proof.
    pub proof_bytes: usize,
    pub peak: PeakMemory,
    #[serde(skip)]
    pub result_json: String,
}

/// Peak memory in MiB. `run_mib` is sampled during this replay (the physical footprint on Apple,
/// which is what jetsam counts on iOS; RSS elsewhere). `rss_mib` and `footprint_mib` (Apple only)
/// are the peaks of the whole process so far, so they include earlier replays.
#[derive(Serialize)]
pub struct PeakMemory {
    pub run_mib: u64,
    pub rss_mib: u64,
    pub footprint_mib: Option<u64>,
}

/// What a replay reports while it runs.
#[derive(Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Progress<'a> {
    /// A phase started.
    Phase { phase: Phase },
    /// The transaction and the block it runs on.
    Transaction { block: u64, tx_hash: &'a str },
    /// The virtual OS finished after this many Cairo steps.
    OsSteps { steps: usize },
    /// Memory in use now, in MiB (as `PeakMemory::run_mib`), about every 250 ms.
    Memory { mib: u64 },
    /// A line for logs.
    Note { message: &'a str },
}

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// Reading the block, the transaction's state and its contract classes.
    State,
    /// Executing the transaction, then building the virtual OS input from the storage proofs.
    Execute,
    /// Running the virtual OS (the Starknet OS on a one-transaction block).
    Os,
    /// Proving the virtual OS run.
    Prove,
    /// Verifying the proof with the sequencer's verifier.
    Verify,
}

const MEMORY_SAMPLE_INTERVAL: Duration = Duration::from_millis(50);
const MEMORY_SAMPLES_PER_EVENT: u32 = 5;

/// Replays the capture in `capture` on the current rayon pool; panics on any failure. `progress`
/// gets the events, from this thread and from a memory sampler thread.
pub fn replay(capture: &Path, progress: &(dyn Fn(Progress<'_>) + Sync)) -> Report {
    let sampling = AtomicBool::new(true);
    let run_peak_mib = AtomicU64::new(0);
    let mut report = std::thread::scope(|scope| {
        scope.spawn(|| {
            let mut samples = 0;
            while sampling.load(Ordering::Relaxed) {
                let mib = current_memory_mib();
                run_peak_mib.fetch_max(mib, Ordering::Relaxed);
                if samples % MEMORY_SAMPLES_PER_EVENT == 0 {
                    progress(Progress::Memory { mib });
                }
                samples += 1;
                std::thread::sleep(MEMORY_SAMPLE_INTERVAL);
            }
        });
        // Stops the sampler also when the pipeline panics; the scope would wait for it forever.
        let _stop_sampling = StopOnDrop(&sampling);
        run_pipeline(capture, progress)
    });
    report.peak.run_mib = run_peak_mib.load(Ordering::Relaxed);
    report
}

struct StopOnDrop<'a>(&'a AtomicBool);

impl Drop for StopOnDrop<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Relaxed);
    }
}

/// A capture made by rpc-record-proxy.py: its recorded RPC answers, the prove request and the
/// native prover's response. Each method panics on what the recording doesn't hold.
pub struct Recording {
    dir: PathBuf,
    recorded: Vec<Value>,
}

impl Recording {
    pub fn open(capture: &Path) -> Recording {
        let recorded = std::fs::read_to_string(capture.join("rpc-log.jsonl"))
            .expect("rpc-log.jsonl")
            .lines()
            .map(|line| serde_json::from_str(line).expect("log line"))
            .collect();
        Recording { dir: capture.to_path_buf(), recorded }
    }

    /// The result of the first recorded `method` call whose params match.
    pub fn lookup(&self, method: &str, matches: &dyn Fn(&Value) -> bool) -> Value {
        self.recorded
            .iter()
            .find(|entry| entry["request"]["method"] == method && matches(&entry["request"]["params"]))
            .map(|entry| entry["response"]["result"].clone())
            .unwrap_or_else(|| panic!("not in the recording: {method}"))
    }

    /// The transaction of the prove request.
    pub fn transaction(&self) -> Value {
        read_json(&first_file(&self.dir, "request-"))["params"]["transaction"].clone()
    }

    /// The block header (`starknet_getBlockWithTxHashes`).
    pub fn header(&self) -> Value {
        self.lookup("starknet_getBlockWithTxHashes", &|_| true)
    }

    /// The simulation's `initial_reads`.
    pub fn initial_reads(&self) -> Value {
        self.lookup("starknet_simulateTransactions", &|_| true)["initial_reads"].clone()
    }

    /// `add_class`'s CASM JSON and Sierra program head JSON for `class_hash`.
    pub fn class(&self, class_hash: &Value) -> (String, String) {
        let casm = read_json(&self.dir.join(format!("casm-{}.json", class_hash.as_str().unwrap())));
        let sierra =
            self.lookup("starknet_getClass", &|params| felt(&params["class_hash"]) == felt(class_hash));
        let head = Value::Array(sierra["sierra_program"].as_array().expect("sierra")[..3].to_vec());
        (casm["result"].to_string(), head.to_string())
    }

    /// The reads that `execute` reported missing, in `add_reads`' format, and how many there are.
    /// Only storage misses are recorded (the simulation prefetched the rest).
    pub fn missing_reads(&self, missing: &Value) -> (String, usize) {
        let storage: Vec<Value> = missing["storage"]
            .as_array()
            .unwrap()
            .iter()
            .map(|pair| {
                let value = self.lookup("starknet_getStorageAt", &|params| {
                    felt(&params["contract_address"]) == felt(&pair[0]) && felt(&params["key"]) == felt(&pair[1])
                });
                json!({ "contract_address": pair[0], "key": pair[1], "value": value })
            })
            .collect();
        assert!(
            missing["nonces"].as_array().unwrap().is_empty()
                && missing["class_hashes"].as_array().unwrap().is_empty()
                && missing["classes"].as_array().unwrap().is_empty(),
            "only storage misses are replayed: {missing}"
        );
        (json!({ "storage": storage }).to_string(), storage.len())
    }

    /// The `starknet_getStorageProof` results for `execute`'s `storage_proof_params`, as JSON.
    pub fn storage_proofs(&self, proof_params: &Value) -> String {
        let proofs: Vec<Value> = proof_params
            .as_array()
            .unwrap()
            .iter()
            .map(|_| self.lookup("starknet_getStorageProof", &|_| true))
            .collect();
        Value::Array(proofs).to_string()
    }

    /// The native prover's `starknet_proveTransaction` result.
    pub fn native_result(&self) -> Value {
        read_json(&first_file(&self.dir, "response-"))["result"].clone()
    }
}

/// Proof and proof_facts of a prove result equal to the native prover's.
pub fn identical(result: &Value, native: &Value) -> bool {
    result["proof"] == native["proof"] && result["proof_facts"] == native["proof_facts"]
}

fn run_pipeline(capture: &Path, progress: &(dyn Fn(Progress<'_>) + Sync)) -> Report {
    let note = |message: &str| progress(Progress::Note { message });
    progress(Progress::Phase { phase: Phase::State });
    let recording = Recording::open(capture);
    let transaction = recording.transaction();

    let started = Instant::now();
    let header = recording.header();
    let mut session =
        TxSession::new("SN_SEPOLIA", &header.to_string(), &transaction.to_string()).expect("session");
    progress(Progress::Transaction { block: session.block_number(), tx_hash: &session.tx_hash() });
    let initial_reads = recording.initial_reads();
    session.add_reads(&initial_reads.to_string()).expect("reads");
    for entry in initial_reads["class_hashes"].as_array().expect("class hashes") {
        let (casm, head) = recording.class(&entry["class_hash"]);
        session.add_class(entry["class_hash"].as_str().unwrap(), &casm, &head).expect("class");
    }

    progress(Progress::Phase { phase: Phase::Execute });
    let mut rounds = 0;
    let proof_params = loop {
        rounds += 1;
        let result: Value = serde_json::from_str(&session.execute().expect("execute")).unwrap();
        let Some(missing) = result.get("missing") else {
            break result["storage_proof_params"].clone();
        };
        let (reads, count) = recording.missing_reads(missing);
        note(&format!("round {rounds} missed {count} storage"));
        session.add_reads(&reads).expect("reads");
    };
    session.set_storage_proofs(&recording.storage_proofs(&proof_params)).expect("storage proofs");
    let input_s = started.elapsed().as_secs_f64();

    progress(Progress::Phase { phase: Phase::Os });
    let os_started = Instant::now();
    let pie = session.run_os().expect("virtual OS");
    let os_s = os_started.elapsed().as_secs_f64();
    let os_steps = session.os_steps().expect("the virtual OS ran");
    note(&pie);
    progress(Progress::OsSteps { steps: os_steps });

    progress(Progress::Phase { phase: Phase::Prove });
    let prove_started = Instant::now();
    let result_json = session.prove().expect("prove");
    let prove_s = prove_started.elapsed().as_secs_f64();

    progress(Progress::Phase { phase: Phase::Verify });
    let verify_started = Instant::now();
    let verdict = crate::verify_result(&result_json);
    let verify_s = verify_started.elapsed().as_secs_f64();

    let result: Value = serde_json::from_str(&result_json).unwrap();
    let native = recording.native_result();
    let proof_base64 = result["proof"].as_str().expect("proof");
    let padding = proof_base64.bytes().rev().take_while(|&byte| byte == b'=').count();
    Report {
        block: session.block_number(),
        tx_hash: session.tx_hash(),
        threads: rayon::current_num_threads(),
        rounds,
        input_s: milliseconds(input_s),
        os_s: milliseconds(os_s),
        prove_s: milliseconds(prove_s),
        verify_s: milliseconds(verify_s),
        total_s: milliseconds(started.elapsed().as_secs_f64()),
        os_steps,
        pie,
        verified: verdict.is_ok(),
        verify_error: verdict.err(),
        identical: identical(&result, &native),
        proof_bytes: proof_base64.len() / 4 * 3 - padding,
        peak: peak_memory(),
        result_json,
    }
}

/// Receives each [`Progress`] event as JSON, with the `context` given to [`tx_prove_replay`]. It
/// is called from the proving thread and from the memory sampler thread, and the JSON is only
/// valid during the call.
pub type ProgressCallback = unsafe extern "C" fn(context: *mut c_void, event_json: *const c_char);

struct ProgressTarget {
    callback: Option<ProgressCallback>,
    context: *mut c_void,
}

// The caller of tx_prove_replay promises that the callback may run on any thread with `context`.
unsafe impl Sync for ProgressTarget {}

/// C entry point (the iOS app): replays the capture in the directory `capture_dir` on a pool of
/// `threads` threads (0: one per core) and returns the [`Report`] as pretty JSON, or
/// `{"error": ...}`. `progress` (may be null) gets the [`Progress`] events. Free the result with
/// [`tx_prove_free`].
///
/// # Safety
/// `capture_dir` must be a valid NUL-terminated string. `progress`, if not null, must be safe to
/// call with `context` from any thread until this function returns.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tx_prove_replay(
    capture_dir: *const c_char,
    threads: u32,
    progress: Option<ProgressCallback>,
    context: *mut c_void,
) -> *mut c_char {
    let capture = PathBuf::from(unsafe { CStr::from_ptr(capture_dir) }.to_string_lossy().into_owned());
    // A reference to the whole target, which is Sync, not to its fields (closures capture those).
    let target = &ProgressTarget { callback: progress, context };
    let send = |event: Progress<'_>| {
        if let Some(callback) = target.callback {
            let event_json = CString::new(serde_json::to_string(&event).expect("event")).expect("no NUL");
            unsafe { callback(target.context, event_json.as_ptr()) };
        }
    };
    let run = AssertUnwindSafe(|| {
        // install() runs the whole pipeline on a pool thread, not just the parallel parts: give the
        // threads a main thread's stack (as the wasm build's -zstack-size) instead of 2 MiB.
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(threads as usize)
            .stack_size(16 << 20)
            .build()
            .expect("thread pool");
        let report = pool.install(|| replay(&capture, &send));
        serde_json::to_string_pretty(&report).expect("report")
    });
    let answer = std::panic::catch_unwind(run).unwrap_or_else(|panic| {
        let message = panic
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| panic.downcast_ref::<&str>().copied())
            .unwrap_or("panic");
        json!({ "error": message }).to_string()
    });
    CString::new(answer).expect("no NUL in JSON").into_raw()
}

/// Frees a string returned by [`tx_prove_replay`] or by the session functions (`native`).
///
/// # Safety
/// `answer` must come from this library and not be freed before.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tx_prove_free(answer: *mut c_char) {
    if !answer.is_null() {
        drop(unsafe { CString::from_raw(answer) });
    }
}

#[cfg(target_vendor = "apple")]
pub fn peak_memory() -> PeakMemory {
    let Some(info) = task_vm_info() else {
        return PeakMemory { run_mib: 0, rss_mib: 0, footprint_mib: None };
    };
    PeakMemory {
        run_mib: 0,
        rss_mib: info.resident_size_peak >> 20,
        footprint_mib: Some((info.ledger_phys_footprint_peak.max(0) as u64) >> 20),
    }
}

/// The physical footprint now (Apple).
#[cfg(target_vendor = "apple")]
fn current_memory_mib() -> u64 {
    task_vm_info().map_or(0, |info| info.phys_footprint >> 20)
}

/// <mach/task_info.h> `struct task_vm_info` through rev 3 (TASK_VM_INFO_REV3_COUNT = 84 ints).
/// The kernel fills whole revisions only, so the struct must cover all of rev 3. Every field
/// after page_size is 8-byte aligned, so the header's pack(4) does not change this layout.
#[cfg(target_vendor = "apple")]
#[repr(C)]
#[derive(Default)]
struct TaskVmInfo {
    virtual_size: u64,
    region_count: i32,
    page_size: i32,
    resident_size: u64,
    resident_size_peak: u64,
    _device_to_compressed_lifetime: [u64; 14],
    phys_footprint: u64,
    min_address: u64,
    max_address: u64,
    ledger_phys_footprint_peak: i64,
    _rest_of_rev3: [i64; 20],
}

#[cfg(target_vendor = "apple")]
fn task_vm_info() -> Option<TaskVmInfo> {
    const TASK_VM_INFO: u32 = 22;
    unsafe extern "C" {
        static mach_task_self_: u32;
        fn task_info(task: u32, flavor: u32, info: *mut i32, count: *mut u32) -> i32;
    }
    let mut info = TaskVmInfo::default();
    let mut count = (size_of::<TaskVmInfo>() / size_of::<i32>()) as u32;
    let status = unsafe {
        task_info(mach_task_self_, TASK_VM_INFO, (&raw mut info).cast(), &mut count)
    };
    (status == 0).then_some(info)
}

/// VmHWM (Linux / Android).
#[cfg(not(target_vendor = "apple"))]
pub fn peak_memory() -> PeakMemory {
    PeakMemory { run_mib: 0, rss_mib: proc_status_mib("VmHWM:"), footprint_mib: None }
}

/// VmRSS (Linux / Android).
#[cfg(not(target_vendor = "apple"))]
fn current_memory_mib() -> u64 {
    proc_status_mib("VmRSS:")
}

/// A `/proc/self/status` field given in kB, in MiB (0 if missing).
#[cfg(not(target_vendor = "apple"))]
fn proc_status_mib(field: &str) -> u64 {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    status
        .lines()
        .find(|line| line.starts_with(field))
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|kib| kib.parse::<u64>().ok())
        .map_or(0, |kib| kib / 1024)
}

/// Seconds rounded to the millisecond, which JSON then prints in full.
fn milliseconds(seconds: f64) -> f64 {
    (seconds * 1000.0).round() / 1000.0
}

fn read_json(path: &Path) -> Value {
    serde_json::from_slice(&std::fs::read(path).unwrap_or_else(|e| panic!("{path:?}: {e}")))
        .unwrap_or_else(|e| panic!("{path:?}: {e}"))
}

fn first_file(dir: &Path, prefix: &str) -> PathBuf {
    std::fs::read_dir(dir)
        .expect("read capture dir")
        .map(|entry| entry.expect("dir entry").path())
        .find(|path| path.file_name().unwrap().to_string_lossy().starts_with(prefix))
        .unwrap_or_else(|| panic!("no {prefix}* in {dir:?}"))
}

fn felt(value: &Value) -> Felt {
    Felt::from_hex(value.as_str().expect("hex string")).expect("felt")
}
