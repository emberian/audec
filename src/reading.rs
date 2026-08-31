//! Portable identity and verification model for shareable readings.
//!
//! A reading carries claims and reconstruction recipes, never source PCM and
//! never authority. Every imported entity remains qualified by the reading
//! that minted it, so import and merge do not renumber or silently collapse
//! competing interpretations.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

use crate::artifact_catalog::{ContentDigest, DigestAlgorithm};
use crate::ontology;

pub const READING_FORMAT: &str = "audec-reading";
pub const READING_FORMAT_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ReadingId([u8; 16]);

impl ReadingId {
    pub fn new(bytes: [u8; 16]) -> Option<Self> {
        (bytes != [0; 16]).then_some(Self(bytes))
    }

    pub const fn bytes(self) -> [u8; 16] {
        self.0
    }
}

impl fmt::Display for ReadingId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(formatter, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl FromStr for ReadingId {
    type Err = ReadingError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let bytes = decode_hex::<16>(value).ok_or(ReadingError::InvalidReadingId)?;
        Self::new(bytes).ok_or(ReadingError::InvalidReadingId)
    }
}

impl Serialize for ReadingId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for ReadingId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

/// Entity kind is an open string vocabulary. Older readers retain unfamiliar
/// kinds instead of mapping them to a known local type.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct QualifiedEntityId {
    pub reading: ReadingId,
    pub kind: String,
    pub local_id: u64,
}

impl QualifiedEntityId {
    pub fn new(
        reading: ReadingId,
        kind: impl Into<String>,
        local_id: u64,
    ) -> Result<Self, ReadingError> {
        let value = Self {
            reading,
            kind: kind.into(),
            local_id,
        };
        value.validate()?;
        Ok(value)
    }

    pub fn validate(&self) -> Result<(), ReadingError> {
        if self.local_id == 0 || !valid_token(&self.kind) {
            return Err(ReadingError::InvalidQualifiedId(self.clone()));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PortableDigestAlgorithm {
    Sha256,
    Blake3,
    StableNonCryptographic,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct PortableDigest {
    pub algorithm: PortableDigestAlgorithm,
    #[serde(with = "hex_32")]
    pub bytes: [u8; 32],
}

impl PortableDigest {
    pub fn is_strong(self) -> bool {
        matches!(
            self.algorithm,
            PortableDigestAlgorithm::Sha256 | PortableDigestAlgorithm::Blake3
        )
    }
}

impl From<ContentDigest> for PortableDigest {
    fn from(value: ContentDigest) -> Self {
        Self {
            algorithm: match value.algorithm {
                DigestAlgorithm::Sha256 => PortableDigestAlgorithm::Sha256,
                DigestAlgorithm::Blake3 => PortableDigestAlgorithm::Blake3,
                DigestAlgorithm::StableNonCryptographic => {
                    PortableDigestAlgorithm::StableNonCryptographic
                }
            },
            bytes: value.bytes,
        }
    }
}

impl From<PortableDigest> for ContentDigest {
    fn from(value: PortableDigest) -> Self {
        ContentDigest::new(
            match value.algorithm {
                PortableDigestAlgorithm::Sha256 => DigestAlgorithm::Sha256,
                PortableDigestAlgorithm::Blake3 => DigestAlgorithm::Blake3,
                PortableDigestAlgorithm::StableNonCryptographic => {
                    DigestAlgorithm::StableNonCryptographic
                }
            },
            value.bytes,
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadingVersionRef {
    pub reading_id: ReadingId,
    pub revision: u64,
    pub manifest_digest: PortableDigest,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProducerDto {
    Human {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    Analyzer {
        name: String,
        version: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        configuration_digest: Option<String>,
    },
    Importer {
        format: String,
        version: String,
    },
}

impl From<&ontology::Producer> for ProducerDto {
    fn from(value: &ontology::Producer) -> Self {
        match value {
            ontology::Producer::Human { name } => Self::Human { name: name.clone() },
            ontology::Producer::Analyzer {
                name,
                version,
                configuration_digest,
            } => Self::Analyzer {
                name: name.clone(),
                version: version.clone(),
                configuration_digest: configuration_digest.clone(),
            },
            ontology::Producer::Importer { format, version } => Self::Importer {
                format: format.clone(),
                version: version.clone(),
            },
        }
    }
}

impl From<ProducerDto> for ontology::Producer {
    fn from(value: ProducerDto) -> Self {
        match value {
            ProducerDto::Human { name } => Self::Human { name },
            ProducerDto::Analyzer {
                name,
                version,
                configuration_digest,
            } => Self::Analyzer {
                name,
                version,
                configuration_digest,
            },
            ProducerDto::Importer { format, version } => Self::Importer { format, version },
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProvenanceDto {
    pub producer: ProducerDto,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_unix_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_revision: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl From<&ontology::Provenance> for ProvenanceDto {
    fn from(value: &ontology::Provenance) -> Self {
        Self {
            producer: (&value.producer).into(),
            created_unix_ms: value.created_unix_ms,
            source_revision: value.source_revision.clone(),
            note: value.note.clone(),
        }
    }
}

impl From<ProvenanceDto> for ontology::Provenance {
    fn from(value: ProvenanceDto) -> Self {
        Self {
            producer: value.producer.into(),
            created_unix_ms: value.created_unix_ms,
            source_revision: value.source_revision,
            note: value.note,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadingSource {
    /// At least one collision-resistant identity is required. Multiple values
    /// may represent independently hashed containers of exactly the declared
    /// decoded source; tolerant remaster alignment is deliberately absent.
    pub fingerprints: Vec<PortableDigest>,
    pub sample_rate: u32,
    pub channels: u16,
    pub frame_count: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub declared_title: Option<String>,
    #[serde(default, flatten)]
    pub extensions: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReadingSection {
    pub name: String,
    pub schema_major: u32,
    #[serde(default)]
    pub schema_minor: u32,
    /// Unknown sections and unknown fields inside known sections remain raw
    /// JSON values and survive decode/encode unchanged in meaning.
    pub payload: Value,
    #[serde(default, flatten)]
    pub extensions: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadingAttachmentRef {
    pub digest: PortableDigest,
    pub media_type: String,
    pub role: String,
    pub provenance: ProvenanceDto,
    #[serde(default, flatten)]
    pub extensions: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReadingFile {
    pub format: String,
    pub version: u32,
    pub reading_id: ReadingId,
    pub revision: u64,
    #[serde(default)]
    pub parents: Vec<ReadingVersionRef>,
    pub author: ProvenanceDto,
    pub source: ReadingSource,
    #[serde(default)]
    pub sections: Vec<ReadingSection>,
    /// Derived audio is opt-in and referenced by digest; bytes are never
    /// smuggled into the core envelope.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<ReadingAttachmentRef>,
    #[serde(default, flatten)]
    pub extensions: BTreeMap<String, Value>,
}

impl ReadingFile {
    pub fn validate(&self) -> Result<(), ReadingError> {
        if self.format != READING_FORMAT {
            return Err(ReadingError::WrongFormat(self.format.clone()));
        }
        if self.version != READING_FORMAT_VERSION {
            return Err(ReadingError::UnsupportedVersion(self.version));
        }
        if self.revision == 0 {
            return Err(ReadingError::ZeroRevision);
        }
        validate_source(&self.source)?;
        let mut parents = BTreeSet::new();
        for parent in &self.parents {
            if parent.revision == 0 || !parent.manifest_digest.is_strong() {
                return Err(ReadingError::InvalidParent(parent.clone()));
            }
            if !parents.insert((parent.reading_id, parent.revision)) {
                return Err(ReadingError::DuplicateParent(parent.clone()));
            }
        }
        let mut sections = BTreeSet::new();
        for section in &self.sections {
            if !valid_token(&section.name) || section.schema_major == 0 {
                return Err(ReadingError::InvalidSection(section.name.clone()));
            }
            if !sections.insert(section.name.clone()) {
                return Err(ReadingError::DuplicateSection(section.name.clone()));
            }
        }
        for attachment in &self.attachments {
            if !attachment.digest.is_strong()
                || attachment.media_type.trim().is_empty()
                || !valid_token(&attachment.role)
            {
                return Err(ReadingError::InvalidAttachment(attachment.role.clone()));
            }
        }
        Ok(())
    }

    /// Tier 1 needs no audio. Tier 2 is granted only by exact digest and
    /// decoded-format agreement. Tier 3 is a typed refusal, never a warning.
    pub fn verify_source(
        &self,
        local: Option<&LocalSourceDescriptor>,
    ) -> Result<VerificationTier, ReadingVerificationRefusal> {
        let Some(local) = local else {
            return Ok(VerificationTier::GraphOnly);
        };
        if self.source.sample_rate != local.sample_rate {
            return Err(ReadingVerificationRefusal::SampleRate {
                expected: self.source.sample_rate,
                actual: local.sample_rate,
            });
        }
        if self.source.channels != local.channels {
            return Err(ReadingVerificationRefusal::Channels {
                expected: self.source.channels,
                actual: local.channels,
            });
        }
        if self.source.frame_count != local.frame_count {
            return Err(ReadingVerificationRefusal::FrameCount {
                expected: self.source.frame_count,
                actual: local.frame_count,
                delta: i128::from(local.frame_count) - i128::from(self.source.frame_count),
            });
        }
        if !local.digest.is_strong() {
            return Err(ReadingVerificationRefusal::WeakLocalFingerprint);
        }
        if !self.source.fingerprints.contains(&local.digest) {
            return Err(ReadingVerificationRefusal::FingerprintMismatch {
                expected: self.source.fingerprints.clone(),
                actual: local.digest,
            });
        }
        Ok(VerificationTier::SourceMatched)
    }
}

fn validate_source(source: &ReadingSource) -> Result<(), ReadingError> {
    if source.fingerprints.is_empty()
        || source.fingerprints.iter().any(|digest| !digest.is_strong())
    {
        return Err(ReadingError::WeakOrMissingSourceFingerprint);
    }
    if source.sample_rate == 0 || source.channels == 0 || source.frame_count == 0 {
        return Err(ReadingError::InvalidSourceFormat);
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LocalSourceDescriptor {
    pub digest: PortableDigest,
    pub sample_rate: u32,
    pub channels: u16,
    pub frame_count: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VerificationTier {
    /// Claim graph and recipes are inspectable; nothing is auditionable.
    GraphOnly,
    /// Exact source identity and decoded geometry match. Re-rendering may now
    /// establish replication; matching alone does not claim it happened.
    SourceMatched,
    /// Caller re-ran every declared verification record successfully.
    Replicated,
}

pub fn replication_tier(
    source_tier: VerificationTier,
    checks: &[ReplicationCheck],
) -> Result<VerificationTier, ReadingVerificationRefusal> {
    if source_tier != VerificationTier::SourceMatched {
        return Err(ReadingVerificationRefusal::SourceNotMatched);
    }
    if checks.is_empty() {
        return Err(ReadingVerificationRefusal::NoReplicationChecks);
    }
    if let Some(check) = checks.iter().find(|check| !check.matches) {
        return Err(ReadingVerificationRefusal::ReplicationMismatch(
            check.subject.clone(),
        ));
    }
    Ok(VerificationTier::Replicated)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReplicationCheck {
    pub subject: QualifiedEntityId,
    pub matches: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReadingVerificationRefusal {
    WeakLocalFingerprint,
    FingerprintMismatch {
        expected: Vec<PortableDigest>,
        actual: PortableDigest,
    },
    SampleRate {
        expected: u32,
        actual: u32,
    },
    Channels {
        expected: u16,
        actual: u16,
    },
    FrameCount {
        expected: u64,
        actual: u64,
        delta: i128,
    },
    SourceNotMatched,
    NoReplicationChecks,
    ReplicationMismatch(QualifiedEntityId),
}

#[derive(Clone, Debug, PartialEq)]
pub enum ReadingError {
    InvalidReadingId,
    InvalidQualifiedId(QualifiedEntityId),
    WrongFormat(String),
    UnsupportedVersion(u32),
    ZeroRevision,
    WeakOrMissingSourceFingerprint,
    InvalidSourceFormat,
    InvalidParent(ReadingVersionRef),
    DuplicateParent(ReadingVersionRef),
    InvalidSection(String),
    DuplicateSection(String),
    InvalidAttachment(String),
}

impl fmt::Display for ReadingError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "invalid reading: {self:?}")
    }
}

impl std::error::Error for ReadingError {}

fn valid_token(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn decode_hex<const N: usize>(value: &str) -> Option<[u8; N]> {
    if value.len() != N * 2 || !value.is_ascii() {
        return None;
    }
    let mut out = [0; N];
    for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
        out[index] = (hex_digit(pair[0])? << 4) | hex_digit(pair[1])?;
    }
    Some(out)
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

mod hex_32 {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S>(bytes: &[u8; 32], serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut text = String::with_capacity(64);
        for byte in bytes {
            use std::fmt::Write;
            write!(&mut text, "{byte:02x}").expect("writing to String cannot fail");
        }
        serializer.serialize_str(&text)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<[u8; 32], D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        super::decode_hex::<32>(&value)
            .ok_or_else(|| serde::de::Error::custom("expected 64 hexadecimal characters"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digest(value: u8) -> PortableDigest {
        PortableDigest {
            algorithm: PortableDigestAlgorithm::Sha256,
            bytes: [value; 32],
        }
    }

    fn reading() -> ReadingFile {
        ReadingFile {
            format: READING_FORMAT.into(),
            version: READING_FORMAT_VERSION,
            reading_id: ReadingId::new([1; 16]).unwrap(),
            revision: 1,
            parents: Vec::new(),
            author: ProvenanceDto {
                producer: ProducerDto::Human {
                    name: Some("ember".into()),
                },
                created_unix_ms: None,
                source_revision: None,
                note: None,
            },
            source: ReadingSource {
                fingerprints: vec![digest(7)],
                sample_rate: 48_000,
                channels: 2,
                frame_count: 100,
                declared_title: None,
                extensions: BTreeMap::new(),
            },
            sections: Vec::new(),
            attachments: Vec::new(),
            extensions: BTreeMap::new(),
        }
    }

    #[test]
    fn source_verification_has_explicit_graph_match_and_refusal_states() {
        let reading = reading();
        assert_eq!(reading.verify_source(None), Ok(VerificationTier::GraphOnly));
        let local = LocalSourceDescriptor {
            digest: digest(7),
            sample_rate: 48_000,
            channels: 2,
            frame_count: 100,
        };
        assert_eq!(
            reading.verify_source(Some(&local)),
            Ok(VerificationTier::SourceMatched)
        );
        let mismatch = LocalSourceDescriptor {
            frame_count: 99,
            ..local
        };
        assert!(matches!(
            reading.verify_source(Some(&mismatch)),
            Err(ReadingVerificationRefusal::FrameCount { delta: -1, .. })
        ));
    }

    #[test]
    fn reading_ids_are_fixed_width_and_round_trip_as_hex() {
        let id = ReadingId::new([0xab; 16]).unwrap();
        assert_eq!(id.to_string().parse::<ReadingId>().unwrap(), id);
        assert!("00".parse::<ReadingId>().is_err());
    }
}
