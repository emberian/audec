//! Disposable real-CLAP scanner and DSP worker.
//!
//! Native code, CLAP ABI callbacks and shared mappings stay in this child. The
//! controller owns deadlines and kills the process on a crash or timeout.

#[path = "../clap_adapter.rs"]
mod clap_adapter;
#[allow(dead_code)]
#[path = "../plugin.rs"]
mod plugin;
#[allow(dead_code)]
#[path = "../plugin_wire.rs"]
mod plugin_wire;
#[allow(dead_code)]
#[path = "../plugin_worker.rs"]
mod plugin_worker;

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, BufRead, Write};
use std::path::PathBuf;

use plugin_wire::{
    digest_bytes, Envelope, Message, RuntimeFailureDto, RuntimeFailureKindDto, ScanFailureDto,
    ScanRecordDto, SessionValidator, StateArtifactDto, TailDto,
};

fn main() {
    let arguments = std::env::args().skip(1).collect::<Vec<_>>();
    let session_root = arguments
        .windows(2)
        .find(|pair| pair[0] == "--session-root")
        .map(|pair| PathBuf::from(&pair[1]))
        .unwrap_or_else(|| std::env::current_dir().expect("CLAP worker current directory"));
    let stdin = io::stdin();
    let mut stdout = io::BufWriter::new(io::stdout());
    let mut session = SessionValidator::default();
    let mut worker_sequence = 0_u64;
    let mut instances = BTreeMap::<u128, RuntimeInstance>::new();

    for line in stdin.lock().lines() {
        let line =
            line.unwrap_or_else(|error| fatal(64, &format!("could not read input: {error}")));
        let incoming = Envelope::from_jsonl(&line)
            .unwrap_or_else(|error| fatal(65, &format!("could not decode input: {error}")));
        session
            .observe_controller(&incoming)
            .unwrap_or_else(|error| fatal(66, &format!("invalid controller transition: {error}")));
        let response = match incoming.message {
            Message::Hello => Message::Capabilities {
                capabilities: clap_adapter::capabilities(),
            },
            Message::Scan { request } => match clap_adapter::scan(&request) {
                Ok(record) => Message::ScanReady {
                    request_id: request.request_id,
                    record: ScanRecordDto::from_domain(&record).unwrap_or_else(|error| {
                        fatal(67, &format!("could not encode scan record: {error}"))
                    }),
                },
                Err(failure) => Message::ScanFailed {
                    request_id: request.request_id,
                    failure: ScanFailureDto::from_domain(&failure).unwrap_or_else(|error| {
                        fatal(67, &format!("could not encode scan failure: {error}"))
                    }),
                },
            },
            Message::Instantiate { request } => {
                let request_id = request.request_id;
                let token = request.instance.clone();
                let plugin = request.plugin.clone();
                match token
                    .value()
                    .map_err(|error| error.to_string())
                    .and_then(|id| {
                        if instances.contains_key(&id) {
                            return Err("duplicate native CLAP instance token".into());
                        }
                        let (native, latency_frames, tail) =
                            clap_adapter::NativeClapInstance::instantiate(&request, &session_root)?;
                        instances.insert(id, RuntimeInstance { plugin, native });
                        Ok((latency_frames, tail))
                    }) {
                    Ok((latency_frames, tail)) => Message::Instantiated {
                        request_id,
                        instance: token,
                        latency_frames,
                        tail: TailDto::from_domain(tail),
                    },
                    Err(detail) => runtime_error(Some(request_id), Some(token), detail),
                }
            }
            Message::BindSharedMemory { binding } => {
                let token = binding.instance.clone();
                match token
                    .value()
                    .map_err(|error| error.to_string())
                    .and_then(|id| instance_mut(&mut instances, id)?.native.bind(&binding))
                {
                    Ok(()) => Message::Bound { instance: token },
                    Err(detail) => runtime_error(None, Some(token), detail),
                }
            }
            Message::SetParameters { request } => {
                let request_id = request.request_id;
                let token = request.instance.clone();
                match token
                    .value()
                    .map_err(|error| error.to_string())
                    .and_then(|id| {
                        instance_mut(&mut instances, id)?
                            .native
                            .set_parameters(&request.values)
                    }) {
                    Ok(()) => Message::ParametersSet {
                        request_id,
                        instance: token,
                    },
                    Err(detail) => runtime_error(Some(request_id), Some(token), detail),
                }
            }
            Message::Activate { instance } => {
                let token = instance;
                match token
                    .value()
                    .map_err(|error| error.to_string())
                    .and_then(|id| instance_mut(&mut instances, id)?.native.activate())
                {
                    Ok(()) => Message::Activated { instance: token },
                    Err(detail) => runtime_error(None, Some(token), detail),
                }
            }
            Message::Process {
                instance,
                process_sequence,
                frames,
                input_event_count,
            } => {
                let token = instance;
                match token
                    .value()
                    .map_err(|error| error.to_string())
                    .and_then(|id| {
                        instance_mut(&mut instances, id)?
                            .native
                            .process(frames, input_event_count)
                    }) {
                    Ok(output_event_count) => Message::Processed {
                        instance: token,
                        process_sequence,
                        output_event_count,
                    },
                    Err(detail) => runtime_error(None, Some(token), detail),
                }
            }
            Message::SaveState { request } => {
                let request_id = request.request_id;
                let token = request.instance.clone();
                let result = token
                    .value()
                    .map_err(|error| error.to_string())
                    .and_then(|id| {
                        let runtime = instance_mut(&mut instances, id)?;
                        let bytes = runtime.native.save_state(request.maximum_bytes)?;
                        let path = session_root.join(&request.output_relative_path);
                        if let Some(parent) = path.parent() {
                            fs::create_dir_all(parent).map_err(|error| {
                                format!("could not create CLAP state directory: {error}")
                            })?;
                        }
                        fs::write(&path, &bytes)
                            .map_err(|error| format!("could not write CLAP state: {error}"))?;
                        Ok(StateArtifactDto {
                            plugin: runtime.plugin.clone(),
                            state_format_version: 1,
                            sha256: digest_bytes(&bytes).to_hex(),
                            byte_len: bytes.len() as u64,
                            relative_path: request.output_relative_path,
                        })
                    });
                match result {
                    Ok(state) => Message::StateSaved { request_id, state },
                    Err(detail) => runtime_error(Some(request_id), Some(token), detail),
                }
            }
            Message::Deactivate { instance } => {
                let token = instance;
                match token
                    .value()
                    .map_err(|error| error.to_string())
                    .and_then(|id| instance_mut(&mut instances, id)?.native.deactivate())
                {
                    Ok(()) => Message::Deactivated { instance: token },
                    Err(detail) => runtime_error(None, Some(token), detail),
                }
            }
            Message::Destroy { instance } => {
                let token = instance;
                match token.value().map_err(|error| error.to_string()) {
                    Ok(id) if instances.remove(&id).is_some() => {
                        Message::Destroyed { instance: token }
                    }
                    Ok(_) => runtime_error(None, Some(token), "unknown CLAP instance".into()),
                    Err(detail) => runtime_error(None, Some(token), detail),
                }
            }
            Message::Shutdown => return,
            _ => Message::Error {
                failure: RuntimeFailureDto {
                    request_id: None,
                    instance: None,
                    kind: RuntimeFailureKindDto::InvalidLifecycle,
                    recoverable: false,
                    detail: "message is unsupported by the CLAP worker lifecycle".into(),
                },
            },
        };
        let outgoing = Envelope::new(worker_sequence, response);
        session
            .observe_worker(&outgoing)
            .unwrap_or_else(|error| fatal(68, &format!("invalid worker transition: {error}")));
        let encoded = outgoing
            .to_jsonl()
            .unwrap_or_else(|error| fatal(69, &format!("could not encode response: {error}")));
        stdout
            .write_all(encoded.as_bytes())
            .and_then(|_| stdout.flush())
            .unwrap_or_else(|error| fatal(70, &format!("could not write response: {error}")));
        worker_sequence += 1;
    }
}

struct RuntimeInstance {
    plugin: plugin_wire::PluginKeyDto,
    native: clap_adapter::NativeClapInstance,
}

fn instance_mut(
    instances: &mut BTreeMap<u128, RuntimeInstance>,
    id: u128,
) -> Result<&mut RuntimeInstance, String> {
    instances
        .get_mut(&id)
        .ok_or_else(|| format!("unknown CLAP instance {id:032x}"))
}

fn runtime_error(
    request_id: Option<u64>,
    instance: Option<plugin_wire::TokenDto>,
    detail: String,
) -> Message {
    Message::Error {
        failure: RuntimeFailureDto {
            request_id,
            instance,
            kind: RuntimeFailureKindDto::Backend,
            recoverable: false,
            detail,
        },
    }
}

fn fatal(code: i32, detail: &str) -> ! {
    eprintln!("audec-clap-worker: {detail}");
    std::process::exit(code);
}
