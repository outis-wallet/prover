//! Native checks.
//! `os <virtual-os-input.json>`: runs the virtual OS and prints the PIE summary.
//! `verify <response.json>`: verifies a starknet_proveTransaction result.
//! `replay <capture dir> [out.json]`: the whole TxSession pipeline on a recording made by
//! rpc-record-proxy.py (as replay.mjs does in Node), with timings, peak memory and a comparison
//! with the native prover's response.
use std::path::Path;
use std::time::Instant;

use os_wasm_probe::replay::{Phase, Progress};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let usage = "usage: os_native_probe os <input.json> | verify <response.json> | replay <capture dir> [out.json]";
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
        Some("replay") => replay(Path::new(&args[2]), args.get(3).map(Path::new)),
        _ => panic!("{usage}"),
    }
}

fn replay(capture: &Path, out: Option<&Path>) {
    let report = os_wasm_probe::replay::replay(capture, &|event| match event {
        Progress::Transaction { block, tx_hash } => println!("block {block}, tx {tx_hash}"),
        Progress::Note { message } => println!("{message}"),
        Progress::Phase { phase: Phase::Prove } => println!("proving"),
        _ => {}
    });
    if let Some(out) = out {
        std::fs::write(out, &report.result_json).expect("write result");
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
