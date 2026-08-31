//! Deterministic executable reference for the model-worker JSONL boundary.
//!
//! This binary deliberately has no model dependency. It is useful for a
//! supervisor harness: `--crash` exits during analysis; `--wait-for-cancel`
//! reports one chunk then waits for a cancellation record; default mode emits
//! one empty, typed sidecar after verified progress.

#[allow(dead_code)]
#[path = "../beat_this.rs"]
mod beat_this;
#[allow(dead_code)]
#[path = "../model_claim.rs"]
mod model_claim;
#[allow(dead_code)]
#[path = "../model_registry.rs"]
mod model_registry;
#[allow(dead_code)]
#[path = "../model_store.rs"]
mod model_store;
#[allow(dead_code)]
#[path = "../model_supervisor.rs"]
mod model_supervisor;
#[allow(dead_code)]
#[path = "../model_task_service.rs"]
mod model_task_service;
#[allow(dead_code)]
#[path = "../model_wire.rs"]
mod model_wire;
#[allow(dead_code)]
#[path = "../model_worker.rs"]
mod model_worker;
#[allow(dead_code)]
#[path = "../worker_runtime.rs"]
mod worker_runtime;

use std::collections::BTreeSet;
use std::fs;
use std::io::{self, BufRead, Write};
use std::path::Path;

use model_wire::{
    AdditivityDeclaration, ArtifactDescriptor, ArtifactKind, ProgressReport, ResultPhase,
    SessionValidator, WireCapabilities, WireEnvelope, WireMessage, WorkerResult,
};

fn main() {
    let mode = std::env::args().nth(1).unwrap_or_else(|| "happy".into());
    let stdin = io::stdin();
    let mut stdout = io::BufWriter::new(io::stdout());
    let mut session = SessionValidator::default();
    let mut outbound_sequence = 0_u64;

    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let incoming = match WireEnvelope::from_jsonl(&line) {
            Ok(value) => value,
            Err(error) => {
                eprintln!("fake worker could not decode input: {error}");
                std::process::exit(64);
            }
        };
        if let Err(error) = session.observe_controller(&incoming) {
            eprintln!("fake worker rejected controller transition: {error}");
            std::process::exit(65);
        }
        match incoming.message {
            WireMessage::Hello => send(
                &mut stdout,
                &mut session,
                &mut outbound_sequence,
                WireMessage::Capabilities {
                    capabilities: WireCapabilities {
                        worker_name: "audec-fake-worker".into(),
                        backends: BTreeSet::from(["cpu".into()]),
                        maximum_parallel_jobs: 1,
                        shared_memory: false,
                    },
                },
            ),
            WireMessage::LoadModel { manifest_sha256 } => send(
                &mut stdout,
                &mut session,
                &mut outbound_sequence,
                WireMessage::ModelLoaded { manifest_sha256 },
            ),
            WireMessage::Analyze { request } | WireMessage::Separate { request } => {
                if mode == "crash" {
                    std::process::exit(70);
                }
                send(
                    &mut stdout,
                    &mut session,
                    &mut outbound_sequence,
                    WireMessage::Progress {
                        progress: ProgressReport {
                            job_id: request.job_id.clone(),
                            phase: ResultPhase::Analyzing,
                            completed_chunks: 1,
                            total_chunks: 2,
                        },
                    },
                );
                if mode == "wait-for-cancel" {
                    continue;
                }
                let staging = Path::new(&request.files.staging_directory);
                fs::create_dir_all(staging)
                    .expect("fake worker creates assigned staging directory");
                fs::write(staging.join("empty.json"), []).expect("fake worker writes sidecar");
                send(
                    &mut stdout,
                    &mut session,
                    &mut outbound_sequence,
                    WireMessage::Progress {
                        progress: ProgressReport {
                            job_id: request.job_id.clone(),
                            phase: ResultPhase::Verifying,
                            completed_chunks: 2,
                            total_chunks: 2,
                        },
                    },
                );
                send(
                    &mut stdout,
                    &mut session,
                    &mut outbound_sequence,
                    WireMessage::Complete {
                        result: WorkerResult {
                            job_id: request.job_id,
                            cache_key: request.cache_key,
                            artifacts: vec![ArtifactDescriptor {
                                relative_path: "empty.json".into(),
                                sha256: "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855".into(),
                                byte_len: 0,
                                kind: ArtifactKind::Measurement,
                                media_type: "application/json".into(),
                                schema_revision: 1,
                                time_base_hz: None,
                                additivity: AdditivityDeclaration::NonAudio,
                                source_backlinks: vec![],
                            }],
                            measurements: vec![],
                        },
                    },
                );
            }
            WireMessage::Cancel { job_id } => send(
                &mut stdout,
                &mut session,
                &mut outbound_sequence,
                WireMessage::Cancelled { job_id },
            ),
            WireMessage::Shutdown => return,
            _ => unreachable!("session validator admits controller records only"),
        }
    }
}

fn send(
    stdout: &mut impl Write,
    session: &mut SessionValidator,
    sequence: &mut u64,
    message: WireMessage,
) {
    let envelope = WireEnvelope::new(*sequence, message);
    session
        .observe_worker(&envelope)
        .expect("fake worker emits valid protocol transitions");
    stdout
        .write_all(
            envelope
                .to_jsonl()
                .expect("fake worker encodes JSONL")
                .as_bytes(),
        )
        .expect("fake worker writes stdout");
    stdout
        .flush()
        .expect("fake worker flushes protocol progress");
    *sequence += 1;
}
