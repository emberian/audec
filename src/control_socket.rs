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

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{sync_channel, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{json, Value};

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
    /// Invoke a registered palette action by id.
    Action {
        id: String,
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
    /// Bounce the project master to a WAV at an absolute path.
    Export {
        path: PathBuf,
    },
    /// The Explorer's typed object tree for the current project.
    Objects,
    Quit,
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
        "export" => ControlRequest::Export { path: path(&raw)? },
        "objects" => ControlRequest::Objects,
        "quit" => ControlRequest::Quit,
        other => return Err(format!("unknown op `{other}`")),
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
                id: "audec.transport.toggle".to_string()
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
                path: PathBuf::from("/tmp/out.wav")
            })
        );
        assert_eq!(
            parse_request(r#"{"op":"objects"}"#),
            Ok(ControlRequest::Objects)
        );
        assert_eq!(parse_request(r#"{"op":"quit"}"#), Ok(ControlRequest::Quit));
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
