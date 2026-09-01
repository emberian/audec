//! Deterministic subprocess fixture for the plugin scanner/runtime protocol.
//!
//! Crash/hang modes let a parent watchdog verify scan and realtime failure
//! diagnostics and recovery. No dynamic library is loaded.

#[allow(dead_code)]
#[path = "../plugin.rs"]
mod plugin;
#[allow(dead_code)]
#[path = "../plugin_wire.rs"]
mod plugin_wire;
#[allow(dead_code)]
#[path = "../plugin_worker.rs"]
mod plugin_worker;

use std::io::{self, BufRead, Write};
use std::path::PathBuf;

use plugin_wire::{Envelope, Message, SessionValidator};
use plugin_worker::FakeWorker;

fn main() {
    let arguments = std::env::args().skip(1).collect::<Vec<_>>();
    let crash_on_scan = arguments.iter().any(|value| value == "--crash-on-scan");
    let hang_on_scan = arguments.iter().any(|value| value == "--hang-on-scan");
    let crash_on_process = arguments.iter().any(|value| value == "--crash-on-process");
    let hang_on_process = arguments.iter().any(|value| value == "--hang-on-process");
    let session_root = arguments
        .windows(2)
        .find(|pair| pair[0] == "--session-root")
        .map(|pair| PathBuf::from(&pair[1]))
        .unwrap_or_else(|| std::env::current_dir().expect("fake worker current directory"));

    let stdin = io::stdin();
    let mut stdout = io::BufWriter::new(io::stdout());
    let mut session = SessionValidator::default();
    let mut worker = FakeWorker::new(session_root);
    let mut worker_sequence = 0_u64;

    for line in stdin.lock().lines() {
        let line = match line {
            Ok(line) => line,
            Err(error) => fatal(64, &format!("could not read protocol input: {error}")),
        };
        let incoming = match Envelope::from_jsonl(&line) {
            Ok(envelope) => envelope,
            Err(error) => fatal(65, &format!("could not decode protocol input: {error}")),
        };
        if let Err(error) = session.observe_controller(&incoming) {
            fatal(66, &format!("invalid controller transition: {error}"));
        }
        if matches!(incoming.message, Message::Scan { .. }) {
            if crash_on_scan {
                std::process::exit(70);
            }
            if hang_on_scan {
                loop {
                    std::thread::park();
                }
            }
        }
        if matches!(incoming.message, Message::Process { .. }) {
            if crash_on_process {
                std::process::exit(71);
            }
            if hang_on_process {
                loop {
                    std::thread::park();
                }
            }
        }
        let response = match worker.handle(incoming.message) {
            Ok(response) => response,
            Err(error) => Some(FakeWorker::failure(&error)),
        };
        let Some(response) = response else {
            return;
        };
        let envelope = Envelope::new(worker_sequence, response);
        if let Err(error) = session.observe_worker(&envelope) {
            fatal(67, &format!("invalid worker transition: {error}"));
        }
        let encoded = envelope
            .to_jsonl()
            .unwrap_or_else(|error| fatal(68, &format!("could not encode response: {error}")));
        stdout
            .write_all(encoded.as_bytes())
            .unwrap_or_else(|error| fatal(69, &format!("could not write response: {error}")));
        stdout
            .flush()
            .unwrap_or_else(|error| fatal(69, &format!("could not flush response: {error}")));
        worker_sequence += 1;
    }
}

fn fatal(code: i32, detail: &str) -> ! {
    eprintln!("audec-fake-plugin: {detail}");
    std::process::exit(code);
}
