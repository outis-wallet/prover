//! The C entry points for proving a live transaction natively (the wallet apps): a [`TxSession`]
//! behind a handle, called method by method with JSON in and out, as `js/tx-driver.mjs` drives
//! the wasm one, and stoppable from another thread. `include/outis_prover.h` declares them;
//! `tx_prove_free` (replay.rs) frees their strings.
//!
//! No panic leaves these functions (one crossing into JNI would end the app): each runs inside
//! `catch_unwind` and turns a panic into an error. A call that panicked leaves the session
//! unusable: later calls return an error, and it can only be closed.
use std::any::Any;
use std::ffi::{c_char, CStr, CString};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use serde_json::{json, Value};

use crate::TxSession;

/// One transaction's session, its cancel flag and the thread pool its calls run on.
pub struct TxNativeSession {
    /// Taken by each call for its whole run; never by `tx_session_cancel`.
    session: Mutex<TxSession>,
    cancel: Arc<AtomicBool>,
    pool: rayon::ThreadPool,
}

impl TxNativeSession {
    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    fn call(&self, method: &str, args_json: &str) -> Value {
        if self.cancelled() {
            return cancelled("cancelled");
        }
        let args: Vec<Value> = match serde_json::from_str(args_json) {
            Ok(args) => args,
            Err(error) => return json!({ "error": format!("args_json must be a JSON array: {error}") }),
        };
        // Held until the answer is made: once close() has the lock, this call touches nothing of
        // the session.
        let Ok(mut session) = self.session.lock() else {
            return json!({ "error": "an earlier call panicked: the session can only be closed" });
        };
        if self.cancelled() {
            return cancelled("cancelled");
        }
        let session = &mut *session;
        match self.pool.install(|| dispatch(session, method, &args)) {
            Ok(value) => json!({ "ok": value }),
            Err(message) if self.cancelled() => cancelled(&message),
            Err(message) => json!({ "error": message }),
        }
    }
}

fn cancelled(message: &str) -> Value {
    json!({ "error": message, "cancelled": true })
}

/// Runs one TxSession method; its arguments are `args`, in the wasm method's order.
fn dispatch(session: &mut TxSession, method: &str, args: &[Value]) -> Result<Value, String> {
    let string = |index: usize| {
        args.get(index)
            .and_then(Value::as_str)
            .ok_or_else(|| format!("{method}: argument {index} must be a string"))
    };
    Ok(match method {
        "tx_hash" => Value::String(session.tx_hash()),
        "block_number" => json!(session.block_number()),
        "set_placeholders" => {
            let on = args
                .first()
                .and_then(Value::as_bool)
                .ok_or_else(|| format!("{method}: argument 0 must be a bool"))?;
            session.set_placeholders(on);
            Value::Null
        }
        "add_reads" => {
            session.add_reads(string(0)?).map_err(|f| f.0)?;
            Value::Null
        }
        "add_class" => {
            session.add_class(string(0)?, string(1)?, string(2)?).map_err(|f| f.0)?;
            Value::Null
        }
        "add_undeclared_class" => {
            session.add_undeclared_class(string(0)?).map_err(|f| f.0)?;
            Value::Null
        }
        "execute" => Value::String(session.execute().map_err(|f| f.0)?),
        // The input stays in the session: it is megabytes, and nothing reads it natively.
        "set_storage_proofs" => {
            session.build_os_input(string(0)?).map_err(|f| f.0)?;
            Value::Null
        }
        "run_os" => Value::String(session.run_os().map_err(|f| f.0)?),
        "prove" => Value::String(session.prove().map_err(|f| f.0)?),
        "verify" => {
            crate::verify_result(string(0)?)?;
            Value::Null
        }
        _ => return Err(format!("unknown method: {method}")),
    })
}

/// The thread count of a pool whose caller asks for 0: one per physical core. Hyperthreads don't
/// speed the proof up: on an 18-core, 36-thread x86-64 machine, 36 threads proved no faster than
/// 18, and the extra idle threads spent seconds of system time spinning in `sched_yield`. Phones
/// have one thread per core, so there it is one per core as before. Never more than the threads
/// the process may run on (its affinity, a cgroup's quota).
pub fn default_threads() -> usize {
    let available = std::thread::available_parallelism().map_or(1, |threads| threads.get());
    physical_cores().map_or(available, |cores| cores.clamp(1, available))
}

/// The physical cores: the distinct sets of hardware threads that share a core, among the online
/// CPUs (an offline CPU has no topology directory).
#[cfg(any(target_os = "linux", target_os = "android"))]
fn physical_cores() -> Option<usize> {
    let mut cores = std::collections::HashSet::new();
    for entry in std::fs::read_dir("/sys/devices/system/cpu").ok()?.flatten() {
        let name = entry.file_name();
        let is_cpu = name.to_str().and_then(|name| name.strip_prefix("cpu")).is_some_and(|index| {
            !index.is_empty() && index.bytes().all(|byte| byte.is_ascii_digit())
        });
        if !is_cpu {
            continue;
        }
        if let Ok(siblings) = std::fs::read_to_string(entry.path().join("topology/thread_siblings_list")) {
            cores.insert(siblings.trim().to_owned());
        }
    }
    (!cores.is_empty()).then_some(cores.len())
}

/// The physical cores (`hw.physicalcpu`).
#[cfg(target_vendor = "apple")]
fn physical_cores() -> Option<usize> {
    unsafe extern "C" {
        fn sysctlbyname(
            name: *const c_char,
            old: *mut std::ffi::c_void,
            old_len: *mut usize,
            new: *mut std::ffi::c_void,
            new_len: usize,
        ) -> std::ffi::c_int;
    }
    let mut cores: i32 = 0;
    let mut size = size_of::<i32>();
    let read = unsafe {
        sysctlbyname(c"hw.physicalcpu".as_ptr(), (&raw mut cores).cast(), &mut size, std::ptr::null_mut(), 0)
    };
    (read == 0 && cores > 0).then_some(cores as usize)
}

#[cfg(not(any(target_os = "linux", target_os = "android", target_vendor = "apple")))]
fn physical_cores() -> Option<usize> {
    None
}

/// Opens a session for one transaction (see `TxSession::new`) on a rayon pool of `threads`
/// threads (0: one per physical core, see [`default_threads`]) with 16 MiB stacks: a call runs wholly on a pool thread, not just
/// its parallel parts, so the threads get a main thread's stack (as the wasm build's
/// -zstack-size) instead of rayon's 2 MiB. Returns null on failure and, if `error` is not null,
/// sets `*error` to the message (free it with `tx_prove_free`); on success sets `*error` to null.
///
/// # Safety
/// The strings must be valid NUL-terminated strings (or null, an error); `error` null or valid
/// for a write.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tx_session_open(
    chain_id: *const c_char,
    header_json: *const c_char,
    rpc_transaction_json: *const c_char,
    threads: u32,
    error: *mut *mut c_char,
) -> *mut TxNativeSession {
    let open = AssertUnwindSafe(|| -> Result<Box<TxNativeSession>, String> {
        let chain_id = unsafe { argument(chain_id, "chain_id") }?;
        let header_json = unsafe { argument(header_json, "header_json") }?;
        let rpc_transaction_json = unsafe { argument(rpc_transaction_json, "rpc_transaction_json") }?;
        let mut session = TxSession::new(chain_id, header_json, rpc_transaction_json).map_err(|f| f.0)?;
        let cancel = Arc::new(AtomicBool::new(false));
        session.set_cancel_flag(cancel.clone());
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(if threads == 0 { default_threads() } else { threads as usize })
            .stack_size(16 << 20)
            .thread_name(|index| format!("outis-prover-{index}"))
            .build()
            .map_err(|e| format!("thread pool: {e}"))?;
        Ok(Box::new(TxNativeSession { session: Mutex::new(session), cancel, pool }))
    });
    let opened = catch_unwind(open).unwrap_or_else(|panic| Err(panic_message(panic)));
    let (session, message) = match opened {
        Ok(session) => (Box::into_raw(session), std::ptr::null_mut()),
        Err(message) => (std::ptr::null_mut(), into_c_string(message)),
    };
    if !error.is_null() {
        unsafe { *error = message };
    } else if !message.is_null() {
        drop(unsafe { CString::from_raw(message) });
    }
    session
}

/// Calls one method of the session: `method` is a TxSession method (tx_hash, block_number,
/// set_placeholders, add_reads, add_class, add_undeclared_class, execute, set_storage_proofs,
/// run_os, prove) or verify, and `args_json` a JSON array of its arguments in the wasm method's
/// order (null: no arguments). Returns `{"ok": value}`, `{"error": message}` or
/// `{"error": message, "cancelled": true}`; free it with `tx_prove_free`. Runs in the session's
/// pool; calls from several threads take turns.
///
/// # Safety
/// `session` must come from `tx_session_open` and not be closed; the strings must be valid
/// NUL-terminated strings (`args_json` may be null).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tx_session_call(
    session: *mut TxNativeSession,
    method: *const c_char,
    args_json: *const c_char,
) -> *mut c_char {
    let call = AssertUnwindSafe(|| {
        let Some(session) = (unsafe { session.as_ref() }) else {
            return json!({ "error": "no session" });
        };
        let method = match unsafe { argument(method, "method") } {
            Ok(method) => method,
            Err(message) => return json!({ "error": message }),
        };
        let args_json =
            if args_json.is_null() { Ok("[]") } else { unsafe { argument(args_json, "args_json") } };
        match args_json {
            Ok(args_json) => session.call(method, args_json),
            Err(message) => json!({ "error": message }),
        }
    });
    let answer = catch_unwind(call).unwrap_or_else(|panic| json!({ "error": panic_message(panic) }));
    into_c_string(answer.to_string())
}

/// Asks the session to stop: a call in progress returns `{"error": ..., "cancelled": true}` at
/// its next step, and every later call at once. Never waits: safe from any thread while a call
/// runs, but not once `tx_session_close` started.
///
/// # Safety
/// `session` must come from `tx_session_open` and not be closed (null is ignored).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tx_session_cancel(session: *mut TxNativeSession) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        if let Some(session) = unsafe { session.as_ref() } {
            session.cancel.store(true, Ordering::Relaxed);
        }
    }));
}

/// Frees the session. If a call is still running, waits for it to return first (cancel it to
/// make that quick); no call may start once this has been called.
///
/// # Safety
/// `session` must come from `tx_session_open` and not be closed before (null is ignored).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tx_session_close(session: *mut TxNativeSession) {
    if session.is_null() {
        return;
    }
    let _ = catch_unwind(AssertUnwindSafe(|| {
        // A running call holds the lock until it is done with the session.
        drop(unsafe { &*session }.session.lock().unwrap_or_else(PoisonError::into_inner));
        drop(unsafe { Box::from_raw(session) });
    }));
}

/// Turns the GPU commitment on (`enabled` not 0) or off for every proof from now on, in this
/// process (see `gpu`; only in a library built with the `gpu` feature). Off by default, unless
/// `OUTIS_GPU=1` is in the environment at the first proof and this was not called. Returns
/// `{"ok": adapter}` when on (e.g. "NVIDIA GeForce RTX 3080 (Vulkan)"), `{"ok": null}` when off, or
/// `{"error": message}` when there is no GPU to use (the proofs then stay on the CPU). The first
/// call that turns it on creates the device and compiles the kernels, which can take a second.
/// Free the answer with `tx_prove_free`.
#[cfg(feature = "gpu")]
#[unsafe(no_mangle)]
pub extern "C" fn tx_prove_set_gpu(enabled: u32) -> *mut c_char {
    let set = AssertUnwindSafe(|| match crate::gpu::set_gpu(enabled != 0) {
        Ok(adapter) => json!({ "ok": adapter }),
        Err(message) => json!({ "error": message }),
    });
    let answer = catch_unwind(set).unwrap_or_else(|panic| json!({ "error": panic_message(panic) }));
    into_c_string(answer.to_string())
}

/// A C string argument as UTF-8.
unsafe fn argument<'a>(pointer: *const c_char, name: &str) -> Result<&'a str, String> {
    if pointer.is_null() {
        return Err(format!("{name} is null"));
    }
    unsafe { CStr::from_ptr(pointer) }.to_str().map_err(|_| format!("{name} is not UTF-8"))
}

fn into_c_string(text: String) -> *mut c_char {
    // JSON escapes NUL; only an error message of tx_session_open could hold one.
    CString::new(text.replace('\0', "")).expect("no NUL").into_raw()
}

fn panic_message(panic: Box<dyn Any + Send>) -> String {
    let message = panic
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| panic.downcast_ref::<&str>().copied())
        .unwrap_or("no message");
    format!("panic: {message}")
}
