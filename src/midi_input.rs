//! UI-neutral MIDI 1 input foundation.
//!
//! A native callback validates a small MIDI packet, copies one fixed-size
//! Audec event into a bounded SPSC queue, updates atomic diagnostics, and
//! returns. Clock calibration, note correlation, command construction, and
//! authoritative command execution all happen on the control side.
//!
//! This module intentionally does not schedule instruments or retain a second
//! note graph. A recording is not durable merely because an event entered the
//! queue: [`MidiControlIngress::record_into_commands`] only counts it as
//! recorded after a caller-supplied command authority accepts the lowered
//! command.

use std::error::Error;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use midir::{Ignore, MidiInput, MidiInputConnection};
use rtrb::{Consumer, Producer, RingBuffer};
use wmidi::MidiMessage;

const MICROS_PER_SECOND: i128 = 1_000_000;

/// Process-local identity for a port in one discovery generation.
///
/// This token is deliberately unsuitable for persistence. It only selects an
/// exact port from the catalog currently displayed to a user.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct MidiPortToken {
    generation: u64,
    ordinal: u32,
}

impl MidiPortToken {
    pub const fn generation(self) -> u64 {
        self.generation
    }

    pub const fn ordinal(self) -> u32 {
        self.ordinal
    }
}

/// User-visible facts that may be retained as a relink preference.
///
/// Midir/backend IDs, indexes, and connection handles are never durable
/// identity. A display-name match is intentionally conservative: if two
/// current ports share the name, relinking refuses as ambiguous.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MidiPortMatch {
    pub display_name: String,
}

impl MidiPortMatch {
    pub fn new(display_name: impl Into<String>) -> Result<Self, MidiInputError> {
        let display_name = display_name.into();
        if display_name.trim().is_empty() {
            return Err(MidiInputError::EmptyPortName);
        }
        Ok(Self { display_name })
    }
}

/// One currently discoverable input. Only `relink` is persistence-safe.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MidiInputPortDescriptor {
    pub token: MidiPortToken,
    pub display_name: String,
    pub relink: MidiPortMatch,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MidiPortSelection {
    /// Exact selection from the most recent discovery result.
    Runtime(MidiPortToken),
    /// Conservative relink from user-visible persistent preference.
    Relink(MidiPortMatch),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MidiInputError {
    ZeroQueueCapacity,
    ZeroSampleRate,
    EmptyClientName,
    EmptyPortName,
    TooManyPorts(usize),
    BackendInitialization(String),
    PortInformation {
        ordinal: usize,
        detail: String,
    },
    StaleSelection {
        requested_generation: u64,
        current_generation: u64,
    },
    MissingRuntimePort(MidiPortToken),
    MissingRelinkPort(String),
    AmbiguousRelinkPort {
        display_name: String,
        matches: usize,
    },
    PortDisappeared(String),
    PortChanged {
        expected: String,
        actual: String,
    },
    Connection(String),
}

impl fmt::Display for MidiInputError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroQueueCapacity => formatter.write_str("MIDI ingress capacity must be positive"),
            Self::ZeroSampleRate => formatter.write_str("MIDI clock sample rate must be positive"),
            Self::EmptyClientName => formatter.write_str("MIDI client name must not be empty"),
            Self::EmptyPortName => formatter.write_str("MIDI port name must not be empty"),
            Self::TooManyPorts(count) => write!(formatter, "MIDI backend returned {count} ports; catalog identity supports at most {}", u32::MAX),
            Self::BackendInitialization(detail) => write!(formatter, "could not initialize MIDI input backend: {detail}"),
            Self::PortInformation { ordinal, detail } => write!(formatter, "could not inspect MIDI input port {ordinal}: {detail}"),
            Self::StaleSelection { requested_generation, current_generation } => write!(formatter, "MIDI selection is from discovery generation {requested_generation}, current generation is {current_generation}"),
            Self::MissingRuntimePort(token) => write!(formatter, "MIDI port {} from discovery generation {} is no longer available", token.ordinal, token.generation),
            Self::MissingRelinkPort(name) => write!(formatter, "no MIDI input named {name:?} is available"),
            Self::AmbiguousRelinkPort { display_name, matches } => write!(formatter, "MIDI input name {display_name:?} matches {matches} ports; choose an exact current port"),
            Self::PortDisappeared(name) => write!(formatter, "MIDI input {name:?} disappeared after discovery"),
            Self::PortChanged { expected, actual } => write!(formatter, "MIDI input changed after discovery: expected {expected:?}, found {actual:?}"),
            Self::Connection(detail) => write!(formatter, "could not connect MIDI input: {detail}"),
        }
    }
}

impl Error for MidiInputError {}

/// The MIDI 1 channel events currently admitted to Audec's ingress.
///
/// Channel numbers and data bytes are zero-based/raw MIDI values. Pitch bend
/// is centered at zero and spans `-8192..=8191`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MidiEventKind {
    NoteOn { key: u8, velocity: u8 },
    NoteOff { key: u8, release_velocity: u8 },
    ControlChange { controller: u8, value: u8 },
    PitchBend { value: i16 },
}

/// Fixed-size value copied by the native callback into the bounded queue.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TimestampedMidiEvent {
    /// Backend timestamp in microseconds from an unspecified, connection-stable
    /// origin. It must be calibrated before it becomes project time.
    pub source_timestamp_micros: u64,
    /// Zero-based MIDI channel, `0..=15`.
    pub channel: u8,
    pub kind: MidiEventKind,
}

/// A MIDI observation expressed in signed canonical project frames.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProjectMidiEvent {
    pub project_frame: i64,
    pub channel: u8,
    pub kind: MidiEventKind,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MidiDiagnostics {
    pub callback_packets: u64,
    pub enqueued_events: u64,
    pub invalid_packets: u64,
    pub unsupported_packets: u64,
    /// Queue-full events. This is the MIDI ingress xrun/drop count.
    pub dropped_queue_full: u64,
    pub peak_queue_depth: u64,
    pub source_timestamp_regressions: u64,
    pub frame_mapping_saturations: u64,
    pub command_refusals: u64,
}

#[derive(Default)]
struct MidiDiagnosticCounters {
    callback_packets: AtomicU64,
    enqueued_events: AtomicU64,
    invalid_packets: AtomicU64,
    unsupported_packets: AtomicU64,
    dropped_queue_full: AtomicU64,
    peak_queue_depth: AtomicU64,
    source_timestamp_regressions: AtomicU64,
    frame_mapping_saturations: AtomicU64,
    command_refusals: AtomicU64,
}

impl MidiDiagnosticCounters {
    fn snapshot(&self) -> MidiDiagnostics {
        MidiDiagnostics {
            callback_packets: self.callback_packets.load(Ordering::Relaxed),
            enqueued_events: self.enqueued_events.load(Ordering::Relaxed),
            invalid_packets: self.invalid_packets.load(Ordering::Relaxed),
            unsupported_packets: self.unsupported_packets.load(Ordering::Relaxed),
            dropped_queue_full: self.dropped_queue_full.load(Ordering::Relaxed),
            peak_queue_depth: self.peak_queue_depth.load(Ordering::Relaxed),
            source_timestamp_regressions: self.source_timestamp_regressions.load(Ordering::Relaxed),
            frame_mapping_saturations: self.frame_mapping_saturations.load(Ordering::Relaxed),
            command_refusals: self.command_refusals.load(Ordering::Relaxed),
        }
    }
}

/// Single-producer callback edge. Construction allocates; `push_packet` does
/// not allocate, lock, block, or log.
pub struct MidiIngressProducer {
    queue: Producer<TimestampedMidiEvent>,
    capacity: usize,
    diagnostics: Arc<MidiDiagnosticCounters>,
}

impl MidiIngressProducer {
    pub fn push_packet(&mut self, source_timestamp_micros: u64, packet: &[u8]) {
        self.diagnostics
            .callback_packets
            .fetch_add(1, Ordering::Relaxed);
        let Some(event) = decode_packet(source_timestamp_micros, packet, &self.diagnostics) else {
            return;
        };
        if self.queue.push(event).is_err() {
            self.diagnostics
                .dropped_queue_full
                .fetch_add(1, Ordering::Relaxed);
            return;
        }
        self.diagnostics
            .enqueued_events
            .fetch_add(1, Ordering::Relaxed);
        let depth = self.capacity.saturating_sub(self.queue.slots()) as u64;
        self.diagnostics
            .peak_queue_depth
            .fetch_max(depth, Ordering::Relaxed);
    }

    pub fn diagnostics(&self) -> MidiDiagnostics {
        self.diagnostics.snapshot()
    }
}

fn decode_packet(
    source_timestamp_micros: u64,
    packet: &[u8],
    diagnostics: &MidiDiagnosticCounters,
) -> Option<TimestampedMidiEvent> {
    let message = match MidiMessage::try_from(packet) {
        Ok(message) => message,
        Err(_) => {
            diagnostics.invalid_packets.fetch_add(1, Ordering::Relaxed);
            return None;
        }
    };
    if message.bytes_size() != packet.len() {
        diagnostics.invalid_packets.fetch_add(1, Ordering::Relaxed);
        return None;
    }
    let (channel, kind) = match message {
        MidiMessage::NoteOn(channel, key, velocity) => (
            channel.index(),
            MidiEventKind::NoteOn {
                key: key.into(),
                velocity: velocity.into(),
            },
        ),
        MidiMessage::NoteOff(channel, key, release_velocity) => (
            channel.index(),
            MidiEventKind::NoteOff {
                key: key.into(),
                release_velocity: release_velocity.into(),
            },
        ),
        MidiMessage::ControlChange(channel, controller, value) => (
            channel.index(),
            MidiEventKind::ControlChange {
                controller: controller.into(),
                value: value.into(),
            },
        ),
        MidiMessage::PitchBendChange(channel, value) => {
            let centered = i32::from(u16::from(value)) - 8_192;
            (
                channel.index(),
                MidiEventKind::PitchBend {
                    value: centered as i16,
                },
            )
        }
        _ => {
            diagnostics
                .unsupported_packets
                .fetch_add(1, Ordering::Relaxed);
            return None;
        }
    };
    Some(TimestampedMidiEvent {
        source_timestamp_micros,
        channel,
        kind,
    })
}

/// Paired source/project timestamp captured by the control layer.
///
/// Re-anchor on seek, transport discontinuity, device reconnect, or sample-rate
/// change. `input_latency_frames` is subtracted from every mapped event.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MidiClockCalibration {
    pub source_timestamp_micros: u64,
    pub project_frame: i64,
    pub sample_rate: u32,
    pub input_latency_frames: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MidiClockMapOutcome {
    pub project_frame: i64,
    pub source_timestamp_regressed: bool,
    pub saturated: bool,
}

/// Exact integer mapping from one connection-stable MIDI clock to project
/// frames. It performs no wall-clock lookup and owns no transport state.
pub struct MidiClockMapper {
    calibration: MidiClockCalibration,
    last_source_timestamp_micros: Option<u64>,
}

impl MidiClockMapper {
    pub fn new(calibration: MidiClockCalibration) -> Result<Self, MidiInputError> {
        if calibration.sample_rate == 0 {
            return Err(MidiInputError::ZeroSampleRate);
        }
        Ok(Self {
            calibration,
            last_source_timestamp_micros: None,
        })
    }

    pub fn calibration(&self) -> MidiClockCalibration {
        self.calibration
    }

    pub fn recalibrate(&mut self, calibration: MidiClockCalibration) -> Result<(), MidiInputError> {
        if calibration.sample_rate == 0 {
            return Err(MidiInputError::ZeroSampleRate);
        }
        self.calibration = calibration;
        self.last_source_timestamp_micros = None;
        Ok(())
    }

    pub fn map_timestamp(&mut self, source_timestamp_micros: u64) -> MidiClockMapOutcome {
        let source_timestamp_regressed = self
            .last_source_timestamp_micros
            .is_some_and(|last| source_timestamp_micros < last);
        self.last_source_timestamp_micros = Some(source_timestamp_micros);

        let delta_micros = i128::from(source_timestamp_micros)
            - i128::from(self.calibration.source_timestamp_micros);
        let numerator = delta_micros * i128::from(self.calibration.sample_rate);
        let delta_frames = round_ratio(numerator, MICROS_PER_SECOND);
        let mapped = i128::from(self.calibration.project_frame) + delta_frames
            - i128::from(self.calibration.input_latency_frames);
        let (project_frame, saturated) = saturating_i128_to_i64(mapped);
        MidiClockMapOutcome {
            project_frame,
            source_timestamp_regressed,
            saturated,
        }
    }
}

fn round_ratio(numerator: i128, denominator: i128) -> i128 {
    debug_assert!(denominator > 0);
    if numerator >= 0 {
        (numerator + denominator / 2) / denominator
    } else {
        (numerator - denominator / 2) / denominator
    }
}

fn saturating_i128_to_i64(value: i128) -> (i64, bool) {
    if value > i128::from(i64::MAX) {
        (i64::MAX, true)
    } else if value < i128::from(i64::MIN) {
        (i64::MIN, true)
    } else {
        (value as i64, false)
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MidiCommandPumpReport {
    pub dequeued: usize,
    pub commands_submitted: usize,
    pub events_awaiting_command: usize,
}

#[derive(Debug, Eq, PartialEq)]
pub enum MidiCommandPumpError<LoweringError, AuthorityError> {
    Lowering(LoweringError),
    Authority(AuthorityError),
}

/// Control-side queue owner. It maps clock domains and can pass observations
/// through an explicit lowering boundary into the authoritative command path.
pub struct MidiControlIngress {
    queue: Consumer<TimestampedMidiEvent>,
    diagnostics: Arc<MidiDiagnosticCounters>,
}

impl MidiControlIngress {
    pub fn bounded(capacity: usize) -> Result<(MidiIngressProducer, Self), MidiInputError> {
        if capacity == 0 {
            return Err(MidiInputError::ZeroQueueCapacity);
        }
        let diagnostics = Arc::new(MidiDiagnosticCounters::default());
        let (queue_producer, queue_consumer) = RingBuffer::new(capacity);
        Ok((
            MidiIngressProducer {
                queue: queue_producer,
                capacity,
                diagnostics: Arc::clone(&diagnostics),
            },
            Self {
                queue: queue_consumer,
                diagnostics,
            },
        ))
    }

    pub fn pending(&self) -> usize {
        self.queue.slots()
    }

    pub fn producer_disconnected(&self) -> bool {
        self.queue.is_abandoned()
    }

    pub fn diagnostics(&self) -> MidiDiagnostics {
        self.diagnostics.snapshot()
    }

    /// Build an initial mapping by pairing the next queued source timestamp
    /// with an authoritative audio/transport frame sampled by the controller.
    /// Returns `None` until the device has produced at least one admitted
    /// event. The caller should replace this anchor after every transport or
    /// device-clock discontinuity.
    pub fn calibrate_at_next_event(
        &self,
        project_frame: i64,
        sample_rate: u32,
        input_latency_frames: u32,
    ) -> Result<Option<MidiClockMapper>, MidiInputError> {
        let Ok(next) = self.queue.peek() else {
            return Ok(None);
        };
        MidiClockMapper::new(MidiClockCalibration {
            source_timestamp_micros: next.source_timestamp_micros,
            project_frame,
            sample_rate,
            input_latency_frames,
        })
        .map(Some)
    }

    pub fn pop_mapped(&mut self, clock: &mut MidiClockMapper) -> Option<ProjectMidiEvent> {
        let timestamped = self.queue.pop().ok()?;
        let mapped = clock.map_timestamp(timestamped.source_timestamp_micros);
        if mapped.source_timestamp_regressed {
            self.diagnostics
                .source_timestamp_regressions
                .fetch_add(1, Ordering::Relaxed);
        }
        if mapped.saturated {
            self.diagnostics
                .frame_mapping_saturations
                .fetch_add(1, Ordering::Relaxed);
        }
        Some(ProjectMidiEvent {
            project_frame: mapped.project_frame,
            channel: timestamped.channel,
            kind: timestamped.kind,
        })
    }

    /// Drain at most `event_budget` observations. This method may allocate or
    /// reject on the control thread through the caller-supplied `lower` and
    /// `submit`; neither activity occurs in the native callback.
    ///
    /// `lower` is stateful: a note-on returns `Ok(None)` while it waits for
    /// the matching note-off before producing one put-style command. `submit`
    /// is the project command authority - in the application, the owner of
    /// `CommandEnvelope`/`ProjectController`.
    pub fn record_into_commands<Command, LoweringError, AuthorityError>(
        &mut self,
        clock: &mut MidiClockMapper,
        event_budget: usize,
        mut lower: impl FnMut(ProjectMidiEvent) -> Result<Option<Command>, LoweringError>,
        mut submit: impl FnMut(Command) -> Result<(), AuthorityError>,
    ) -> Result<MidiCommandPumpReport, MidiCommandPumpError<LoweringError, AuthorityError>> {
        let mut report = MidiCommandPumpReport::default();
        while report.dequeued < event_budget {
            let Some(event) = self.pop_mapped(clock) else {
                break;
            };
            report.dequeued += 1;
            let command = match lower(event) {
                Ok(command) => command,
                Err(error) => {
                    self.diagnostics
                        .command_refusals
                        .fetch_add(1, Ordering::Relaxed);
                    return Err(MidiCommandPumpError::Lowering(error));
                }
            };
            let Some(command) = command else {
                report.events_awaiting_command += 1;
                continue;
            };
            if let Err(error) = submit(command) {
                self.diagnostics
                    .command_refusals
                    .fetch_add(1, Ordering::Relaxed);
                return Err(MidiCommandPumpError::Authority(error));
            }
            report.commands_submitted += 1;
        }
        Ok(report)
    }
}

pub trait MidiInputBackend {
    type Connection;
    type Error;

    fn discover(&mut self) -> Result<Vec<MidiInputPortDescriptor>, Self::Error>;
    fn connect(
        &mut self,
        selection: &MidiPortSelection,
        ingress: MidiIngressProducer,
    ) -> Result<Self::Connection, Self::Error>;
}

#[derive(Clone, Debug)]
struct CatalogPort {
    descriptor: MidiInputPortDescriptor,
    backend_id: String,
}

/// Native `midir` adapter. It owns only discovery metadata; an open connection
/// owns the callback producer and native handle.
pub struct MidirInputBackend {
    client_name: String,
    generation: u64,
    catalog: Vec<CatalogPort>,
}

impl MidirInputBackend {
    pub fn new(client_name: impl Into<String>) -> Result<Self, MidiInputError> {
        let client_name = client_name.into();
        if client_name.trim().is_empty() {
            return Err(MidiInputError::EmptyClientName);
        }
        Ok(Self {
            client_name,
            generation: 0,
            catalog: Vec::new(),
        })
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    fn resolve(&self, selection: &MidiPortSelection) -> Result<&CatalogPort, MidiInputError> {
        match selection {
            MidiPortSelection::Runtime(token) => {
                if token.generation != self.generation {
                    return Err(MidiInputError::StaleSelection {
                        requested_generation: token.generation,
                        current_generation: self.generation,
                    });
                }
                self.catalog
                    .get(token.ordinal as usize)
                    .filter(|port| port.descriptor.token == *token)
                    .ok_or(MidiInputError::MissingRuntimePort(*token))
            }
            MidiPortSelection::Relink(preference) => {
                let mut matches = self
                    .catalog
                    .iter()
                    .filter(|port| port.descriptor.display_name == preference.display_name);
                let first = matches.next().ok_or_else(|| {
                    MidiInputError::MissingRelinkPort(preference.display_name.clone())
                })?;
                let additional = matches.count();
                if additional > 0 {
                    return Err(MidiInputError::AmbiguousRelinkPort {
                        display_name: preference.display_name.clone(),
                        matches: additional + 1,
                    });
                }
                Ok(first)
            }
        }
    }
}

struct MidiCallbackState {
    ingress: MidiIngressProducer,
}

pub struct MidirInputHandle {
    descriptor: MidiInputPortDescriptor,
    connection: MidiInputConnection<MidiCallbackState>,
}

impl MidirInputHandle {
    pub fn descriptor(&self) -> &MidiInputPortDescriptor {
        &self.descriptor
    }

    pub fn close(self) -> MidiIngressProducer {
        let (_, state) = self.connection.close();
        state.ingress
    }
}

impl MidiInputBackend for MidirInputBackend {
    type Connection = MidirInputHandle;
    type Error = MidiInputError;

    fn discover(&mut self) -> Result<Vec<MidiInputPortDescriptor>, Self::Error> {
        // A failed refresh must not leave the previous catalog selectable as
        // though it were current. Advance first, then publish only a complete
        // new catalog.
        let next_generation = self.generation.wrapping_add(1).max(1);
        self.generation = next_generation;
        self.catalog.clear();
        let input = MidiInput::new(&self.client_name)
            .map_err(|error| MidiInputError::BackendInitialization(error.to_string()))?;
        let ports = input.ports();
        if ports.len() > u32::MAX as usize {
            return Err(MidiInputError::TooManyPorts(ports.len()));
        }
        let mut catalog = Vec::with_capacity(ports.len());
        for (ordinal, port) in ports.iter().enumerate() {
            let display_name =
                input
                    .port_name(port)
                    .map_err(|error| MidiInputError::PortInformation {
                        ordinal,
                        detail: error.to_string(),
                    })?;
            if display_name.trim().is_empty() {
                return Err(MidiInputError::PortInformation {
                    ordinal,
                    detail: "backend returned an empty port name".into(),
                });
            }
            let descriptor = MidiInputPortDescriptor {
                token: MidiPortToken {
                    generation: next_generation,
                    ordinal: ordinal as u32,
                },
                relink: MidiPortMatch {
                    display_name: display_name.clone(),
                },
                display_name,
            };
            catalog.push(CatalogPort {
                descriptor,
                backend_id: port.id(),
            });
        }
        self.catalog = catalog;
        Ok(self
            .catalog
            .iter()
            .map(|port| port.descriptor.clone())
            .collect())
    }

    fn connect(
        &mut self,
        selection: &MidiPortSelection,
        ingress: MidiIngressProducer,
    ) -> Result<Self::Connection, Self::Error> {
        let selected = self.resolve(selection)?.clone();
        let mut input = MidiInput::new(&self.client_name)
            .map_err(|error| MidiInputError::BackendInitialization(error.to_string()))?;
        input.ignore(Ignore::All);
        let port = input.find_port_by_id(&selected.backend_id).ok_or_else(|| {
            MidiInputError::PortDisappeared(selected.descriptor.display_name.clone())
        })?;
        let actual_name =
            input
                .port_name(&port)
                .map_err(|error| MidiInputError::PortInformation {
                    ordinal: selected.descriptor.token.ordinal as usize,
                    detail: error.to_string(),
                })?;
        if actual_name != selected.descriptor.display_name {
            return Err(MidiInputError::PortChanged {
                expected: selected.descriptor.display_name,
                actual: actual_name,
            });
        }
        let connection_name = format!("{} input", self.client_name);
        let connection = input
            .connect(
                &port,
                &connection_name,
                |timestamp, packet, state| state.ingress.push_packet(timestamp, packet),
                MidiCallbackState { ingress },
            )
            .map_err(|error| MidiInputError::Connection(error.to_string()))?;
        Ok(MidirInputHandle {
            descriptor: selected.descriptor,
            connection,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clock() -> MidiClockMapper {
        MidiClockMapper::new(MidiClockCalibration {
            source_timestamp_micros: 1_000_000,
            project_frame: 48_000,
            sample_rate: 48_000,
            input_latency_frames: 96,
        })
        .unwrap()
    }

    #[test]
    fn callback_decodes_note_cc_and_centered_pitch_without_retaining_bytes() {
        let (mut producer, mut control) = MidiControlIngress::bounded(8).unwrap();
        producer.push_packet(1, &[0x92, 60, 100]);
        producer.push_packet(2, &[0x92, 60, 0]);
        producer.push_packet(3, &[0xB1, 74, 91]);
        producer.push_packet(4, &[0xE0, 0, 64]);

        let mut mapper = MidiClockMapper::new(MidiClockCalibration {
            source_timestamp_micros: 1,
            project_frame: 0,
            sample_rate: 1_000_000,
            input_latency_frames: 0,
        })
        .unwrap();
        let events: Vec<_> = (0..4)
            .map(|_| control.pop_mapped(&mut mapper).unwrap())
            .collect();
        assert_eq!(events[0].channel, 2);
        assert_eq!(
            events[0].kind,
            MidiEventKind::NoteOn {
                key: 60,
                velocity: 100
            }
        );
        assert_eq!(
            events[1].kind,
            MidiEventKind::NoteOff {
                key: 60,
                release_velocity: 0
            }
        );
        assert_eq!(
            events[2].kind,
            MidiEventKind::ControlChange {
                controller: 74,
                value: 91
            }
        );
        assert_eq!(events[3].kind, MidiEventKind::PitchBend { value: 0 });
        assert_eq!(producer.diagnostics().enqueued_events, 4);
    }

    #[test]
    fn invalid_unsupported_and_full_queue_drops_are_distinct() {
        let (mut producer, _control) = MidiControlIngress::bounded(1).unwrap();
        producer.push_packet(1, &[0x90, 64]);
        producer.push_packet(2, &[0xC0, 10]);
        producer.push_packet(3, &[0x90, 64, 100]);
        producer.push_packet(4, &[0x80, 64, 80]);
        let diagnostics = producer.diagnostics();
        assert_eq!(diagnostics.callback_packets, 4);
        assert_eq!(diagnostics.invalid_packets, 1);
        assert_eq!(diagnostics.unsupported_packets, 1);
        assert_eq!(diagnostics.enqueued_events, 1);
        assert_eq!(diagnostics.dropped_queue_full, 1);
        assert_eq!(diagnostics.peak_queue_depth, 1);
    }

    #[test]
    fn packet_with_trailing_message_is_rejected_instead_of_partially_parsed() {
        let (mut producer, _control) = MidiControlIngress::bounded(2).unwrap();
        producer.push_packet(1, &[0x90, 64, 100, 0xF8]);
        assert_eq!(producer.diagnostics().invalid_packets, 1);
        assert_eq!(producer.diagnostics().enqueued_events, 0);
    }

    #[test]
    fn clock_mapping_is_signed_rounded_and_latency_compensated() {
        let mut mapper = clock();
        assert_eq!(mapper.map_timestamp(1_000_000).project_frame, 47_904);
        assert_eq!(mapper.map_timestamp(1_500_000).project_frame, 71_904);
        assert_eq!(mapper.map_timestamp(0).project_frame, -96);
    }

    #[test]
    fn control_side_can_anchor_unspecified_device_clock_at_first_event() {
        let (mut producer, control) = MidiControlIngress::bounded(2).unwrap();
        assert!(control
            .calibrate_at_next_event(500, 48_000, 24)
            .unwrap()
            .is_none());
        producer.push_packet(9_876_543, &[0x90, 60, 100]);
        let mut mapper = control
            .calibrate_at_next_event(500, 48_000, 24)
            .unwrap()
            .unwrap();
        assert_eq!(mapper.map_timestamp(9_876_543).project_frame, 476);
    }

    #[test]
    fn timestamp_regression_is_mapped_but_diagnosed() {
        let (mut producer, mut control) = MidiControlIngress::bounded(2).unwrap();
        producer.push_packet(100, &[0x90, 60, 100]);
        producer.push_packet(99, &[0x80, 60, 0]);
        let mut mapper = MidiClockMapper::new(MidiClockCalibration {
            source_timestamp_micros: 100,
            project_frame: 0,
            sample_rate: 48_000,
            input_latency_frames: 0,
        })
        .unwrap();
        control.pop_mapped(&mut mapper).unwrap();
        control.pop_mapped(&mut mapper).unwrap();
        assert_eq!(control.diagnostics().source_timestamp_regressions, 1);
    }

    #[derive(Default)]
    struct NotePairLowerer {
        active: Option<ProjectMidiEvent>,
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    struct PutRecordedNote {
        start: i64,
        end: i64,
        key: u8,
    }

    impl NotePairLowerer {
        fn lower(
            &mut self,
            event: ProjectMidiEvent,
        ) -> Result<Option<PutRecordedNote>, &'static str> {
            match event.kind {
                MidiEventKind::NoteOn { .. } => {
                    self.active = Some(event);
                    Ok(None)
                }
                MidiEventKind::NoteOff { key, .. } => {
                    let start = self.active.take().ok_or("note-off without note-on")?;
                    Ok(Some(PutRecordedNote {
                        start: start.project_frame,
                        end: event.project_frame,
                        key,
                    }))
                }
                _ => Ok(None),
            }
        }
    }

    #[test]
    fn recording_only_becomes_durable_through_command_authority() {
        let (mut producer, mut control) = MidiControlIngress::bounded(4).unwrap();
        producer.push_packet(1_000_000, &[0x90, 60, 100]);
        producer.push_packet(1_250_000, &[0x80, 60, 64]);
        let mut lowerer = NotePairLowerer::default();
        let mut accepted: Vec<PutRecordedNote> = Vec::new();
        let report = control
            .record_into_commands(
                &mut clock(),
                4,
                |event| lowerer.lower(event),
                |command| {
                    accepted.push(command);
                    Ok::<(), &'static str>(())
                },
            )
            .unwrap();
        assert_eq!(report.dequeued, 2);
        assert_eq!(report.events_awaiting_command, 1);
        assert_eq!(report.commands_submitted, 1);
        assert_eq!(
            accepted,
            vec![PutRecordedNote {
                start: 47_904,
                end: 59_904,
                key: 60
            }]
        );
    }

    #[test]
    fn relink_refuses_duplicate_names() {
        let mut backend = MidirInputBackend::new("Audec test").unwrap();
        backend.generation = 7;
        backend.catalog = vec![
            catalog_port(7, 0, "Keyboard", "one"),
            catalog_port(7, 1, "Keyboard", "two"),
        ];
        let error = backend
            .resolve(&MidiPortSelection::Relink(
                MidiPortMatch::new("Keyboard").unwrap(),
            ))
            .unwrap_err();
        assert_eq!(
            error,
            MidiInputError::AmbiguousRelinkPort {
                display_name: "Keyboard".into(),
                matches: 2
            }
        );
    }

    #[test]
    fn runtime_selection_refuses_stale_discovery_generation() {
        let mut backend = MidirInputBackend::new("Audec test").unwrap();
        backend.generation = 7;
        let error = backend
            .resolve(&MidiPortSelection::Runtime(MidiPortToken {
                generation: 6,
                ordinal: 0,
            }))
            .unwrap_err();
        assert_eq!(
            error,
            MidiInputError::StaleSelection {
                requested_generation: 6,
                current_generation: 7
            }
        );
    }

    fn catalog_port(generation: u64, ordinal: u32, name: &str, id: &str) -> CatalogPort {
        CatalogPort {
            descriptor: MidiInputPortDescriptor {
                token: MidiPortToken {
                    generation,
                    ordinal,
                },
                display_name: name.into(),
                relink: MidiPortMatch {
                    display_name: name.into(),
                },
            },
            backend_id: id.into(),
        }
    }
}
