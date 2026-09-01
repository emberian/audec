//! Isolated pure-Rust Beat This! worker.
//!
//! The executable is opt-in (`beat-this-rten-worker`) and never acquires model
//! files. Its CLI receives one already-verified installation directory. Raw
//! logits and decoded events are separate immutable artifacts; neither is a
//! project edit or an instrument label.

#[allow(dead_code)]
#[path = "../beat_this.rs"]
mod audec_beat_this;
#[allow(dead_code)]
#[path = "../inference_recipe.rs"]
mod inference_recipe;
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
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use ::beat_this::{BeatThis, RtenRuntime};
use audec_beat_this::{
    small0_registration, ADAPTER_PROTOCOL, BEAT_MODEL_FILE, BEAT_MODEL_SHA256, CHUNK_FRAMES,
    INPUT_SAMPLE_RATE_HZ, LOGIT_FRAME_RATE_HZ, MEL_MODEL_FILE, MEL_MODEL_SHA256, OVERLAP_FRAMES,
    WORKER_NAME,
};
use model_wire::{
    AdditivityDeclaration, AnalyzeRequest, ArtifactDescriptor, ArtifactKind, ProgressReport,
    ResultPhase, SessionValidator, SourceBacklink, WireCapabilities, WireEnvelope, WireMessage,
    WireParameter, WorkerFailure, WorkerFailureKind, WorkerResult,
};
use model_worker::sha256_bytes;
use serde::Serialize;

type Tracker = BeatThis<<RtenRuntime as ::beat_this::Runtime>::Model>;

fn main() {
    if let Err(error) = run() {
        eprintln!("audec Beat This worker: {error}");
        std::process::exit(64);
    }
}

fn run() -> Result<(), String> {
    let model_directory = parse_model_directory()?;
    let expected_manifest = small0_registration()
        .map_err(|error| error.to_string())?
        .manifest
        .canonical_hash()
        .map_err(|error| error.to_string())?
        .to_string();

    let (input_tx, input_rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("audec-beat-this-protocol-input".into())
        .spawn(move || {
            for line in io::stdin().lock().lines() {
                let message = line.map_err(|error| error.to_string()).and_then(|line| {
                    WireEnvelope::from_jsonl(&line).map_err(|error| error.to_string())
                });
                if input_tx.send(message).is_err() {
                    return;
                }
            }
        })
        .map_err(|error| format!("could not start protocol reader: {error}"))?;

    let mut stdout = io::BufWriter::new(io::stdout());
    let mut session = SessionValidator::default();
    let mut output_sequence = 0_u64;
    let mut tracker: Option<Arc<Mutex<Tracker>>> = None;
    let mut active: Option<ActiveJob> = None;

    loop {
        if let Some(job) = active.as_ref() {
            match job.results.try_recv() {
                Ok(result) => {
                    finish_analysis(
                        &mut stdout,
                        &mut session,
                        &mut output_sequence,
                        job.total_chunks,
                        result,
                    )?;
                    active = None;
                    continue;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    return Err("analysis thread ended without a typed result".into())
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }

        let incoming = match input_rx.recv_timeout(Duration::from_millis(10)) {
            Ok(Ok(value)) => value,
            Ok(Err(error)) => return Err(format!("could not decode controller record: {error}")),
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
        };
        session
            .observe_controller(&incoming)
            .map_err(|error| format!("rejected controller transition: {error}"))?;

        match incoming.message {
            WireMessage::Hello => send(
                &mut stdout,
                &mut session,
                &mut output_sequence,
                WireMessage::Capabilities {
                    capabilities: WireCapabilities {
                        worker_name: WORKER_NAME.into(),
                        backends: BTreeSet::from(["cpu".into()]),
                        maximum_parallel_jobs: 1,
                        shared_memory: false,
                    },
                },
            )?,
            WireMessage::LoadModel { manifest_sha256 } => {
                if manifest_sha256 != expected_manifest {
                    return Err("controller requested a manifest other than the pinned Beat This RTen small graph".into());
                }
                verify_file(&model_directory.join(MEL_MODEL_FILE), MEL_MODEL_SHA256)?;
                verify_file(&model_directory.join(BEAT_MODEL_FILE), BEAT_MODEL_SHA256)?;
                let loaded = BeatThis::new(
                    &RtenRuntime,
                    &model_directory.join(MEL_MODEL_FILE),
                    &model_directory.join(BEAT_MODEL_FILE),
                )
                .map_err(|error| format!("could not load pinned RTen graphs: {error}"))?;
                tracker = Some(Arc::new(Mutex::new(loaded)));
                send(
                    &mut stdout,
                    &mut session,
                    &mut output_sequence,
                    WireMessage::ModelLoaded { manifest_sha256 },
                )?;
            }
            WireMessage::Analyze { request } | WireMessage::Separate { request } => {
                if active.is_some() {
                    return Err(
                        "worker received concurrent analysis despite advertising one slot".into(),
                    );
                }
                let Some(tracker) = tracker.clone() else {
                    return Err("analysis arrived before the RTen graphs were loaded".into());
                };
                validate_request(&request, &expected_manifest)?;
                let total_chunks = model_chunk_count(request.frame_count);
                send(
                    &mut stdout,
                    &mut session,
                    &mut output_sequence,
                    WireMessage::Progress {
                        progress: ProgressReport {
                            job_id: request.job_id.clone(),
                            phase: ResultPhase::Analyzing,
                            completed_chunks: 0,
                            total_chunks,
                        },
                    },
                )?;
                let (result_tx, result_rx) = mpsc::channel();
                std::thread::Builder::new()
                    .name(format!("audec-beat-this-{}", request.job_id))
                    .spawn(move || {
                        let result = analyze(&tracker, request);
                        let _ = result_tx.send(result);
                    })
                    .map_err(|error| format!("could not start analysis thread: {error}"))?;
                active = Some(ActiveJob {
                    results: result_rx,
                    total_chunks,
                });
            }
            WireMessage::Cancel { job_id } => {
                if active.is_none() {
                    return Err("cancellation arrived without active inference".into());
                }
                send(
                    &mut stdout,
                    &mut session,
                    &mut output_sequence,
                    WireMessage::Cancelled { job_id },
                )?;
                // RTen's audited public API currently exposes whole-analysis
                // inference, not a chunk callback. Exit after the terminal
                // acknowledgement so process isolation provides prompt,
                // truthful cancellation rather than publishing late output.
                stdout.flush().map_err(|error| error.to_string())?;
                std::process::exit(0);
            }
            WireMessage::Shutdown => return Ok(()),
            _ => return Err("controller sent a worker-direction message".into()),
        }
    }
}

struct ActiveJob {
    results: mpsc::Receiver<Result<WorkerResult, WorkerFailure>>,
    total_chunks: u64,
}

fn finish_analysis(
    stdout: &mut impl Write,
    session: &mut SessionValidator,
    sequence: &mut u64,
    total_chunks: u64,
    result: Result<WorkerResult, WorkerFailure>,
) -> Result<(), String> {
    match result {
        Ok(result) => {
            for phase in [ResultPhase::Encoding, ResultPhase::Verifying] {
                send(
                    stdout,
                    session,
                    sequence,
                    WireMessage::Progress {
                        progress: ProgressReport {
                            job_id: result.job_id.clone(),
                            phase,
                            completed_chunks: total_chunks,
                            total_chunks,
                        },
                    },
                )?;
            }
            send(stdout, session, sequence, WireMessage::Complete { result })
        }
        Err(error) => send(stdout, session, sequence, WireMessage::Error { error }),
    }
}

fn analyze(
    tracker: &Mutex<Tracker>,
    request: AnalyzeRequest,
) -> Result<WorkerResult, WorkerFailure> {
    analyze_inner(tracker, &request).map_err(|detail| WorkerFailure {
        job_id: request.job_id,
        kind: WorkerFailureKind::Adapter,
        detail,
    })
}

fn analyze_inner(
    tracker: &Mutex<Tracker>,
    request: &AnalyzeRequest,
) -> Result<WorkerResult, String> {
    let bytes = fs::read(&request.files.material)
        .map_err(|error| format!("could not read assigned material: {error}"))?;
    if sha256_bytes(&bytes).to_string() != request.material_sha256 {
        return Err("material bytes do not match the supervisor digest".into());
    }
    let samples = decode_f32le(&bytes, request.frame_count)?;
    let analysis = tracker
        .lock()
        .map_err(|_| "RTen tracker lock was poisoned".to_string())?
        .analyze_audio(&samples, INPUT_SAMPLE_RATE_HZ)
        .map_err(|error| format!("RTen inference failed: {error}"))?;

    if analysis.beat_logits.len() != analysis.downbeat_logits.len()
        || analysis
            .beat_logits
            .iter()
            .chain(&analysis.downbeat_logits)
            .any(|value| !value.is_finite())
    {
        return Err("RTen returned malformed or non-finite logits".into());
    }

    let staging = Path::new(&request.files.staging_directory);
    fs::create_dir_all(staging)
        .map_err(|error| format!("could not create assigned staging directory: {error}"))?;
    let backlink = SourceBacklink {
        material_sha256: request.material_sha256.clone(),
        start_frame: request.start_frame,
        frame_count: request.frame_count,
    };
    let artifacts = vec![
        write_artifact(
            staging,
            "beat-logits.json",
            ArtifactKind::Measurement,
            Some(LOGIT_FRAME_RATE_HZ),
            &LogitArtifact {
                schema: "audec.beat-this.logits.v1",
                source_start_frame: request.start_frame,
                source_sample_rate_hz: INPUT_SAMPLE_RATE_HZ,
                frame_rate_hz: LOGIT_FRAME_RATE_HZ,
                logits: &analysis.beat_logits,
            },
            backlink.clone(),
        )?,
        write_artifact(
            staging,
            "downbeat-logits.json",
            ArtifactKind::Measurement,
            Some(LOGIT_FRAME_RATE_HZ),
            &LogitArtifact {
                schema: "audec.beat-this.logits.v1",
                source_start_frame: request.start_frame,
                source_sample_rate_hz: INPUT_SAMPLE_RATE_HZ,
                frame_rate_hz: LOGIT_FRAME_RATE_HZ,
                logits: &analysis.downbeat_logits,
            },
            backlink.clone(),
        )?,
        write_artifact(
            staging,
            "beat-events.json",
            ArtifactKind::EventMap,
            Some(INPUT_SAMPLE_RATE_HZ),
            &EventArtifact {
                schema: "audec.beat-this.events.v1",
                source_start_frame: request.start_frame,
                source_sample_rate_hz: INPUT_SAMPLE_RATE_HZ,
                times_seconds: &analysis.beats,
            },
            backlink.clone(),
        )?,
        write_artifact(
            staging,
            "downbeat-events.json",
            ArtifactKind::EventMap,
            Some(INPUT_SAMPLE_RATE_HZ),
            &EventArtifact {
                schema: "audec.beat-this.events.v1",
                source_start_frame: request.start_frame,
                source_sample_rate_hz: INPUT_SAMPLE_RATE_HZ,
                times_seconds: &analysis.downbeats,
            },
            backlink,
        )?,
    ];
    Ok(WorkerResult {
        job_id: request.job_id.clone(),
        cache_key: request.cache_key.clone(),
        artifacts,
        measurements: Vec::new(),
    })
}

#[derive(Serialize)]
struct LogitArtifact<'a> {
    schema: &'static str,
    source_start_frame: u64,
    source_sample_rate_hz: u32,
    frame_rate_hz: u32,
    logits: &'a [f32],
}

#[derive(Serialize)]
struct EventArtifact<'a> {
    schema: &'static str,
    source_start_frame: u64,
    source_sample_rate_hz: u32,
    times_seconds: &'a [f32],
}

fn write_artifact(
    staging: &Path,
    relative_path: &str,
    kind: ArtifactKind,
    time_base_hz: Option<u32>,
    value: &impl Serialize,
    backlink: SourceBacklink,
) -> Result<ArtifactDescriptor, String> {
    let mut bytes = serde_json::to_vec(value).map_err(|error| error.to_string())?;
    bytes.push(b'\n');
    fs::write(staging.join(relative_path), &bytes)
        .map_err(|error| format!("could not stage {relative_path}: {error}"))?;
    Ok(ArtifactDescriptor {
        relative_path: relative_path.into(),
        sha256: sha256_bytes(&bytes).to_string(),
        byte_len: bytes.len() as u64,
        kind,
        media_type: "application/vnd.audec.beat-this+json".into(),
        schema_revision: 1,
        time_base_hz,
        additivity: AdditivityDeclaration::NonAudio,
        source_backlinks: vec![backlink],
    })
}

fn validate_request(request: &AnalyzeRequest, expected_manifest: &str) -> Result<(), String> {
    if request.model_manifest_sha256 != expected_manifest {
        return Err("analysis manifest differs from the loaded pinned manifest".into());
    }
    if !request.channel_selection.is_empty()
        || request.prompt.is_some()
        || !request.reference_sha256.is_empty()
        || !request.mask_sha256.is_empty()
    {
        return Err("Beat This accepts only already-downmixed mono material".into());
    }
    let expected = [
        ("adapter", WireParameter::String(ADAPTER_PROTOCOL.into())),
        ("chunk_frames", WireParameter::Unsigned(CHUNK_FRAMES)),
        ("input_encoding", WireParameter::String("float32le".into())),
        (
            "input_rate_hz",
            WireParameter::Unsigned(u64::from(INPUT_SAMPLE_RATE_HZ)),
        ),
        ("overlap_frames", WireParameter::Unsigned(OVERLAP_FRAMES)),
        ("postprocess", WireParameter::String("minimal-v1".into())),
    ];
    if request.parameters.len() != expected.len()
        || expected
            .iter()
            .any(|(name, value)| request.parameters.get(*name) != Some(value))
    {
        return Err("analysis parameters differ from the pinned adapter recipe".into());
    }
    Ok(())
}

fn decode_f32le(bytes: &[u8], frames: u64) -> Result<Vec<f32>, String> {
    let expected = frames
        .checked_mul(4)
        .and_then(|value| usize::try_from(value).ok())
        .ok_or_else(|| "material frame count overflows memory size".to_string())?;
    if bytes.len() != expected {
        return Err(format!(
            "material contains {} bytes; expected {expected}",
            bytes.len()
        ));
    }
    bytes
        .chunks_exact(4)
        .enumerate()
        .map(|(index, chunk)| {
            let value = f32::from_le_bytes(chunk.try_into().expect("four-byte chunk"));
            value
                .is_finite()
                .then_some(value)
                .ok_or_else(|| format!("material sample {index} is non-finite"))
        })
        .collect()
}

fn model_chunk_count(frames: u64) -> u64 {
    let stride = CHUNK_FRAMES.saturating_sub(OVERLAP_FRAMES).max(1);
    frames
        .saturating_add(stride - 1)
        .saturating_div(stride)
        .max(1)
}

fn verify_file(path: &Path, expected_sha256: &str) -> Result<(), String> {
    let bytes = fs::read(path).map_err(|error| {
        format!(
            "could not read installed artifact {}: {error}",
            path.display()
        )
    })?;
    let actual = sha256_bytes(&bytes).to_string();
    if actual != expected_sha256 {
        return Err(format!(
            "installed artifact {} has digest {actual}, expected {expected_sha256}",
            path.display()
        ));
    }
    Ok(())
}

fn parse_model_directory() -> Result<PathBuf, String> {
    let mut arguments = std::env::args_os().skip(1);
    if arguments.next().as_deref() != Some(std::ffi::OsStr::new("--model-directory")) {
        return Err("usage: audec-beat-this-worker --model-directory <verified-directory>".into());
    }
    let directory = arguments
        .next()
        .map(PathBuf::from)
        .ok_or_else(|| "missing model directory".to_string())?;
    if arguments.next().is_some() {
        return Err("unexpected worker arguments".into());
    }
    directory
        .canonicalize()
        .map_err(|error| format!("could not resolve model directory: {error}"))
}

fn send(
    stdout: &mut impl Write,
    session: &mut SessionValidator,
    sequence: &mut u64,
    message: WireMessage,
) -> Result<(), String> {
    let envelope = WireEnvelope::new(*sequence, message);
    session
        .observe_worker(&envelope)
        .map_err(|error| format!("worker generated invalid transition: {error}"))?;
    stdout
        .write_all(
            envelope
                .to_jsonl()
                .map_err(|error| error.to_string())?
                .as_bytes(),
        )
        .map_err(|error| error.to_string())?;
    stdout.flush().map_err(|error| error.to_string())?;
    *sequence = sequence.saturating_add(1);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_progress_uses_the_pinned_overlap_recipe() {
        assert_eq!(model_chunk_count(1), 1);
        assert_eq!(model_chunk_count(CHUNK_FRAMES), 2);
        assert_eq!(model_chunk_count(CHUNK_FRAMES - OVERLAP_FRAMES), 1);
    }

    #[test]
    fn raw_pcm_boundary_quarantines_non_finite_samples() {
        let bytes = [0.25_f32.to_le_bytes(), f32::NAN.to_le_bytes()].concat();
        assert!(decode_f32le(&bytes, 2)
            .unwrap_err()
            .contains("sample 1 is non-finite"));
        assert!(decode_f32le(&bytes[..4], 2)
            .unwrap_err()
            .contains("expected 8"));
    }

    #[test]
    fn raw_logit_artifact_encoding_is_deterministic_and_typed() {
        let artifact = LogitArtifact {
            schema: "audec.beat-this.logits.v1",
            source_start_frame: 441,
            source_sample_rate_hz: INPUT_SAMPLE_RATE_HZ,
            frame_rate_hz: LOGIT_FRAME_RATE_HZ,
            logits: &[0.5, -1.25],
        };
        let encoded = serde_json::to_vec(&artifact).unwrap();
        assert_eq!(
            String::from_utf8(encoded).unwrap(),
            "{\"schema\":\"audec.beat-this.logits.v1\",\"source_start_frame\":441,\"source_sample_rate_hz\":22050,\"frame_rate_hz\":50,\"logits\":[0.5,-1.25]}"
        );
    }
}
