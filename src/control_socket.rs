//! External control protocol for a running audec desktop instance.
//!
//! The application is driven by the same verbs a musician reaches through the
//! command palette, plus a handful of structured requests that need parameters
//! (open a file, select a range, seek, export). Requests are newline-delimited
//! JSON objects on a Unix domain socket; each produces exactly one JSON reply
//! line, in order.
//!
//! This module is toolkit-free. It owns the listener thread, request parsing,
//! and the mailbox that the GPUI main thread drains; it never touches project
//! state itself. Nothing here can claim that an action succeeded: the reply
//! reports what the host did, and the host reports through the same
//! authorities the palette uses.
//!
//! The server is opt-in through `AUDEC_CONTROL_SOCKET=<path>` and exists so a
//! scripted client (a test harness, an agent, a musician's macro) can exercise
//! the live desktop build instead of trusting headless green.

use std::collections::{BTreeMap, VecDeque};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{sync_channel, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{json, Value};

use crate::export::ExportRange;
use crate::render_plan::{BusTap, RenderScope};
use crate::ui_actions::{ActionParameterValue, ActionParameters};

/// How long the listener thread waits for the main thread to answer one
/// request before replying with a timeout error. The UI thread drains the
/// mailbox on its ordinary tick, so this only trips when the app is wedged.
pub const REPLY_TIMEOUT: Duration = Duration::from_secs(30);

/// One parsed request. Verb names mirror the palette's dotted action ids.
#[derive(Clone, Debug, PartialEq)]
pub enum ControlRequest {
    Ping,
    /// Transport, selection, loop, project, and revision facts.
    Status,
    /// Every registered action id with its projected enabled state.
    Actions,
    /// Invoke a registered palette action by id, with the parameters that id
    /// declares. `parameters` is a JSON object of name to bool, integer, or
    /// string; a name the action does not declare is refused by the host,
    /// naming the ones it does take.
    Action {
        id: String,
        parameters: ActionParameters,
    },
    /// Load material or a project package from an absolute path.
    Open {
        path: PathBuf,
    },
    Seek(SeekTarget),
    /// `None` clears the time selection.
    Select(Option<SampleSpan>),
    /// A pointer press and release at one sample on the overview timeline:
    /// the same kernel path as a mouse click (locate, clear selection).
    Click {
        sample: u64,
    },
    /// A pointer press at `start` dragged to `end` on the overview timeline,
    /// optionally with the loop-authoring modifier held.
    Drag {
        start: u64,
        end: u64,
        alt: bool,
    },
    Loop(LoopRequest),
    Play,
    Pause,
    Stop,
    /// Bounce a scope to a WAV at an absolute path. Everything the Export
    /// dialog offers can be named here; anything left out keeps the dialog's
    /// default (whole project, master, 24-bit, seeded dither, unity gain).
    Export {
        path: PathBuf,
        options: ExportOverrides,
    },
    /// Act on one published analysis Finding: the same Keep / Apply / Compare
    /// / Make sample / Hear the reverse pane's RESULT ACTIONS offer, reached
    /// by the index or the address `status.findings` reports. Without this the
    /// reverse flow is pane-bound and the Compare branch cannot be filled by
    /// a script.
    Finding {
        target: FindingTarget,
        action: FindingAction,
    },
    /// Set the tempo of the segment the playhead is standing in, exactly as
    /// typing a number into the toolbar's BPM field does. It plans the same
    /// `TempoPointIntent` the ± buttons plan, at the same position.
    Tempo {
        bpm: f64,
    },
    /// The Explorer's typed object tree for the current project.
    Objects,
    /// Load a portable reading from an absolute path, the same way the
    /// pane's LOAD READING button does without a file dialog. With
    /// `manifest_digest` the bytes are verified against an identity the
    /// caller did not compute.
    ReadingImport {
        path: PathBuf,
        manifest_digest: Option<String>,
    },
    /// Write this project's own hypotheses out as a portable reading.
    ReadingExport {
        path: PathBuf,
    },
    /// Save the project — and the workspace document with it — as a package
    /// at an absolute path. This is Save As without the file dialog, so a
    /// scripted session can prove what survives a reopen.
    Save {
        path: PathBuf,
    },
    /// Drive one analysis lens's control by name (the same handlers its
    /// header buttons call): `spectral-transform`, `fft-size-up`,
    /// `fft-size-down`, `fft-window`, `db-range-up`, `db-range-down`,
    /// `refresh`. `view` is the workspace view id from `status.lenses`.
    Lens {
        view: u64,
        control: String,
    },
    Quit,
}

/// Export settings a client named. Every field is optional; `None` means the
/// host keeps [`crate::export::ExportOptions::default`] for that setting. The
/// scope is parsed here but only the host can say whether this project has it,
/// so an unknown bus or track id is answered by the host, naming the ids it
/// does have.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ExportOverrides {
    pub bits: Option<u16>,
    pub dither: Option<bool>,
    pub gain_db: Option<f64>,
    pub range: Option<ExportRange>,
    pub scope: Option<RenderScope>,
    /// Seconds of tail after the range. The host splits it into what the
    /// compiled render can still sound and what can only be silence, and says
    /// which in the status line.
    pub tail_seconds: Option<f64>,
    /// Whether the monitor click belongs in the file. Absent means no, which
    /// is what a bounce means.
    pub metronome: Option<bool>,
}

/// Which published Finding a `finding` request means. The index is the
/// position in `status.findings`, which is address order; the address is the
/// stable identity that same list reports, and survives a republished list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FindingTarget {
    Index(usize),
    Address(String),
}

/// What to do with it. `Audition` names one of the signal kinds the Finding
/// itself offers; the host refuses an unoffered name by listing the offered
/// ones rather than staying silent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FindingAction {
    Open,
    Keep,
    Compare,
    Apply,
    Sample,
    Audition(String),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SeekTarget {
    Sample(u64),
    Seconds(f64),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SampleSpan {
    pub start: u64,
    pub end: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoopRequest {
    Clear,
    Replace { span: SampleSpan, enabled: bool },
}

#[derive(Deserialize)]
struct RawRequest {
    op: String,
    id: Option<String>,
    path: Option<String>,
    sample: Option<u64>,
    seconds: Option<f64>,
    start: Option<u64>,
    end: Option<u64>,
    enabled: Option<bool>,
    clear: Option<bool>,
    alt: Option<bool>,
    view: Option<u64>,
    control: Option<String>,
    index: Option<usize>,
    address: Option<String>,
    #[serde(rename = "do")]
    action: Option<String>,
    parameters: Option<BTreeMap<String, Value>>,
    manifest_digest: Option<String>,
    bits: Option<u16>,
    dither: Option<bool>,
    gain_db: Option<f64>,
    tail_seconds: Option<f64>,
    metronome: Option<bool>,
    bpm: Option<f64>,
    /// `"project" | "loop" | "selection" | [start_sample, end_sample]`.
    range: Option<Value>,
    /// `"master" | "bus:<id>" | "track:<id>"`.
    scope: Option<String>,
}

/// Parse one request line. Errors are returned to the client verbatim.
pub fn parse_request(line: &str) -> Result<ControlRequest, String> {
    let raw: RawRequest =
        serde_json::from_str(line).map_err(|error| format!("malformed request: {error}"))?;
    let span = |raw: &RawRequest| -> Result<SampleSpan, String> {
        match (raw.start, raw.end) {
            (Some(start), Some(end)) if start < end => Ok(SampleSpan { start, end }),
            (Some(_), Some(_)) => Err("start must be less than end".to_string()),
            _ => Err("start and end are required".to_string()),
        }
    };
    let path = |raw: &RawRequest| -> Result<PathBuf, String> {
        let path = raw.path.as_deref().ok_or("path is required")?;
        let path = PathBuf::from(path);
        if !path.is_absolute() {
            return Err("path must be absolute".to_string());
        }
        Ok(path)
    };
    Ok(match raw.op.as_str() {
        "ping" => ControlRequest::Ping,
        "status" => ControlRequest::Status,
        "actions" => ControlRequest::Actions,
        "action" => ControlRequest::Action {
            id: raw.id.clone().ok_or("id is required")?,
            parameters: action_parameters(&raw)?,
        },
        "open" => ControlRequest::Open { path: path(&raw)? },
        "seek" => ControlRequest::Seek(match (raw.sample, raw.seconds) {
            (Some(sample), _) => SeekTarget::Sample(sample),
            (None, Some(seconds)) if seconds.is_finite() && seconds >= 0.0 => {
                SeekTarget::Seconds(seconds)
            }
            (None, Some(_)) => return Err("seconds must be finite and non-negative".to_string()),
            (None, None) => return Err("sample or seconds is required".to_string()),
        }),
        "click" => ControlRequest::Click {
            sample: raw.sample.ok_or("sample is required")?,
        },
        "drag" => match (raw.start, raw.end) {
            (Some(start), Some(end)) if start != end => ControlRequest::Drag {
                start,
                end,
                alt: raw.alt.unwrap_or(false),
            },
            (Some(_), Some(_)) => return Err("drag start and end must differ".to_string()),
            _ => return Err("start and end are required".to_string()),
        },
        "select" => {
            if raw.start.is_none() && raw.end.is_none() {
                ControlRequest::Select(None)
            } else {
                ControlRequest::Select(Some(span(&raw)?))
            }
        }
        "loop" => ControlRequest::Loop(if raw.clear == Some(true) {
            LoopRequest::Clear
        } else {
            LoopRequest::Replace {
                span: span(&raw)?,
                enabled: raw.enabled.unwrap_or(true),
            }
        }),
        "play" => ControlRequest::Play,
        "pause" => ControlRequest::Pause,
        "stop" => ControlRequest::Stop,
        "export" => ControlRequest::Export {
            path: path(&raw)?,
            options: export_overrides(&raw)?,
        },
        "finding" => ControlRequest::Finding {
            target: match (raw.index, raw.address.as_deref()) {
                (Some(index), None) => FindingTarget::Index(index),
                (None, Some(address)) if !address.trim().is_empty() => {
                    FindingTarget::Address(address.to_string())
                }
                (None, Some(_)) => return Err("address must not be empty".to_string()),
                (Some(_), Some(_)) => {
                    return Err("name a finding by index or by address, not both".to_string())
                }
                (None, None) => return Err("index or address is required".to_string()),
            },
            action: match raw.action.as_deref() {
                None => return Err("`do` is required".to_string()),
                Some("open") => FindingAction::Open,
                Some("keep") => FindingAction::Keep,
                Some("compare") => FindingAction::Compare,
                Some("apply") => FindingAction::Apply,
                Some("sample") => FindingAction::Sample,
                Some(other) => match other.split_once(':') {
                    Some(("audition", kind)) if !kind.is_empty() => {
                        FindingAction::Audition(kind.to_string())
                    }
                    _ => {
                        return Err(format!(
                            "`do` must be open, keep, compare, apply, sample, or audition:<kind>; got `{other}`"
                        ))
                    }
                },
            },
        },
        "tempo" => ControlRequest::Tempo {
            bpm: match raw.bpm {
                Some(bpm) if bpm.is_finite() && bpm > 0.0 => bpm,
                Some(bpm) => return Err(format!("bpm must be finite and positive; got {bpm}")),
                None => return Err("bpm is required".to_string()),
            },
        },
        "objects" => ControlRequest::Objects,
        "reading_import" => ControlRequest::ReadingImport {
            path: path(&raw)?,
            manifest_digest: raw.manifest_digest.clone(),
        },
        "reading_export" => ControlRequest::ReadingExport { path: path(&raw)? },
        "save" => ControlRequest::Save { path: path(&raw)? },
        "lens" => ControlRequest::Lens {
            view: raw.view.ok_or("view is required")?,
            control: raw.control.clone().ok_or("control is required")?,
        },
        "quit" => ControlRequest::Quit,
        other => return Err(format!("unknown op `{other}`")),
    })
}

/// Read the parameters a client named beside an action id. Values avoid
/// floating point for the same reason [`ActionParameters`] does: a parameter
/// that round-trips through a journal or a test fixture must not carry NaN or
/// locale semantics. Which names an action accepts is the registry's business,
/// not this parser's, so an unknown name reaches the host and is refused there
/// with the names that action does take.
fn action_parameters(raw: &RawRequest) -> Result<ActionParameters, String> {
    let mut parameters = ActionParameters::new();
    for (name, value) in raw.parameters.iter().flatten() {
        let value = match value {
            Value::Bool(value) => ActionParameterValue::Bool(*value),
            Value::String(value) => ActionParameterValue::Text(value.clone()),
            Value::Number(number) => match (number.as_u64(), number.as_i64()) {
                (Some(value), _) => ActionParameterValue::Unsigned(value),
                (None, Some(value)) => ActionParameterValue::Signed(value),
                (None, None) => {
                    return Err(format!(
                        "parameter `{name}` must be a whole number; got {number}"
                    ))
                }
            },
            other => {
                return Err(format!(
                    "parameter `{name}` must be a bool, whole number, or string; got {other}"
                ))
            }
        };
        parameters.insert(name.clone(), value);
    }
    Ok(parameters)
}

/// Read the optional export settings off one request. Everything absent is
/// left to the host's defaults; everything present is checked here so a typo
/// is refused before an export starts.
fn export_overrides(raw: &RawRequest) -> Result<ExportOverrides, String> {
    if let Some(bits) = raw.bits {
        if crate::export::sample_format_for_bits(bits).is_none() {
            return Err(format!("bits must be 16, 24, or 32; got {bits}"));
        }
    }
    if let Some(gain_db) = raw.gain_db {
        if !gain_db.is_finite() {
            return Err("gain_db must be finite".to_string());
        }
    }
    if let Some(tail_seconds) = raw.tail_seconds {
        if !tail_seconds.is_finite()
            || tail_seconds < 0.0
            || tail_seconds > crate::export::MAXIMUM_TAIL_SECONDS
        {
            return Err(format!(
                "tail_seconds must be between 0 and {}; got {tail_seconds}",
                crate::export::MAXIMUM_TAIL_SECONDS
            ));
        }
    }
    let range = match raw.range.as_ref() {
        None => None,
        Some(Value::String(word)) => Some(match word.as_str() {
            "project" => ExportRange::Project,
            "loop" => ExportRange::Loop,
            "selection" => ExportRange::Selection,
            other => {
                return Err(format!(
                    "range must be \"project\", \"loop\", \"selection\", or [start, end]; got `{other}`"
                ))
            }
        }),
        Some(Value::Array(bounds)) => {
            let bounds = bounds
                .iter()
                .map(|bound| bound.as_u64())
                .collect::<Option<Vec<_>>>()
                .ok_or("range bounds must be sample numbers")?;
            match bounds.as_slice() {
                [start, end] if start < end => Some(ExportRange::Custom {
                    start: *start,
                    end: *end,
                }),
                [_, _] => return Err("range start must be less than range end".to_string()),
                _ => return Err("a range array must be [start, end]".to_string()),
            }
        }
        Some(_) => {
            return Err(
                "range must be \"project\", \"loop\", \"selection\", or [start, end]".to_string(),
            )
        }
    };
    let scope = match raw.scope.as_deref() {
        None => None,
        Some("master") => Some(RenderScope::Master),
        Some(other) => Some(match other.split_once(':') {
            // The post-fader output is the only tap a stem export means.
            Some(("bus", id)) => RenderScope::Bus {
                bus: id
                    .parse()
                    .map_err(|_| format!("bus id must be a number; got `{id}`"))?,
                tap: BusTap::Output,
            },
            Some(("track", id)) => RenderScope::Track(
                id.parse()
                    .map_err(|_| format!("track id must be a number; got `{id}`"))?,
            ),
            _ => {
                return Err(format!(
                    "scope must be \"master\", \"bus:<id>\", or \"track:<id>\"; got `{other}`"
                ))
            }
        }),
    };
    Ok(ExportOverrides {
        bits: raw.bits,
        dither: raw.dither,
        gain_db: raw.gain_db,
        range,
        scope,
        tail_seconds: raw.tail_seconds,
        metronome: raw.metronome,
    })
}

/// Encode a successful reply.
pub fn ok_reply(result: Value) -> String {
    json!({ "ok": true, "result": result }).to_string()
}

/// Encode a failed reply.
pub fn error_reply(message: impl AsRef<str>) -> String {
    json!({ "ok": false, "error": message.as_ref() }).to_string()
}

/// A request waiting for the main thread, with the channel its reply goes to.
pub struct PendingControl {
    pub request: ControlRequest,
    reply: SyncSender<String>,
}

impl PendingControl {
    /// Deliver the reply line. A client that already hung up is not an error.
    pub fn reply(self, line: String) {
        let _ = self.reply.send(line);
    }
}

/// Main-thread side of the socket: drained on the host's ordinary tick.
#[derive(Clone, Default)]
pub struct ControlMailbox {
    queue: Arc<Mutex<VecDeque<PendingControl>>>,
}

impl ControlMailbox {
    pub fn drain(&self) -> Vec<PendingControl> {
        let mut queue = self
            .queue
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        queue.drain(..).collect()
    }

    fn push(&self, pending: PendingControl) {
        let mut queue = self
            .queue
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        queue.push_back(pending);
    }
}

/// Bind the socket and start accepting clients on a background thread.
///
/// A stale socket file at `path` is removed first; the caller owns the choice
/// of path (normally `AUDEC_CONTROL_SOCKET`).
pub fn serve(path: &Path) -> std::io::Result<ControlMailbox> {
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    let listener = UnixListener::bind(path)?;
    let mailbox = ControlMailbox::default();
    let worker = mailbox.clone();
    thread::Builder::new()
        .name("audec-control-socket".to_string())
        .spawn(move || {
            for stream in listener.incoming() {
                match stream {
                    Ok(stream) => serve_client(stream, &worker),
                    Err(error) => {
                        eprintln!("audec control socket accept failed: {error}");
                        break;
                    }
                }
            }
        })?;
    Ok(mailbox)
}

fn serve_client(stream: UnixStream, mailbox: &ControlMailbox) {
    let Ok(mut writer) = stream.try_clone() else {
        return;
    };
    let reader = BufReader::new(stream);
    for line in reader.lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let reply_line = match parse_request(&line) {
            Err(message) => error_reply(message),
            Ok(request) => {
                let (sender, receiver) = sync_channel(1);
                mailbox.push(PendingControl {
                    request,
                    reply: sender,
                });
                match receiver.recv_timeout(REPLY_TIMEOUT) {
                    Ok(reply) => reply,
                    Err(RecvTimeoutError::Timeout) => {
                        error_reply("the application did not answer within the reply timeout")
                    }
                    Err(RecvTimeoutError::Disconnected) => {
                        error_reply("the application dropped the request")
                    }
                }
            }
        };
        if writeln!(writer, "{reply_line}")
            .and_then(|_| writer.flush())
            .is_err()
        {
            break;
        }
    }
}

/// Path from the environment, if the operator asked for a control socket.
pub fn socket_path_from_env() -> Option<PathBuf> {
    std::env::var_os("AUDEC_CONTROL_SOCKET")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    #[test]
    fn parses_every_verb_with_its_parameters() {
        assert_eq!(parse_request(r#"{"op":"ping"}"#), Ok(ControlRequest::Ping));
        assert_eq!(
            parse_request(r#"{"op":"status"}"#),
            Ok(ControlRequest::Status)
        );
        assert_eq!(
            parse_request(r#"{"op":"action","id":"audec.transport.toggle"}"#),
            Ok(ControlRequest::Action {
                id: "audec.transport.toggle".to_string(),
                parameters: ActionParameters::default(),
            })
        );
        assert_eq!(
            parse_request(r#"{"op":"open","path":"/tmp/x.flac"}"#),
            Ok(ControlRequest::Open {
                path: PathBuf::from("/tmp/x.flac")
            })
        );
        assert_eq!(
            parse_request(r#"{"op":"seek","sample":441000}"#),
            Ok(ControlRequest::Seek(SeekTarget::Sample(441000)))
        );
        assert_eq!(
            parse_request(r#"{"op":"seek","seconds":1.5}"#),
            Ok(ControlRequest::Seek(SeekTarget::Seconds(1.5)))
        );
        assert_eq!(
            parse_request(r#"{"op":"select","start":10,"end":20}"#),
            Ok(ControlRequest::Select(Some(SampleSpan {
                start: 10,
                end: 20
            })))
        );
        assert_eq!(
            parse_request(r#"{"op":"select"}"#),
            Ok(ControlRequest::Select(None))
        );
        assert_eq!(
            parse_request(r#"{"op":"click","sample":7}"#),
            Ok(ControlRequest::Click { sample: 7 })
        );
        assert_eq!(
            parse_request(r#"{"op":"drag","start":30,"end":10,"alt":true}"#),
            Ok(ControlRequest::Drag {
                start: 30,
                end: 10,
                alt: true
            })
        );
        assert_eq!(
            parse_request(r#"{"op":"loop","start":10,"end":20}"#),
            Ok(ControlRequest::Loop(LoopRequest::Replace {
                span: SampleSpan { start: 10, end: 20 },
                enabled: true
            }))
        );
        assert_eq!(
            parse_request(r#"{"op":"loop","clear":true}"#),
            Ok(ControlRequest::Loop(LoopRequest::Clear))
        );
        assert_eq!(
            parse_request(r#"{"op":"export","path":"/tmp/out.wav"}"#),
            Ok(ControlRequest::Export {
                path: PathBuf::from("/tmp/out.wav"),
                options: ExportOverrides::default(),
            })
        );
        assert_eq!(
            parse_request(r#"{"op":"objects"}"#),
            Ok(ControlRequest::Objects)
        );
        assert_eq!(
            parse_request(r#"{"op":"lens","view":2,"control":"spectral-transform"}"#),
            Ok(ControlRequest::Lens {
                view: 2,
                control: "spectral-transform".to_string()
            })
        );
        assert_eq!(parse_request(r#"{"op":"quit"}"#), Ok(ControlRequest::Quit));
    }

    #[test]
    fn an_action_carries_the_parameters_its_id_declares() {
        let mut expected = ActionParameters::new();
        expected.insert("view", ActionParameterValue::Unsigned(3));
        assert_eq!(
            parse_request(r#"{"op":"action","id":"audec.workspace.activate","parameters":{"view":3}}"#),
            Ok(ControlRequest::Action {
                id: "audec.workspace.activate".to_string(),
                parameters: expected,
            })
        );
        let mut mixed = ActionParameters::new();
        mixed.insert("enabled", ActionParameterValue::Bool(true));
        mixed.insert("offset", ActionParameterValue::Signed(-12));
        mixed.insert("name", ActionParameterValue::Text("verse".into()));
        assert_eq!(
            parse_request(
                r#"{"op":"action","id":"audec.x.y","parameters":{"enabled":true,"offset":-12,"name":"verse"}}"#
            ),
            Ok(ControlRequest::Action {
                id: "audec.x.y".to_string(),
                parameters: mixed,
            })
        );
        // Floating point never enters the parameter vocabulary, so a value
        // that is not a whole number is refused where it was written.
        assert_eq!(
            parse_request(r#"{"op":"action","id":"audec.x.y","parameters":{"bpm":128.5}}"#),
            Err("parameter `bpm` must be a whole number; got 128.5".to_string())
        );
        assert_eq!(
            parse_request(r#"{"op":"action","id":"audec.x.y","parameters":{"who":["a"]}}"#),
            Err(
                "parameter `who` must be a bool, whole number, or string; got [\"a\"]"
                    .to_string()
            )
        );
    }

    #[test]
    fn the_finding_verb_names_one_published_result_and_one_verb() {
        assert_eq!(
            parse_request(r#"{"op":"finding","index":0,"do":"keep"}"#),
            Ok(ControlRequest::Finding {
                target: FindingTarget::Index(0),
                action: FindingAction::Keep,
            })
        );
        assert_eq!(
            parse_request(r#"{"op":"finding","address":"finding:rhythm:7","do":"compare"}"#),
            Ok(ControlRequest::Finding {
                target: FindingTarget::Address("finding:rhythm:7".to_string()),
                action: FindingAction::Compare,
            })
        );
        for (word, action) in [
            ("open", FindingAction::Open),
            ("apply", FindingAction::Apply),
            ("sample", FindingAction::Sample),
        ] {
            assert_eq!(
                parse_request(&format!(r#"{{"op":"finding","index":1,"do":"{word}"}}"#)),
                Ok(ControlRequest::Finding {
                    target: FindingTarget::Index(1),
                    action,
                })
            );
        }
        assert_eq!(
            parse_request(r#"{"op":"finding","index":2,"do":"audition:HpssHarmonic"}"#),
            Ok(ControlRequest::Finding {
                target: FindingTarget::Index(2),
                action: FindingAction::Audition("HpssHarmonic".to_string()),
            })
        );
        assert_eq!(
            parse_request(r#"{"op":"finding","do":"keep"}"#),
            Err("index or address is required".to_string())
        );
        assert_eq!(
            parse_request(r#"{"op":"finding","index":0,"address":"x","do":"keep"}"#),
            Err("name a finding by index or by address, not both".to_string())
        );
        assert_eq!(
            parse_request(r#"{"op":"finding","index":0}"#),
            Err("`do` is required".to_string())
        );
        assert_eq!(
            parse_request(r#"{"op":"finding","index":0,"do":"delete"}"#),
            Err(
                "`do` must be open, keep, compare, apply, sample, or audition:<kind>; got `delete`"
                    .to_string()
            )
        );
        assert_eq!(
            parse_request(r#"{"op":"finding","index":0,"do":"audition:"}"#),
            Err(
                "`do` must be open, keep, compare, apply, sample, or audition:<kind>; got `audition:`"
                    .to_string()
            )
        );
    }

    #[test]
    fn the_export_verb_carries_every_dialog_setting() {
        assert_eq!(
            parse_request(
                r#"{"op":"export","path":"/tmp/loop16.wav","bits":16,"dither":false,"gain_db":-3.0,"range":"loop","scope":"bus:3","tail_seconds":2.0}"#
            ),
            Ok(ControlRequest::Export {
                path: PathBuf::from("/tmp/loop16.wav"),
                options: ExportOverrides {
                    bits: Some(16),
                    dither: Some(false),
                    gain_db: Some(-3.0),
                    range: Some(ExportRange::Loop),
                    scope: Some(RenderScope::Bus {
                        bus: 3,
                        tap: BusTap::Output
                    }),
                    tail_seconds: Some(2.0),
                    metronome: None,
                },
            })
        );
        assert_eq!(
            parse_request(r#"{"op":"export","path":"/tmp/x.wav","tail_seconds":-1.0}"#),
            Err("tail_seconds must be between 0 and 60; got -1".to_string())
        );
        assert_eq!(
            parse_request(r#"{"op":"tempo","bpm":140.0}"#),
            Ok(ControlRequest::Tempo { bpm: 140.0 })
        );
        assert_eq!(
            parse_request(r#"{"op":"tempo","bpm":0}"#),
            Err("bpm must be finite and positive; got 0".to_string())
        );
        assert_eq!(
            parse_request(r#"{"op":"tempo"}"#),
            Err("bpm is required".to_string())
        );
        assert_eq!(
            parse_request(r#"{"op":"export","path":"/tmp/a.wav","range":[100,200]}"#),
            Ok(ControlRequest::Export {
                path: PathBuf::from("/tmp/a.wav"),
                options: ExportOverrides {
                    range: Some(ExportRange::Custom {
                        start: 100,
                        end: 200
                    }),
                    ..ExportOverrides::default()
                },
            })
        );
        assert_eq!(
            parse_request(r#"{"op":"export","path":"/tmp/a.wav","scope":"track:7"}"#),
            Ok(ControlRequest::Export {
                path: PathBuf::from("/tmp/a.wav"),
                options: ExportOverrides {
                    scope: Some(RenderScope::Track(7)),
                    ..ExportOverrides::default()
                },
            })
        );
        assert_eq!(
            parse_request(r#"{"op":"export","path":"/tmp/a.wav","bits":20}"#),
            Err("bits must be 16, 24, or 32; got 20".to_string())
        );
        assert_eq!(
            parse_request(r#"{"op":"export","path":"/tmp/a.wav","range":[200,100]}"#),
            Err("range start must be less than range end".to_string())
        );
        assert_eq!(
            parse_request(r#"{"op":"export","path":"/tmp/a.wav","range":"bar"}"#),
            Err(
                "range must be \"project\", \"loop\", \"selection\", or [start, end]; got `bar`"
                    .to_string()
            )
        );
        assert_eq!(
            parse_request(r#"{"op":"export","path":"/tmp/a.wav","scope":"send:2"}"#),
            Err(
                "scope must be \"master\", \"bus:<id>\", or \"track:<id>\"; got `send:2`"
                    .to_string()
            )
        );
    }

    #[test]
    fn rejects_malformed_and_unsafe_requests_with_reasons() {
        assert!(parse_request("nope")
            .unwrap_err()
            .starts_with("malformed request"));
        assert_eq!(
            parse_request(r#"{"op":"dance"}"#),
            Err("unknown op `dance`".to_string())
        );
        assert_eq!(
            parse_request(r#"{"op":"action"}"#),
            Err("id is required".to_string())
        );
        assert_eq!(
            parse_request(r#"{"op":"open","path":"relative.flac"}"#),
            Err("path must be absolute".to_string())
        );
        assert_eq!(
            parse_request(r#"{"op":"select","start":20,"end":10}"#),
            Err("start must be less than end".to_string())
        );
        assert_eq!(
            parse_request(r#"{"op":"seek","seconds":-1.0}"#),
            Err("seconds must be finite and non-negative".to_string())
        );
    }

    #[test]
    fn replies_are_single_json_lines() {
        let ok: Value = serde_json::from_str(&ok_reply(json!("pong"))).unwrap();
        assert_eq!(ok, json!({ "ok": true, "result": "pong" }));
        let error: Value = serde_json::from_str(&error_reply("bad")).unwrap();
        assert_eq!(error, json!({ "ok": false, "error": "bad" }));
        assert!(!ok_reply(json!("a\nb")).contains('\n'));
    }

    #[test]
    fn socket_round_trips_a_request_through_the_mailbox() {
        let dir = std::env::temp_dir().join(format!("audec-control-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("control.sock");
        let mailbox = serve(&path).unwrap();

        let mut client = UnixStream::connect(&path).unwrap();
        writeln!(client, r#"{{"op":"ping"}}"#).unwrap();

        // Play the main thread: wait for the request, answer it.
        let pending = loop {
            let mut drained = mailbox.drain();
            if let Some(pending) = drained.pop() {
                break pending;
            }
            thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(pending.request, ControlRequest::Ping);
        pending.reply(ok_reply(json!("pong")));

        let mut reply = String::new();
        BufReader::new(&client).read_line(&mut reply).unwrap();
        assert_eq!(reply.trim(), r#"{"ok":true,"result":"pong"}"#);

        // A malformed line is answered by the listener without touching the mailbox.
        writeln!(client, "garbage").unwrap();
        let mut reply = String::new();
        BufReader::new(&client).read_line(&mut reply).unwrap();
        assert!(reply.contains("malformed request"));
        assert!(mailbox.drain().is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }
}
