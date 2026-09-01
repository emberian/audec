//! Append-only, checksummed framing for aggregate command batches.
//!
//! This module owns crash recovery mechanics, not command interpretation. Each
//! payload is JSON so unknown command records remain inspectable and
//! round-trippable, while a fixed binary frame header makes a torn final write
//! unambiguous. Recovery returns every complete verified prefix frame and
//! never applies a partial or checksum-mismatched frame.
//!
//! The checksum is deterministic FNV-1a 128. It detects accidental corruption
//! and torn writes; it is not authentication and makes no adversarial security
//! claim.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::io::{self, Write};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::command::CommandBatch;
use crate::command_record::DurableCommandBatch;

const FRAME_MAGIC: [u8; 8] = *b"AUDECJ1\0";
const HEADER_LEN: usize = FRAME_MAGIC.len() + 8 + 16;
pub const JOURNAL_FRAME_VERSION: u32 = 1;
pub const DEFAULT_MAX_FRAME_BYTES: u64 = 64 * 1024 * 1024;

/// Why a command batch was applied. Undo and redo are ordinary forward
/// applications in the journal, so recovery never needs to reconstruct the
/// controller's historical cursor before rebuilding project state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandOperation {
    Execute,
    Undo,
    Redo,
}

impl CommandOperation {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Execute => "execute",
            Self::Undo => "undo",
            Self::Redo => "redo",
        }
    }

    pub fn parse(value: &str) -> Result<Self, JournalFrameError> {
        match value {
            "execute" => Ok(Self::Execute),
            "undo" => Ok(Self::Undo),
            "redo" => Ok(Self::Redo),
            other => Err(JournalFrameError::UnknownOperation(other.to_owned())),
        }
    }
}

/// The controller-owned runtime journal record. Persistence supplies a codec
/// only at the edge; the controller never serializes domain models itself.
#[derive(Clone, Debug, PartialEq)]
pub struct CommandJournalRecord {
    pub sequence: u64,
    pub base_revision: u64,
    pub resulting_revision: u64,
    pub operation: CommandOperation,
    pub batch: CommandBatch,
}

impl CommandJournalRecord {
    pub fn new(
        sequence: u64,
        base_revision: u64,
        resulting_revision: u64,
        operation: CommandOperation,
        batch: CommandBatch,
    ) -> Result<Self, JournalFrameError> {
        let record = Self {
            sequence,
            base_revision,
            resulting_revision,
            operation,
            batch,
        };
        record.validate()?;
        Ok(record)
    }

    pub fn validate(&self) -> Result<(), JournalFrameError> {
        if self.sequence == 0 {
            return Err(JournalFrameError::ZeroSequence);
        }
        if self.batch.label.trim().is_empty() {
            return Err(JournalFrameError::EmptyBatchLabel);
        }
        if self.batch.commands.is_empty() {
            return Err(JournalFrameError::EmptyCommandBatch);
        }
        let expected = self
            .base_revision
            .checked_add(1)
            .ok_or(JournalFrameError::RevisionOverflow)?;
        if self.resulting_revision != expected {
            return Err(JournalFrameError::RevisionStep {
                base: self.base_revision,
                resulting: self.resulting_revision,
            });
        }
        Ok(())
    }
}

/// Persistence boundary for known runtime command codecs. Unknown durable
/// records remain owned by `DurableCommandBatch`; they are never handed to
/// the runtime controller for execution.
pub trait RuntimeCommandCodec {
    type Error: Error + Send + Sync + 'static;

    fn encode_batch(&self, batch: &CommandBatch) -> Result<DurableCommandBatch, Self::Error>;

    fn decode_batch(&self, batch: &DurableCommandBatch) -> Result<CommandBatch, Self::Error>;
}

#[derive(Debug)]
pub enum RuntimeJournalEncodeError<E> {
    Codec(E),
    Frame(JournalFrameError),
    Encode(JournalEncodeError),
}

impl<E: fmt::Display> fmt::Display for RuntimeJournalEncodeError<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Codec(error) => write!(formatter, "encoding command batch failed: {error}"),
            Self::Frame(error) => write!(formatter, "building journal frame failed: {error}"),
            Self::Encode(error) => error.fmt(formatter),
        }
    }
}

impl<E: Error + 'static> Error for RuntimeJournalEncodeError<E> {}

pub fn encode_runtime_record<C: RuntimeCommandCodec>(
    record: &CommandJournalRecord,
    codec: &C,
) -> Result<Vec<u8>, RuntimeJournalEncodeError<C::Error>> {
    record
        .validate()
        .map_err(RuntimeJournalEncodeError::Frame)?;
    let durable = codec
        .encode_batch(&record.batch)
        .map_err(RuntimeJournalEncodeError::Codec)?;
    let mut frame = JournalFrame::new(
        record.sequence,
        record.base_revision,
        record.operation.as_str(),
        durable,
    )
    .map_err(RuntimeJournalEncodeError::Frame)?;
    frame.resulting_revision = record.resulting_revision;
    encode_frame(&frame).map_err(RuntimeJournalEncodeError::Encode)
}

pub fn decode_runtime_frame<C: RuntimeCommandCodec>(
    frame: &JournalFrame,
    codec: &C,
) -> Result<CommandJournalRecord, RuntimeJournalEncodeError<C::Error>> {
    frame.validate().map_err(RuntimeJournalEncodeError::Frame)?;
    let batch = codec
        .decode_batch(&frame.batch)
        .map_err(RuntimeJournalEncodeError::Codec)?;
    CommandJournalRecord::new(
        frame.sequence,
        frame.base_revision,
        frame.resulting_revision,
        CommandOperation::parse(&frame.operation).map_err(RuntimeJournalEncodeError::Frame)?,
        batch,
    )
    .map_err(RuntimeJournalEncodeError::Frame)
}

pub fn encode_runtime_records<C: RuntimeCommandCodec>(
    records: &[CommandJournalRecord],
    codec: &C,
) -> Result<Vec<u8>, RuntimeJournalEncodeError<C::Error>> {
    validate_runtime_record_chain(records).map_err(RuntimeJournalEncodeError::Frame)?;
    let mut encoded = Vec::new();
    for record in records {
        encoded.extend(encode_runtime_record(record, codec)?);
    }
    Ok(encoded)
}

/// Validate a controller journal suffix before codecs or I/O observe it.
///
/// The first record may start above any checkpoint; every later record must
/// continue both the sequence and aggregate revision exactly. Recovery already
/// enforces this on bytes. Enforcing the same law before encoding prevents a
/// caller from deliberately writing a suffix that the recovery path must
/// reject on the next launch.
pub fn validate_runtime_record_chain(
    records: &[CommandJournalRecord],
) -> Result<(), JournalFrameError> {
    for record in records {
        record.validate()?;
    }
    for pair in records.windows(2) {
        let previous = &pair[0];
        let next = &pair[1];
        let expected_sequence = previous
            .sequence
            .checked_add(1)
            .ok_or(JournalFrameError::SequenceOverflow)?;
        if next.sequence != expected_sequence {
            return Err(JournalFrameError::SequenceGap {
                expected: expected_sequence,
                actual: next.sequence,
            });
        }
        if next.base_revision != previous.resulting_revision {
            return Err(JournalFrameError::RevisionGap {
                expected: previous.resulting_revision,
                actual: next.base_revision,
            });
        }
    }
    Ok(())
}

/// One command application as it actually occurred.
///
/// Undo and redo append their applied inverse/forward batches as new frames;
/// coalescing may combine in-memory undo entries but never rewrites this log.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct JournalFrame {
    pub frame_version: u32,
    pub sequence: u64,
    pub base_revision: u64,
    pub resulting_revision: u64,
    /// Known values currently include `execute`, `undo`, and `redo`. It is a
    /// string so future operation labels remain round-trippable.
    pub operation: String,
    pub batch: DurableCommandBatch,
    #[serde(default, flatten)]
    pub extensions: BTreeMap<String, Value>,
}

impl JournalFrame {
    pub fn new(
        sequence: u64,
        base_revision: u64,
        operation: impl Into<String>,
        batch: DurableCommandBatch,
    ) -> Result<Self, JournalFrameError> {
        let resulting_revision = base_revision
            .checked_add(1)
            .ok_or(JournalFrameError::RevisionOverflow)?;
        let frame = Self {
            frame_version: JOURNAL_FRAME_VERSION,
            sequence,
            base_revision,
            resulting_revision,
            operation: operation.into(),
            batch,
            extensions: BTreeMap::new(),
        };
        frame.validate()?;
        Ok(frame)
    }

    pub fn validate(&self) -> Result<(), JournalFrameError> {
        if self.frame_version == 0 {
            return Err(JournalFrameError::ZeroVersion);
        }
        if self.sequence == 0 {
            return Err(JournalFrameError::ZeroSequence);
        }
        if self.operation.trim().is_empty() {
            return Err(JournalFrameError::EmptyOperation);
        }
        if self.batch.schema_version == 0 {
            return Err(JournalFrameError::ZeroBatchVersion);
        }
        if self.batch.label.trim().is_empty() {
            return Err(JournalFrameError::EmptyBatchLabel);
        }
        if self.batch.commands.is_empty() {
            return Err(JournalFrameError::EmptyCommandBatch);
        }
        let expected = self
            .base_revision
            .checked_add(1)
            .ok_or(JournalFrameError::RevisionOverflow)?;
        if self.resulting_revision != expected {
            return Err(JournalFrameError::RevisionStep {
                base: self.base_revision,
                resulting: self.resulting_revision,
            });
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum JournalFrameError {
    ZeroVersion,
    ZeroSequence,
    EmptyOperation,
    UnknownOperation(String),
    ZeroBatchVersion,
    EmptyBatchLabel,
    EmptyCommandBatch,
    SequenceOverflow,
    SequenceGap { expected: u64, actual: u64 },
    RevisionGap { expected: u64, actual: u64 },
    RevisionOverflow,
    RevisionStep { base: u64, resulting: u64 },
}

impl fmt::Display for JournalFrameError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroVersion => formatter.write_str("journal frame version is zero"),
            Self::ZeroSequence => formatter.write_str("journal frame sequence is zero"),
            Self::EmptyOperation => formatter.write_str("journal operation is empty"),
            Self::UnknownOperation(operation) => {
                write!(formatter, "unknown journal operation {operation:?}")
            }
            Self::ZeroBatchVersion => formatter.write_str("command batch version is zero"),
            Self::EmptyBatchLabel => formatter.write_str("command batch label is empty"),
            Self::EmptyCommandBatch => formatter.write_str("command batch contains no commands"),
            Self::SequenceOverflow => formatter.write_str("journal sequence overflows u64"),
            Self::SequenceGap { expected, actual } => write!(
                formatter,
                "journal sequence is disconnected: expected {expected}, actual {actual}"
            ),
            Self::RevisionGap { expected, actual } => write!(
                formatter,
                "journal revision is disconnected: expected {expected}, actual {actual}"
            ),
            Self::RevisionOverflow => formatter.write_str("journal revision overflows u64"),
            Self::RevisionStep { base, resulting } => write!(
                formatter,
                "journal revision step {base} -> {resulting} is not exactly one commit"
            ),
        }
    }
}

impl Error for JournalFrameError {}

#[derive(Debug)]
pub enum JournalEncodeError {
    Invalid(JournalFrameError),
    Json(serde_json::Error),
    FrameTooLarge(u64),
    Io(io::Error),
}

impl fmt::Display for JournalEncodeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(error) => write!(formatter, "invalid journal frame: {error}"),
            Self::Json(error) => write!(formatter, "journal frame JSON failed: {error}"),
            Self::FrameTooLarge(bytes) => {
                write!(
                    formatter,
                    "journal frame payload is too large: {bytes} bytes"
                )
            }
            Self::Io(error) => write!(formatter, "writing journal frame failed: {error}"),
        }
    }
}

impl Error for JournalEncodeError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Invalid(error) => Some(error),
            Self::Json(error) => Some(error),
            Self::Io(error) => Some(error),
            Self::FrameTooLarge(_) => None,
        }
    }
}

/// Encode one complete self-delimiting frame.
pub fn encode_frame(frame: &JournalFrame) -> Result<Vec<u8>, JournalEncodeError> {
    frame.validate().map_err(JournalEncodeError::Invalid)?;
    let payload = serde_json::to_vec(frame).map_err(JournalEncodeError::Json)?;
    let payload_len = payload.len() as u64;
    if payload_len > DEFAULT_MAX_FRAME_BYTES {
        return Err(JournalEncodeError::FrameTooLarge(payload_len));
    }
    let checksum = fnv1a_128(&payload);
    let mut encoded = Vec::with_capacity(HEADER_LEN + payload.len());
    encoded.extend_from_slice(&FRAME_MAGIC);
    encoded.extend_from_slice(&payload_len.to_le_bytes());
    encoded.extend_from_slice(&checksum.to_le_bytes());
    encoded.extend_from_slice(&payload);
    Ok(encoded)
}

/// Write one frame. Durability (`flush`, `sync_all`, checkpoint ordering) is
/// deliberately owned by the caller's journal sink.
pub fn write_frame(
    writer: &mut impl Write,
    frame: &JournalFrame,
) -> Result<usize, JournalEncodeError> {
    let encoded = encode_frame(frame)?;
    writer.write_all(&encoded).map_err(JournalEncodeError::Io)?;
    Ok(encoded.len())
}

/// Append one encoded frame to an in-memory journal, useful to controller and
/// recovery tests without filesystem timing.
pub fn append_frame(
    journal: &mut Vec<u8>,
    frame: &JournalFrame,
) -> Result<usize, JournalEncodeError> {
    let encoded = encode_frame(frame)?;
    let bytes = encoded.len();
    journal.extend_from_slice(&encoded);
    Ok(bytes)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum JournalTail {
    Clean,
    /// EOF occurred inside the next header or payload. All frames before
    /// `offset` are verified and may be replayed.
    Truncated {
        offset: usize,
        available: usize,
        required: Option<usize>,
    },
    /// A complete-looking frame failed integrity, JSON, or chain validation.
    Corrupt {
        offset: usize,
        error: JournalCorruption,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum JournalCorruption {
    BadMagic,
    FrameTooLarge(u64),
    LengthOverflow,
    ChecksumMismatch,
    Json(String),
    InvalidFrame(String),
    SequenceGap { expected: u64, actual: u64 },
    RevisionGap { expected: u64, actual: u64 },
}

/// The verified prefix of a journal and the disposition of its remaining
/// bytes. Callers may replay `frames` for both truncated and corrupt tails,
/// then surface `tail` rather than guessing at the rejected suffix.
#[derive(Clone, Debug, PartialEq)]
pub struct JournalRecovery {
    pub frames: Vec<JournalFrame>,
    pub valid_bytes: usize,
    pub tail: JournalTail,
}

impl JournalRecovery {
    pub fn is_complete(&self) -> bool {
        self.tail == JournalTail::Clean
    }
}

pub fn recover_prefix(bytes: &[u8]) -> JournalRecovery {
    recover_prefix_with_limit(bytes, DEFAULT_MAX_FRAME_BYTES)
}

pub fn recover_prefix_with_limit(bytes: &[u8], max_frame_bytes: u64) -> JournalRecovery {
    let mut offset = 0_usize;
    let mut frames: Vec<JournalFrame> = Vec::new();
    while offset < bytes.len() {
        let available = bytes.len() - offset;
        if available < HEADER_LEN {
            return recovery(
                frames,
                offset,
                JournalTail::Truncated {
                    offset,
                    available,
                    required: Some(HEADER_LEN),
                },
            );
        }
        let header = &bytes[offset..offset + HEADER_LEN];
        if header[..FRAME_MAGIC.len()] != FRAME_MAGIC {
            return corrupt(frames, offset, JournalCorruption::BadMagic);
        }
        let payload_len = u64::from_le_bytes(
            header[FRAME_MAGIC.len()..FRAME_MAGIC.len() + 8]
                .try_into()
                .expect("fixed header slice"),
        );
        if payload_len > max_frame_bytes {
            return corrupt(
                frames,
                offset,
                JournalCorruption::FrameTooLarge(payload_len),
            );
        }
        let Ok(payload_len_usize) = usize::try_from(payload_len) else {
            return corrupt(frames, offset, JournalCorruption::LengthOverflow);
        };
        let Some(frame_len) = HEADER_LEN.checked_add(payload_len_usize) else {
            return corrupt(frames, offset, JournalCorruption::LengthOverflow);
        };
        if available < frame_len {
            return recovery(
                frames,
                offset,
                JournalTail::Truncated {
                    offset,
                    available,
                    required: Some(frame_len),
                },
            );
        }
        let expected_checksum = u128::from_le_bytes(
            header[FRAME_MAGIC.len() + 8..HEADER_LEN]
                .try_into()
                .expect("fixed checksum slice"),
        );
        let payload = &bytes[offset + HEADER_LEN..offset + frame_len];
        if fnv1a_128(payload) != expected_checksum {
            return corrupt(frames, offset, JournalCorruption::ChecksumMismatch);
        }
        let frame: JournalFrame = match serde_json::from_slice(payload) {
            Ok(frame) => frame,
            Err(error) => {
                return corrupt(frames, offset, JournalCorruption::Json(error.to_string()))
            }
        };
        if let Err(error) = frame.validate() {
            return corrupt(
                frames,
                offset,
                JournalCorruption::InvalidFrame(error.to_string()),
            );
        }
        if let Some(previous) = frames.last() {
            let expected_sequence = match previous.sequence.checked_add(1) {
                Some(value) => value,
                None => {
                    return corrupt(
                        frames,
                        offset,
                        JournalCorruption::SequenceGap {
                            expected: u64::MAX,
                            actual: frame.sequence,
                        },
                    )
                }
            };
            if frame.sequence != expected_sequence {
                return corrupt(
                    frames,
                    offset,
                    JournalCorruption::SequenceGap {
                        expected: expected_sequence,
                        actual: frame.sequence,
                    },
                );
            }
            let expected_revision = previous.resulting_revision;
            if frame.base_revision != expected_revision {
                return corrupt(
                    frames,
                    offset,
                    JournalCorruption::RevisionGap {
                        expected: expected_revision,
                        actual: frame.base_revision,
                    },
                );
            }
        }
        frames.push(frame);
        offset += frame_len;
    }
    recovery(frames, offset, JournalTail::Clean)
}

fn recovery(frames: Vec<JournalFrame>, valid_bytes: usize, tail: JournalTail) -> JournalRecovery {
    JournalRecovery {
        frames,
        valid_bytes,
        tail,
    }
}

fn corrupt(
    frames: Vec<JournalFrame>,
    valid_bytes: usize,
    error: JournalCorruption,
) -> JournalRecovery {
    recovery(
        frames,
        valid_bytes,
        JournalTail::Corrupt {
            offset: valid_bytes,
            error,
        },
    )
}

fn fnv1a_128(bytes: &[u8]) -> u128 {
    const OFFSET: u128 = 0x6c62_272e_07bb_0142_62b8_2175_6295_c58d;
    const PRIME: u128 = 0x0000_0000_0100_0000_0000_0000_0000_013b;
    let mut hash = OFFSET;
    for byte in bytes {
        hash ^= u128::from(*byte);
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::{BindingCommand, DomainCommand};
    use crate::command_record::OpaqueCommandRecord;

    fn frame(sequence: u64, base: u64, marker: &str) -> JournalFrame {
        let command = OpaqueCommandRecord {
            domain: "future-domain".into(),
            kind: "future-command".into(),
            schema_version: 19,
            payload: serde_json::json!({ "marker": marker }),
            extensions: BTreeMap::from([(
                "unknown-command-field".into(),
                serde_json::json!([1, 2, 3]),
            )]),
        };
        let mut batch = DurableCommandBatch::new(marker, vec![command]);
        batch.extensions.insert(
            "unknown-batch-field".into(),
            serde_json::json!({ "kept": true }),
        );
        JournalFrame::new(sequence, base, "execute", batch).unwrap()
    }

    fn runtime_record(sequence: u64, base: u64) -> CommandJournalRecord {
        CommandJournalRecord::new(
            sequence,
            base,
            base + 1,
            CommandOperation::Execute,
            CommandBatch::new(
                format!("record {sequence}"),
                vec![DomainCommand::Bindings(BindingCommand::PutTrackBus {
                    track: crate::arrangement::TrackId::from_raw(1),
                    before: None,
                    after: None,
                })],
            ),
        )
        .unwrap()
    }

    #[test]
    fn frames_round_trip_with_opaque_unknown_records() {
        let original = frame(1, 40, "alpha");
        let bytes = encode_frame(&original).unwrap();
        let recovered = recover_prefix(&bytes);
        assert!(recovered.is_complete());
        assert_eq!(recovered.valid_bytes, bytes.len());
        assert_eq!(recovered.frames, vec![original]);
        let command = &recovered.frames[0].batch.commands[0];
        assert_eq!(command.codec_key(), ("future-domain", "future-command", 19));
        assert_eq!(command.extensions["unknown-command-field"][2], 3);
    }

    #[test]
    fn every_torn_final_frame_recovers_the_verified_prefix_only() {
        let first = encode_frame(&frame(1, 7, "first")).unwrap();
        let second = encode_frame(&frame(2, 8, "second")).unwrap();
        let mut journal = first.clone();
        journal.extend_from_slice(&second);

        for cut in first.len() + 1..journal.len() {
            let recovered = recover_prefix(&journal[..cut]);
            assert_eq!(recovered.frames, vec![frame(1, 7, "first")], "cut={cut}");
            assert_eq!(recovered.valid_bytes, first.len(), "cut={cut}");
            assert!(
                matches!(recovered.tail, JournalTail::Truncated { .. }),
                "cut={cut}, tail={:?}",
                recovered.tail
            );
        }
    }

    #[test]
    fn checksum_failure_never_applies_the_damaged_frame() {
        let first = encode_frame(&frame(4, 12, "first")).unwrap();
        let second = encode_frame(&frame(5, 13, "second")).unwrap();
        let mut journal = first.clone();
        journal.extend_from_slice(&second);
        let last = journal.len() - 1;
        journal[last] ^= 0x01;

        let recovered = recover_prefix(&journal);
        assert_eq!(recovered.frames, vec![frame(4, 12, "first")]);
        assert_eq!(recovered.valid_bytes, first.len());
        assert_eq!(
            recovered.tail,
            JournalTail::Corrupt {
                offset: first.len(),
                error: JournalCorruption::ChecksumMismatch,
            }
        );
    }

    #[test]
    fn revision_gap_stops_before_the_disconnected_frame() {
        let first = encode_frame(&frame(1, 100, "first")).unwrap();
        let disconnected = encode_frame(&frame(2, 900, "disconnected")).unwrap();
        let mut journal = first.clone();
        journal.extend_from_slice(&disconnected);
        let recovered = recover_prefix(&journal);
        assert_eq!(recovered.frames.len(), 1);
        assert_eq!(recovered.valid_bytes, first.len());
        assert_eq!(
            recovered.tail,
            JournalTail::Corrupt {
                offset: first.len(),
                error: JournalCorruption::RevisionGap {
                    expected: 101,
                    actual: 900,
                },
            }
        );
    }

    #[test]
    fn runtime_chain_is_rejected_before_encoding_when_sequence_or_revision_disconnects() {
        let connected = vec![runtime_record(7, 40), runtime_record(8, 41)];
        assert_eq!(validate_runtime_record_chain(&connected), Ok(()));

        let sequence_gap = vec![runtime_record(7, 40), runtime_record(9, 41)];
        assert_eq!(
            validate_runtime_record_chain(&sequence_gap),
            Err(JournalFrameError::SequenceGap {
                expected: 8,
                actual: 9,
            })
        );

        let revision_gap = vec![runtime_record(7, 40), runtime_record(8, 99)];
        assert_eq!(
            validate_runtime_record_chain(&revision_gap),
            Err(JournalFrameError::RevisionGap {
                expected: 41,
                actual: 99,
            })
        );
    }

    #[test]
    fn empty_batches_are_not_valid_journal_events() {
        let runtime = CommandJournalRecord {
            sequence: 1,
            base_revision: 0,
            resulting_revision: 1,
            operation: CommandOperation::Execute,
            batch: CommandBatch::new("empty", Vec::new()),
        };
        assert_eq!(
            runtime.validate(),
            Err(JournalFrameError::EmptyCommandBatch)
        );

        let durable = DurableCommandBatch::new("empty", Vec::new());
        assert_eq!(
            JournalFrame::new(1, 0, "execute", durable),
            Err(JournalFrameError::EmptyCommandBatch)
        );
    }
}
