//! Bounded cross-process DSP buffers for plugin workers.
//!
//! The controller owns four named OS mappings. A `Process` JSONL request is
//! the commit/fence for one slot: the controller finishes writing input PCM
//! and events before sending it, and does not read output PCM/events until the
//! matching `Processed` reply. There is intentionally no second scheduler or
//! lock-free queue here. `shared_memory` supplies the maintained macOS/Linux
//! mapping implementation; the only unsafe code below is its required slice
//! view, contained behind this single-owner protocol.

use std::error::Error;
use std::fmt;

use shared_memory::{Shmem, ShmemConf};

use crate::plugin::{
    NormalizedValue, ParameterEvent, PluginNoteAddress, PluginNoteEvent, PluginNoteEventKind,
    PluginNoteExpression, PluginParameterKey, PortDirection, ProcessingContract,
};
use crate::plugin_wire::{
    SharedMemoryAccessDto, SharedMemoryBindingDto, SharedMemoryRegionDto, TokenDto,
};

const EVENT_MAGIC: &[u8; 8] = b"AUDECEVT";
const EVENT_VERSION: u16 = 1;
const EVENT_HEADER_BYTES: usize = 16;
const EVENT_RECORD_BYTES: usize = 40;
pub const DEFAULT_MAX_EVENTS: u32 = 16_384;

#[derive(Clone, Debug, PartialEq)]
pub enum InputEvent {
    Parameter(ParameterEvent),
    Note(PluginNoteEvent),
}

#[derive(Debug)]
pub enum TransportError {
    InvalidContract(String),
    InvalidBinding(String),
    Mapping(String),
    Bounds(String),
    CorruptEvents(String),
}

impl fmt::Display for TransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidContract(detail) => write!(f, "invalid processing contract: {detail}"),
            Self::InvalidBinding(detail) => write!(f, "invalid shared-memory binding: {detail}"),
            Self::Mapping(detail) => write!(f, "shared-memory mapping failed: {detail}"),
            Self::Bounds(detail) => write!(f, "shared-memory bounds violation: {detail}"),
            Self::CorruptEvents(detail) => write!(f, "corrupt shared event slot: {detail}"),
        }
    }
}

impl Error for TransportError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransportSide {
    Controller,
    Worker,
}

/// One bounded process slot. It is safe to move, but callers must keep all
/// access on the worker/controller protocol thread for the lifetime of a
/// process request.
pub struct SharedBlockTransport {
    side: TransportSide,
    maximum_frames: usize,
    input_channels: usize,
    output_channels: usize,
    maximum_events: usize,
    audio_inputs: Shmem,
    audio_outputs: Shmem,
    events_to_worker: Shmem,
    events_from_worker: Shmem,
}

impl fmt::Debug for SharedBlockTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SharedBlockTransport")
            .field("side", &self.side)
            .field("maximum_frames", &self.maximum_frames)
            .field("input_channels", &self.input_channels)
            .field("output_channels", &self.output_channels)
            .field("maximum_events", &self.maximum_events)
            .finish_non_exhaustive()
    }
}

impl SharedBlockTransport {
    /// Create controller-owned mappings from caller-minted non-zero tokens.
    /// Token identity maps directly to a POSIX shared-memory name on macOS and
    /// Linux, so no inheritable raw descriptor or pathname is needed.
    pub fn create(
        contract: &ProcessingContract,
        binding: SharedMemoryBindingDto,
        maximum_events: u32,
    ) -> Result<Self, TransportError> {
        contract
            .validate()
            .map_err(|error| TransportError::InvalidContract(error.to_string()))?;
        let shape = Shape::new(contract, maximum_events)?;
        shape.validate_binding(&binding)?;
        let audio_inputs = create_region(&binding.audio_inputs)?;
        let audio_outputs = create_region(&binding.audio_outputs)?;
        let mut events_to_worker = create_region(&binding.events_to_worker)?;
        let mut events_from_worker = create_region(&binding.events_from_worker)?;
        initialize_event_slot(&mut events_to_worker, shape.maximum_events)?;
        initialize_event_slot(&mut events_from_worker, shape.maximum_events)?;
        Ok(Self::from_parts(
            TransportSide::Controller,
            shape,
            audio_inputs,
            audio_outputs,
            events_to_worker,
            events_from_worker,
        ))
    }

    /// Open mappings created by the controller. The worker is never owner, so
    /// a crash cannot unlink buffers still needed for supervisor recovery.
    pub fn open(
        contract: &ProcessingContract,
        binding: &SharedMemoryBindingDto,
        maximum_events: u32,
    ) -> Result<Self, TransportError> {
        contract
            .validate()
            .map_err(|error| TransportError::InvalidContract(error.to_string()))?;
        let shape = Shape::new(contract, maximum_events)?;
        shape.validate_binding(binding)?;
        Ok(Self::from_parts(
            TransportSide::Worker,
            shape,
            open_region(&binding.audio_inputs)?,
            open_region(&binding.audio_outputs)?,
            open_region(&binding.events_to_worker)?,
            open_region(&binding.events_from_worker)?,
        ))
    }

    fn from_parts(
        side: TransportSide,
        shape: Shape,
        audio_inputs: Shmem,
        audio_outputs: Shmem,
        events_to_worker: Shmem,
        events_from_worker: Shmem,
    ) -> Self {
        Self {
            side,
            maximum_frames: shape.maximum_frames,
            input_channels: shape.input_channels,
            output_channels: shape.output_channels,
            maximum_events: shape.maximum_events,
            audio_inputs,
            audio_outputs,
            events_to_worker,
            events_from_worker,
        }
    }

    pub fn controller_write_inputs(
        &mut self,
        frames: u32,
        channels: &[&[f32]],
        events: &[InputEvent],
    ) -> Result<(), TransportError> {
        self.require_side(TransportSide::Controller)?;
        write_audio(
            &mut self.audio_inputs,
            self.input_channels,
            self.maximum_frames,
            frames,
            channels,
        )?;
        write_events(
            &mut self.events_to_worker,
            frames,
            self.maximum_events,
            events,
        )
    }

    pub fn worker_read_inputs(
        &self,
        frames: u32,
        channels: &mut [Vec<f32>],
    ) -> Result<Vec<InputEvent>, TransportError> {
        self.require_side(TransportSide::Worker)?;
        read_audio(
            &self.audio_inputs,
            self.input_channels,
            self.maximum_frames,
            frames,
            channels,
        )?;
        read_events(&self.events_to_worker, frames, self.maximum_events)
    }

    pub fn worker_write_outputs(
        &mut self,
        frames: u32,
        channels: &[&[f32]],
    ) -> Result<(), TransportError> {
        self.require_side(TransportSide::Worker)?;
        write_audio(
            &mut self.audio_outputs,
            self.output_channels,
            self.maximum_frames,
            frames,
            channels,
        )?;
        initialize_event_slot(&mut self.events_from_worker, self.maximum_events)
    }

    pub fn controller_read_outputs(
        &self,
        frames: u32,
        channels: &mut [Vec<f32>],
    ) -> Result<(), TransportError> {
        self.require_side(TransportSide::Controller)?;
        read_audio(
            &self.audio_outputs,
            self.output_channels,
            self.maximum_frames,
            frames,
            channels,
        )
    }

    /// Deterministic recovery fallback used when a process request times out or
    /// the worker exits. It never exposes stale output from the previous block.
    pub fn controller_zero_outputs(&mut self) -> Result<(), TransportError> {
        self.require_side(TransportSide::Controller)?;
        mapping_bytes_mut(&mut self.audio_outputs).fill(0);
        initialize_event_slot(&mut self.events_from_worker, self.maximum_events)
    }

    fn require_side(&self, expected: TransportSide) -> Result<(), TransportError> {
        if self.side == expected {
            Ok(())
        } else {
            Err(TransportError::InvalidBinding(format!(
                "operation requires {expected:?} side, opened as {:?}",
                self.side
            )))
        }
    }
}

#[derive(Clone, Copy)]
struct Shape {
    maximum_frames: usize,
    input_channels: usize,
    output_channels: usize,
    maximum_events: usize,
    audio_input_bytes: usize,
    audio_output_bytes: usize,
    event_bytes: usize,
}

impl Shape {
    fn new(contract: &ProcessingContract, maximum_events: u32) -> Result<Self, TransportError> {
        if maximum_events == 0 || maximum_events > 1_000_000 {
            return Err(TransportError::InvalidContract(
                "maximum event count must be in 1..=1,000,000".into(),
            ));
        }
        let input_channels = channel_slots(contract, PortDirection::Input)?;
        let output_channels = channel_slots(contract, PortDirection::Output)?;
        let maximum_frames = contract.maximum_frames as usize;
        let audio_input_bytes = audio_bytes(input_channels, maximum_frames)?;
        let audio_output_bytes = audio_bytes(output_channels, maximum_frames)?;
        let event_bytes = EVENT_HEADER_BYTES
            .checked_add(maximum_events as usize * EVENT_RECORD_BYTES)
            .ok_or_else(|| TransportError::InvalidContract("event slot size overflow".into()))?;
        // Mappings cannot be zero length. A port-less side gets one inert byte.
        Ok(Self {
            maximum_frames,
            input_channels,
            output_channels,
            maximum_events: maximum_events as usize,
            audio_input_bytes: audio_input_bytes.max(1),
            audio_output_bytes: audio_output_bytes.max(1),
            event_bytes,
        })
    }

    fn validate_binding(&self, binding: &SharedMemoryBindingDto) -> Result<(), TransportError> {
        let expected = [
            (
                &binding.audio_inputs,
                self.audio_input_bytes,
                SharedMemoryAccessDto::HostWrites,
            ),
            (
                &binding.audio_outputs,
                self.audio_output_bytes,
                SharedMemoryAccessDto::WorkerWrites,
            ),
            (
                &binding.events_to_worker,
                self.event_bytes,
                SharedMemoryAccessDto::HostWrites,
            ),
            (
                &binding.events_from_worker,
                self.event_bytes,
                SharedMemoryAccessDto::WorkerWrites,
            ),
        ];
        let mut tokens = std::collections::BTreeSet::new();
        for (region, byte_len, access) in expected {
            let token = region
                .token
                .value()
                .map_err(|error| TransportError::InvalidBinding(error.to_string()))?;
            if token == 0 || region.byte_len != byte_len as u64 || region.access != access {
                return Err(TransportError::InvalidBinding(format!(
                    "region {} has wrong size or access",
                    os_id(&region.token)?
                )));
            }
            if !tokens.insert(token) {
                return Err(TransportError::InvalidBinding(
                    "duplicate region token".into(),
                ));
            }
        }
        Ok(())
    }
}

/// Construct the exact wire binding required for a contract. The caller owns
/// token minting and must never reuse `token_base` while a mapping is alive.
pub fn binding_for(
    instance: TokenDto,
    contract: &ProcessingContract,
    maximum_events: u32,
    token_base: u128,
) -> Result<SharedMemoryBindingDto, TransportError> {
    contract
        .validate()
        .map_err(|error| TransportError::InvalidContract(error.to_string()))?;
    if token_base == 0 || token_base > u128::MAX - 4 {
        return Err(TransportError::InvalidBinding("invalid token base".into()));
    }
    let shape = Shape::new(contract, maximum_events)?;
    Ok(SharedMemoryBindingDto {
        instance,
        audio_inputs: region(
            token_base + 1,
            shape.audio_input_bytes,
            SharedMemoryAccessDto::HostWrites,
        ),
        audio_outputs: region(
            token_base + 2,
            shape.audio_output_bytes,
            SharedMemoryAccessDto::WorkerWrites,
        ),
        events_to_worker: region(
            token_base + 3,
            shape.event_bytes,
            SharedMemoryAccessDto::HostWrites,
        ),
        events_from_worker: region(
            token_base + 4,
            shape.event_bytes,
            SharedMemoryAccessDto::WorkerWrites,
        ),
    })
}

fn region(token: u128, byte_len: usize, access: SharedMemoryAccessDto) -> SharedMemoryRegionDto {
    SharedMemoryRegionDto {
        token: TokenDto::new(token),
        byte_len: byte_len as u64,
        access,
    }
}

fn channel_slots(
    contract: &ProcessingContract,
    direction: PortDirection,
) -> Result<usize, TransportError> {
    contract
        .audio_ports
        .iter()
        .filter(|port| port.direction == direction)
        .map(|port| {
            usize::from(port.channel_offset)
                .checked_add(usize::from(port.layout.channels()))
                .ok_or_else(|| TransportError::InvalidContract("channel slot overflow".into()))
        })
        .try_fold(0, |maximum, end| end.map(|end| maximum.max(end)))
}

fn audio_bytes(channels: usize, frames: usize) -> Result<usize, TransportError> {
    channels
        .checked_mul(frames)
        .and_then(|samples| samples.checked_mul(size_of::<f32>()))
        .ok_or_else(|| TransportError::InvalidContract("audio mapping size overflow".into()))
}

fn os_id(token: &TokenDto) -> Result<String, TransportError> {
    let value = token
        .value()
        .map_err(|error| TransportError::InvalidBinding(error.to_string()))?;
    // Darwin limits POSIX shm names to 31 bytes. The wire token remains 128
    // bits; the native name uses its low 96 bits (collision still causes a
    // fail-closed MappingIdExists rather than attaching the wrong region).
    let native = value & ((1_u128 << 96) - 1);
    Ok(format!("/ad_{native:024x}"))
}

fn create_region(region: &SharedMemoryRegionDto) -> Result<Shmem, TransportError> {
    let size = usize::try_from(region.byte_len)
        .map_err(|_| TransportError::InvalidBinding("region too large for platform".into()))?;
    ShmemConf::new()
        .os_id(os_id(&region.token)?)
        .size(size)
        .create()
        .map_err(|error| TransportError::Mapping(error.to_string()))
}

fn open_region(region: &SharedMemoryRegionDto) -> Result<Shmem, TransportError> {
    let size = usize::try_from(region.byte_len)
        .map_err(|_| TransportError::InvalidBinding("region too large for platform".into()))?;
    let mapping = ShmemConf::new()
        .os_id(os_id(&region.token)?)
        .size(size)
        .open()
        .map_err(|error| TransportError::Mapping(error.to_string()))?;
    if mapping.len() < size {
        return Err(TransportError::InvalidBinding(format!(
            "opened region has {} bytes, expected at least {size}",
            mapping.len()
        )));
    }
    Ok(mapping)
}

fn mapping_bytes(mapping: &Shmem) -> &[u8] {
    // SAFETY: the process protocol gives each region one writer. The caller
    // has waited for the peer's JSONL fence before taking this immutable view,
    // and this module never returns the slice outside the current operation.
    unsafe { mapping.as_slice() }
}

fn mapping_bytes_mut(mapping: &mut Shmem) -> &mut [u8] {
    // SAFETY: the process protocol gives each region one writer. `&mut Shmem`
    // prevents two local mutable views, the peer only accesses the region on
    // the opposite side of the Process/Processed fence, and the slice does not
    // escape this module.
    unsafe { mapping.as_slice_mut() }
}

fn write_audio(
    mapping: &mut Shmem,
    expected_channels: usize,
    maximum_frames: usize,
    frames: u32,
    channels: &[&[f32]],
) -> Result<(), TransportError> {
    let frames = frame_count(frames, maximum_frames)?;
    if channels.len() != expected_channels || channels.iter().any(|channel| channel.len() < frames)
    {
        return Err(TransportError::Bounds(format!(
            "expected {expected_channels} channels with at least {frames} frames"
        )));
    }
    let bytes = mapping_bytes_mut(mapping);
    for (index, channel) in channels.iter().enumerate() {
        let start = index * maximum_frames * 4;
        for (frame, sample) in channel[..frames].iter().enumerate() {
            let offset = start + frame * 4;
            bytes[offset..offset + 4].copy_from_slice(&sample.to_le_bytes());
        }
        bytes[start + frames * 4..start + maximum_frames * 4].fill(0);
    }
    Ok(())
}

fn read_audio(
    mapping: &Shmem,
    expected_channels: usize,
    maximum_frames: usize,
    frames: u32,
    channels: &mut [Vec<f32>],
) -> Result<(), TransportError> {
    let frames = frame_count(frames, maximum_frames)?;
    if channels.len() != expected_channels {
        return Err(TransportError::Bounds(format!(
            "expected {expected_channels} destination channels"
        )));
    }
    let bytes = mapping_bytes(mapping);
    for (index, channel) in channels.iter_mut().enumerate() {
        channel.resize(frames, 0.0);
        let start = index * maximum_frames * 4;
        for (frame, sample) in channel.iter_mut().enumerate() {
            let offset = start + frame * 4;
            *sample = f32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap());
        }
    }
    Ok(())
}

fn frame_count(frames: u32, maximum_frames: usize) -> Result<usize, TransportError> {
    let frames = frames as usize;
    if frames == 0 || frames > maximum_frames {
        Err(TransportError::Bounds(format!(
            "frame count {frames} is outside 1..={maximum_frames}"
        )))
    } else {
        Ok(frames)
    }
}

fn initialize_event_slot(mapping: &mut Shmem, maximum_events: usize) -> Result<(), TransportError> {
    let bytes = mapping_bytes_mut(mapping);
    let required = EVENT_HEADER_BYTES + maximum_events * EVENT_RECORD_BYTES;
    if bytes.len() < required {
        return Err(TransportError::InvalidBinding(
            "wrong event region size".into(),
        ));
    }
    bytes.fill(0);
    bytes[..8].copy_from_slice(EVENT_MAGIC);
    bytes[8..10].copy_from_slice(&EVENT_VERSION.to_le_bytes());
    bytes[10..12].copy_from_slice(&(EVENT_RECORD_BYTES as u16).to_le_bytes());
    Ok(())
}

fn write_events(
    mapping: &mut Shmem,
    frames: u32,
    maximum_events: usize,
    events: &[InputEvent],
) -> Result<(), TransportError> {
    if events.len() > maximum_events {
        return Err(TransportError::Bounds(format!(
            "{} events exceed capacity {maximum_events}",
            events.len()
        )));
    }
    initialize_event_slot(mapping, maximum_events)?;
    let bytes = mapping_bytes_mut(mapping);
    bytes[12..16].copy_from_slice(&(events.len() as u32).to_le_bytes());
    let mut previous = 0;
    for (index, event) in events.iter().enumerate() {
        let record = &mut bytes[EVENT_HEADER_BYTES + index * EVENT_RECORD_BYTES
            ..EVENT_HEADER_BYTES + (index + 1) * EVENT_RECORD_BYTES];
        let offset = match event {
            InputEvent::Parameter(event) => event.frame_offset,
            InputEvent::Note(event) => event.frame_offset,
        };
        if offset >= frames || (index > 0 && offset < previous) {
            return Err(TransportError::Bounds(
                "events must be sorted and inside block".into(),
            ));
        }
        previous = offset;
        put_u32(record, 4, offset);
        match event {
            InputEvent::Parameter(event) => {
                let PluginParameterKey::Clap(id) = event.key else {
                    return Err(TransportError::Bounds(
                        "CLAP transport requires CLAP parameter keys".into(),
                    ));
                };
                record[0] = 1;
                put_u32(record, 8, id);
                put_f64(record, 24, event.value.get());
            }
            InputEvent::Note(event) => encode_note(record, event)?,
        }
    }
    Ok(())
}

fn encode_note(record: &mut [u8], event: &PluginNoteEvent) -> Result<(), TransportError> {
    if event.address.key > 127 || event.address.note_id > i32::MAX as u32 {
        return Err(TransportError::Bounds("invalid CLAP note address".into()));
    }
    let (tag, dimension, value) = match event.kind {
        PluginNoteEventKind::On { velocity } => (2, 0, velocity.get()),
        PluginNoteEventKind::Off { velocity } => (3, 0, velocity.get()),
        PluginNoteEventKind::Choke => (4, 0, 0.0),
        PluginNoteEventKind::Expression { dimension, value } => {
            (5, expression_code(dimension), value)
        }
    };
    record[0] = tag;
    put_u32(record, 8, event.address.port);
    put_u16(record, 12, event.address.channel);
    put_u16(record, 14, event.address.key);
    put_u32(record, 16, event.address.note_id);
    put_u16(record, 20, dimension);
    put_f64(record, 24, value);
    Ok(())
}

fn read_events(
    mapping: &Shmem,
    frames: u32,
    maximum_events: usize,
) -> Result<Vec<InputEvent>, TransportError> {
    let bytes = mapping_bytes(mapping);
    if bytes.len() < EVENT_HEADER_BYTES + maximum_events * EVENT_RECORD_BYTES
        || &bytes[..8] != EVENT_MAGIC
        || get_u16(bytes, 8) != EVENT_VERSION
        || usize::from(get_u16(bytes, 10)) != EVENT_RECORD_BYTES
    {
        return Err(TransportError::CorruptEvents("invalid event header".into()));
    }
    let count = get_u32(bytes, 12) as usize;
    if count > maximum_events {
        return Err(TransportError::CorruptEvents(
            "event count exceeds capacity".into(),
        ));
    }
    let mut result = Vec::with_capacity(count);
    let mut previous = 0;
    for index in 0..count {
        let record = &bytes[EVENT_HEADER_BYTES + index * EVENT_RECORD_BYTES
            ..EVENT_HEADER_BYTES + (index + 1) * EVENT_RECORD_BYTES];
        let frame_offset = get_u32(record, 4);
        if frame_offset >= frames || (index > 0 && frame_offset < previous) {
            return Err(TransportError::CorruptEvents(
                "event offsets are invalid".into(),
            ));
        }
        previous = frame_offset;
        let event = match record[0] {
            1 => InputEvent::Parameter(ParameterEvent {
                frame_offset,
                key: PluginParameterKey::Clap(get_u32(record, 8)),
                value: NormalizedValue::new(get_f64(record, 24))
                    .map_err(|error| TransportError::CorruptEvents(error.to_string()))?,
            }),
            tag @ 2..=5 => InputEvent::Note(PluginNoteEvent {
                frame_offset,
                address: PluginNoteAddress {
                    port: get_u32(record, 8),
                    channel: get_u16(record, 12),
                    key: get_u16(record, 14),
                    note_id: get_u32(record, 16),
                },
                kind: match tag {
                    2 => PluginNoteEventKind::On {
                        velocity: normalized(record)?,
                    },
                    3 => PluginNoteEventKind::Off {
                        velocity: normalized(record)?,
                    },
                    4 => PluginNoteEventKind::Choke,
                    5 => PluginNoteEventKind::Expression {
                        dimension: decode_expression(get_u16(record, 20)),
                        value: get_f64(record, 24),
                    },
                    _ => unreachable!(),
                },
            }),
            tag => {
                return Err(TransportError::CorruptEvents(format!(
                    "unknown event tag {tag}"
                )));
            }
        };
        result.push(event);
    }
    Ok(result)
}

fn normalized(record: &[u8]) -> Result<NormalizedValue, TransportError> {
    NormalizedValue::new(get_f64(record, 24))
        .map_err(|error| TransportError::CorruptEvents(error.to_string()))
}

fn expression_code(value: PluginNoteExpression) -> u16 {
    match value {
        PluginNoteExpression::Pressure => 0,
        PluginNoteExpression::Tuning => 1,
        PluginNoteExpression::Brightness => 2,
        PluginNoteExpression::Timbre => 3,
        PluginNoteExpression::Pan => 4,
        PluginNoteExpression::Volume => 5,
        PluginNoteExpression::Other(value) => value.saturating_add(256),
    }
}

fn decode_expression(value: u16) -> PluginNoteExpression {
    match value {
        0 => PluginNoteExpression::Pressure,
        1 => PluginNoteExpression::Tuning,
        2 => PluginNoteExpression::Brightness,
        3 => PluginNoteExpression::Timbre,
        4 => PluginNoteExpression::Pan,
        5 => PluginNoteExpression::Volume,
        other => PluginNoteExpression::Other(other.saturating_sub(256)),
    }
}

fn put_u16(bytes: &mut [u8], offset: usize, value: u16) {
    bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}
fn put_u32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}
fn put_f64(bytes: &mut [u8], offset: usize, value: f64) {
    bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}
fn get_u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes(bytes[offset..offset + 2].try_into().unwrap())
}
fn get_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}
fn get_f64(bytes: &[u8], offset: usize) -> f64 {
    f64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::plugin::{ChannelLayout, NegotiatedAudioPort, NoteDialect, TailReport};

    fn contract() -> ProcessingContract {
        ProcessingContract {
            sample_rate: 48_000,
            minimum_frames: 1,
            maximum_frames: 64,
            audio_ports: vec![
                NegotiatedAudioPort {
                    native_id: 0,
                    direction: PortDirection::Input,
                    layout: ChannelLayout::Stereo,
                    channel_offset: 0,
                },
                NegotiatedAudioPort {
                    native_id: 1,
                    direction: PortDirection::Output,
                    layout: ChannelLayout::Stereo,
                    channel_offset: 0,
                },
            ],
            note_inputs: BTreeMap::from([(7, NoteDialect::Clap)]),
            note_outputs: BTreeMap::new(),
            initial_latency_frames: 0,
            initial_tail: TailReport::None,
            offline: false,
        }
    }

    #[test]
    fn macos_linux_named_mapping_round_trip_is_bounded_and_sample_accurate() {
        let contract = contract();
        let nonce = (std::process::id() as u128) << 64
            | std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
                & u64::MAX as u128;
        let binding = binding_for(TokenDto::new(44), &contract, 8, nonce).unwrap();
        let mut controller = SharedBlockTransport::create(&contract, binding.clone(), 8).unwrap();
        let mut worker = SharedBlockTransport::open(&contract, &binding, 8).unwrap();
        let left = [0.25, -0.5, 0.75, 1.0];
        let right = [1.0, 0.5, 0.0, -1.0];
        let events = [
            InputEvent::Parameter(ParameterEvent {
                frame_offset: 1,
                key: PluginParameterKey::Clap(9),
                value: NormalizedValue::new(0.5).unwrap(),
            }),
            InputEvent::Note(PluginNoteEvent {
                frame_offset: 3,
                address: PluginNoteAddress {
                    port: 7,
                    channel: 2,
                    key: 64,
                    note_id: 99,
                },
                kind: PluginNoteEventKind::On {
                    velocity: NormalizedValue::new(0.8).unwrap(),
                },
            }),
        ];
        controller
            .controller_write_inputs(4, &[&left, &right], &events)
            .unwrap();
        let mut input = vec![Vec::new(), Vec::new()];
        let decoded = worker.worker_read_inputs(4, &mut input).unwrap();
        assert_eq!(input, vec![left.to_vec(), right.to_vec()]);
        assert_eq!(decoded, events);

        let doubled_left = input[0].iter().map(|v| v * 2.0).collect::<Vec<_>>();
        let doubled_right = input[1].iter().map(|v| v * 2.0).collect::<Vec<_>>();
        worker
            .worker_write_outputs(4, &[&doubled_left, &doubled_right])
            .unwrap();
        let mut output = vec![Vec::new(), Vec::new()];
        controller.controller_read_outputs(4, &mut output).unwrap();
        assert_eq!(output, vec![doubled_left, doubled_right]);

        controller.controller_zero_outputs().unwrap();
        controller.controller_read_outputs(4, &mut output).unwrap();
        assert_eq!(output, vec![vec![0.0; 4], vec![0.0; 4]]);
    }
}
