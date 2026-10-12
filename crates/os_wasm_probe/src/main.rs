//! Native checks.
//! `os <virtual-os-input.json>`: runs the virtual OS and prints the PIE summary.
//! `verify <response.json>`: verifies a starknet_proveTransaction result.
//! `replay <capture dir> [out.json]`: the whole TxSession pipeline on a recording made by
//! rpc-record-proxy.py (as replay.mjs does in Node), with timings, peak memory and a comparison
//! with the native prover's response. One thread per physical core, unless RAYON_NUM_THREADS.
//! `ffi <capture dir> [--threads N] [--cancel-after S]`: the same through the C interface the apps
//! link (include/outis_prover.h), proved and verified; with --cancel-after, the prove is cancelled
//! S seconds in and the time it takes to return is measured.
//! TRACE=info (or debug, ...) prints the prover's tracing events with their times, and each span's
//! opening and closing (with its busy and idle time).
//! With the `gpu` feature, OUTIS_GPU=1 commits on the GPU (see the library's `gpu`), and replay
//! lists the commitments it was asked for; `ffi --gpu` turns it on through `tx_prove_set_gpu`.
use std::ffi::{c_char, CStr, CString};
use std::path::Path;
use std::time::{Duration, Instant};

use os_wasm_probe::native::{
    tx_session_call, tx_session_cancel, tx_session_close, tx_session_open, TxNativeSession,
};
use os_wasm_probe::replay::{identical, tx_prove_free, Phase, Progress, Recording};
use serde_json::{json, Value};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if let Ok(level) = std::env::var("TRACE") {
        tracing_subscriber::fmt()
            .with_max_level(level.parse::<tracing::Level>().expect("TRACE: a tracing level"))
            .with_timer(tracing_subscriber::fmt::time::uptime())
            .with_span_events(
                tracing_subscriber::fmt::format::FmtSpan::ENTER | tracing_subscriber::fmt::format::FmtSpan::CLOSE,
            )
            .init();
    }
    let usage = "usage: os_native_probe os <input.json> | verify <response.json> | replay <capture dir> [out.json] \
                 | ffi <capture dir> [--threads N] [--cancel-after S] [--gpu] [--keep-warm MS]";
    let option = |name: &str| {
        args.iter().position(|arg| arg == name).map(|index| args[index + 1].parse::<f64>().expect(name))
    };
    match args.get(1).map(String::as_str) {
        Some("os") => {
            let input = std::fs::read(&args[2]).expect("read input");
            let started = Instant::now();
            let pie = os_wasm_probe::run(&input).expect("virtual OS run");
            println!("{} in {:.2} s", os_wasm_probe::summary(&pie), started.elapsed().as_secs_f64());
        }
        Some("verify") => {
            let input = std::fs::read_to_string(&args[2]).expect("read response");
            let started = Instant::now();
            let verdict = os_wasm_probe::verify_result(&input);
            println!("{verdict:?} in {:.2} s", started.elapsed().as_secs_f64());
        }
        Some("replay") => {
            gpu_from_env();
            replay(Path::new(&args[2]), args.get(3).map(Path::new))
        }
        Some("ffi") => ffi(
            Path::new(&args[2]),
            option("--threads").unwrap_or(0.0) as u32,
            option("--cancel-after").map(Duration::from_secs_f64),
            args.iter().any(|arg| arg == "--gpu"),
            option("--keep-warm").map(|ms| ms as u32),
        ),
        _ => panic!("{usage}"),
    }
    // No GPU work in the background while the process exits.
    #[cfg(feature = "gpu")]
    os_wasm_probe::gpu::shutdown();
}

fn replay(capture: &Path, out: Option<&Path>) {
    // One thread per physical core, as the C entry points do, unless RAYON_NUM_THREADS says.
    if std::env::var_os("RAYON_NUM_THREADS").is_none() {
        rayon::ThreadPoolBuilder::new()
            .num_threads(os_wasm_probe::native::default_threads())
            .build_global()
            .expect("thread pool");
    }
    let report = os_wasm_probe::replay::replay(capture, &|event| match event {
        Progress::Transaction { block, tx_hash } => println!("block {block}, tx {tx_hash}"),
        Progress::Note { message } => println!("{message}"),
        Progress::Phase { phase: Phase::Prove } => println!("proving"),
        _ => {}
    });
    if let Some(out) = out {
        std::fs::write(out, &report.result_json).expect("write result");
    }
    #[cfg(feature = "gpu")]
    for commitment in os_wasm_probe::gpu::take_commitments() {
        println!(
            "commitment #{}: {} columns up to 2^{}, blowup 2^{}, lifted to 2^{}: {} {:.3} s",
            commitment.index,
            commitment.columns,
            commitment.max_log_size,
            commitment.log_blowup,
            commitment.lifting_log_size,
            commitment.declined.map_or("GPU".to_string(), |reason| format!("CPU ({reason}), hook")),
            commitment.seconds,
        );
    }
    let footprint = report.peak.footprint_mib.map_or(String::new(), |mib| format!(", peak footprint {mib} MiB"));
    let proof_kib = report.proof_bytes / 1024;
    println!(
        "input {:.2} s ({} rounds), OS {:.2} s, prove {:.1} s ({} threads), verify {:.2} s {}; proof {} to the \
         native prover's; {} steps, proof {proof_kib} KiB; peak RSS {} MiB{footprint}",
        report.input_s,
        report.rounds,
        report.os_s,
        report.prove_s,
        report.threads,
        report.verify_s,
        report.verify_error.as_deref().map_or("Ok".to_string(), |error| format!("FAILED: {error}")),
        if report.identical { "IDENTICAL" } else { "DIFFERENT" },
        report.os_steps,
        report.peak.rss_mib,
    );
}

/// With the `gpu` feature and OUTIS_GPU=1, the GPU committer, made before the proof (so its setup
/// isn't counted in the prove time) and the adapter printed.
fn gpu_from_env() {
    #[cfg(feature = "gpu")]
    if os_wasm_probe::gpu::env_enabled() {
        let started = Instant::now();
        match os_wasm_probe::gpu::set_gpu(true) {
            Ok(adapter) => println!(
                "GPU commitment on {} (ready in {:.2} s)",
                adapter.unwrap_or_default(),
                started.elapsed().as_secs_f64()
            ),
            Err(error) => println!("GPU commitment unavailable, on the CPU: {error}"),
        }
    }
}

/// A session driven only through the C interface, as the apps drive it.
struct Session(*mut TxNativeSession);

/// The session pointer, for the thread that cancels.
struct SessionPointer(*mut TxNativeSession);
unsafe impl Send for SessionPointer {}

impl Session {
    fn open(header: &Value, transaction: &Value, threads: u32) -> Result<Session, String> {
        let chain_id = c_string("SN_SEPOLIA");
        let (header, transaction) = (c_string(&header.to_string()), c_string(&transaction.to_string()));
        let mut error = std::ptr::null_mut();
        let session = unsafe {
            tx_session_open(chain_id.as_ptr(), header.as_ptr(), transaction.as_ptr(), threads, &mut error)
        };
        if session.is_null() {
            return Err(take(error));
        }
        assert!(error.is_null());
        Ok(Session(session))
    }

    /// The whole answer of a call.
    fn answer(&self, method: &str, args: Value) -> Value {
        let (method, args) = (c_string(method), c_string(&args.to_string()));
        let answer = unsafe { tx_session_call(self.0, method.as_ptr(), args.as_ptr()) };
        serde_json::from_str(&take(answer)).expect("an answer is JSON")
    }

    /// The value of a call that must succeed.
    fn call(&self, method: &str, args: Value) -> Value {
        let answer = self.answer(method, args);
        answer.get("ok").cloned().unwrap_or_else(|| panic!("{method}: {answer}"))
    }

    fn call_string(&self, method: &str, args: Value) -> String {
        let value = self.call(method, args);
        value.as_str().unwrap_or_else(|| panic!("{method}: not a string")).to_string()
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        unsafe { tx_session_close(self.0) };
    }
}

fn c_string(text: &str) -> CString {
    CString::new(text).expect("no NUL")
}

/// A string from the library, freed.
fn take(answer: *mut c_char) -> String {
    let text = unsafe { CStr::from_ptr(answer) }.to_string_lossy().into_owned();
    unsafe { tx_prove_free(answer) };
    text
}

fn ffi(capture: &Path, threads: u32, cancel_after: Option<Duration>, gpu: bool, keep_warm: Option<u32>) {
    // --gpu: the GPU through the C entry point, as an app turns it on. Without it, OUTIS_GPU=1
    // turns it on at the proof, as for an app that sets the environment.
    // --keep-warm MS: keep-warm through its C entry point, before the GPU is on.
    if let Some(milliseconds) = keep_warm {
        #[cfg(feature = "gpu")]
        println!(
            "tx_prove_set_gpu_keep_warm({milliseconds}): {}",
            take(os_wasm_probe::native::tx_prove_set_gpu_keep_warm(milliseconds))
        );
        #[cfg(not(feature = "gpu"))]
        panic!("--keep-warm {milliseconds}: built without the gpu feature");
    }
    if gpu {
        #[cfg(feature = "gpu")]
        println!("tx_prove_set_gpu(1): {}", take(os_wasm_probe::native::tx_prove_set_gpu(1)));
        #[cfg(not(feature = "gpu"))]
        panic!("--gpu: built without the gpu feature");
    }
    let recording = Recording::open(capture);
    let (header, transaction) = (recording.header(), recording.transaction());

    // What the apps must be able to rely on besides the happy path.
    let error = Session::open(&json!({}), &transaction, threads).err().expect("a bad header fails");
    assert!(!error.is_empty());
    let no_session =
        take(unsafe { tx_session_call(std::ptr::null_mut(), c"tx_hash".as_ptr(), std::ptr::null()) });
    assert!(no_session.contains("error"), "{no_session}");
    unsafe { tx_session_cancel(std::ptr::null_mut()) };
    unsafe { tx_session_close(std::ptr::null_mut()) };

    let started = Instant::now();
    let session = Session::open(&header, &transaction, threads).expect("open");
    let bad_calls = [("no_such_method", json!([])), ("add_reads", json!([1])), ("set_placeholders", json!(["yes"]))];
    for (method, args) in bad_calls {
        let answer = session.answer(method, args);
        assert!(answer["error"].is_string() && answer.get("cancelled").is_none(), "{method}: {answer}");
    }
    let nothing = take(unsafe { tx_session_call(session.0, c"tx_hash".as_ptr(), c"{}".as_ptr()) });
    assert!(nothing.contains("error"), "{nothing}");
    let no_args = take(unsafe { tx_session_call(session.0, c"tx_hash".as_ptr(), std::ptr::null()) });
    let block = session.call("block_number", json!([]));
    let tx_hash = serde_json::from_str::<Value>(&no_args).unwrap()["ok"].clone();
    println!("block {block}, tx {tx_hash}; error paths answered");

    assert!(session.call("set_placeholders", json!([false])).is_null());
    let initial_reads = recording.initial_reads();
    session.call("add_reads", json!([initial_reads.to_string()]));
    for entry in initial_reads["class_hashes"].as_array().expect("class hashes") {
        let (casm, head) = recording.class(&entry["class_hash"]);
        session.call("add_class", json!([entry["class_hash"], casm, head]));
    }
    let mut rounds = 0;
    let proof_params = loop {
        rounds += 1;
        let result: Value = serde_json::from_str(&session.call_string("execute", json!([]))).unwrap();
        let Some(missing) = result.get("missing") else {
            break result["storage_proof_params"].clone();
        };
        session.call("add_reads", json!([recording.missing_reads(missing).0]));
    };
    let built = session.call("set_storage_proofs", json!([recording.storage_proofs(&proof_params)]));
    assert!(built.is_null(), "set_storage_proofs answers null natively");
    let input_s = started.elapsed().as_secs_f64();
    let os_started = Instant::now();
    println!("{}", session.call_string("run_os", json!([])));
    let os_s = os_started.elapsed().as_secs_f64();

    let Some(cancel_after) = cancel_after else {
        let prove_started = Instant::now();
        let result_json = session.call_string("prove", json!([]));
        let prove_s = prove_started.elapsed().as_secs_f64();
        let result: Value = serde_json::from_str(&result_json).unwrap();
        let same = identical(&result, &recording.native_result());
        let verify_started = Instant::now();
        let verdict = session.answer("verify", json!([result_json]));
        let verify_s = verify_started.elapsed().as_secs_f64();
        println!(
            "through the C interface: input {input_s:.2} s ({rounds} rounds), OS {os_s:.2} s, prove {prove_s:.1} s, \
             verify {verify_s:.2} s {verdict}; proof {} to the native prover's",
            if same { "IDENTICAL" } else { "DIFFERENT" },
        );
        assert!(same && verdict == json!({ "ok": null }));
        return;
    };

    let pointer = SessionPointer(session.0);
    let canceller = std::thread::spawn(move || {
        let pointer = pointer;
        std::thread::sleep(cancel_after);
        unsafe { tx_session_cancel(pointer.0) };
        Instant::now()
    });
    let prove_started = Instant::now();
    let answer = session.answer("prove", json!([]));
    let returned = Instant::now();
    let cancelled_at = canceller.join().unwrap();
    let later_started = Instant::now();
    let later = session.answer("tx_hash", json!([]));
    let later_s = later_started.elapsed().as_secs_f64();
    assert_eq!(later["cancelled"], true, "a call after the cancel: {later}");
    let into = |instant: Instant| instant.saturating_duration_since(prove_started).as_secs_f64();
    let outcome = if answer.get("ok").is_some() {
        "the proof (no checkpoint left)".to_string()
    } else {
        assert_eq!(answer["cancelled"], true, "{answer}");
        answer.to_string()
    };
    if returned > cancelled_at {
        println!(
            "cancelled {:.1} s into the prove: returned {:.3} s after the cancel with {outcome}; a later call \
             answered {later} in {later_s:.6} s",
            into(cancelled_at),
            (returned - cancelled_at).as_secs_f64(),
        );
    } else {
        println!("the prove ended {:.1} s in, before the cancel at {:.1} s", into(returned), into(cancelled_at));
    }
}
