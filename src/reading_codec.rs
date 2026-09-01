//! Lossless JSON envelope codec for portable readings.
//!
//! Unknown envelope keys, section metadata, entire sections, and unclaimed
//! payload members remain `serde_json::Value`s. Typed section helpers always
//! retain the raw section so a newer producer's data is not destroyed by an
//! older reader performing a narrow update.

use std::fmt;

use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::{Map, Value};

use crate::artifact_catalog::sha256_content;
use crate::reading::{PortableDigest, ReadingError, ReadingFile, ReadingSection};

const READING_MANIFEST_DOMAIN: &[u8] = b"audec:reading-manifest:v1";

/// Canonical bytes plus their detached, collision-resistant manifest identity.
///
/// The digest is detached so the reading does not need to hash a field that
/// contains its own hash. It is suitable for a version-chain parent reference,
/// an exchange index, or a transport checksum; source PCM remains absent.
#[derive(Clone, Debug, PartialEq)]
pub struct EncodedReading {
    pub bytes: Vec<u8>,
    pub manifest_digest: PortableDigest,
}

pub fn decode_reading(bytes: &[u8]) -> Result<ReadingFile, ReadingCodecError> {
    let reading = serde_json::from_slice::<ReadingFile>(bytes)
        .map_err(|error| ReadingCodecError::Json(error.to_string()))?;
    reading.validate().map_err(ReadingCodecError::Invalid)?;
    Ok(reading)
}

pub fn encode_reading(reading: &ReadingFile) -> Result<Vec<u8>, ReadingCodecError> {
    reading.validate().map_err(ReadingCodecError::Invalid)?;
    let mut canonical = reading.clone();
    canonical
        .parents
        .sort_by_key(|parent| (parent.reading_id, parent.revision, parent.manifest_digest));
    canonical.source.fingerprints.sort();
    canonical
        .sections
        .sort_by(|left, right| left.name.cmp(&right.name));
    canonical.attachments.sort_by(|left, right| {
        (&left.role, &left.media_type, left.digest).cmp(&(
            &right.role,
            &right.media_type,
            right.digest,
        ))
    });
    let mut bytes = serde_json::to_vec_pretty(&canonical)
        .map_err(|error| ReadingCodecError::Json(error.to_string()))?;
    bytes.push(b'\n');
    Ok(bytes)
}

/// Encode a canonical reading and compute the identity of exactly those
/// canonical bytes. Equivalent readings therefore have the same manifest
/// digest even if an incoming JSON document used different insignificant
/// whitespace or section ordering.
pub fn encode_reading_with_manifest(
    reading: &ReadingFile,
) -> Result<EncodedReading, ReadingCodecError> {
    let bytes = encode_reading(reading)?;
    let manifest_digest =
        PortableDigest::from(sha256_content(READING_MANIFEST_DOMAIN, &[bytes.as_slice()]));
    Ok(EncodedReading {
        bytes,
        manifest_digest,
    })
}

pub fn reading_manifest_digest(reading: &ReadingFile) -> Result<PortableDigest, ReadingCodecError> {
    Ok(encode_reading_with_manifest(reading)?.manifest_digest)
}

/// Decode and validate a reading against an independently supplied manifest
/// identity. Verification is over canonical semantics rather than incidental
/// JSON formatting.
pub fn decode_verified_reading(
    bytes: &[u8],
    expected: PortableDigest,
) -> Result<ReadingFile, ReadingCodecError> {
    if !expected.is_strong() {
        return Err(ReadingCodecError::WeakManifestDigest(expected));
    }
    let reading = decode_reading(bytes)?;
    let actual = reading_manifest_digest(&reading)?;
    if actual != expected {
        return Err(ReadingCodecError::ManifestMismatch { expected, actual });
    }
    Ok(reading)
}

/// Typed view with the untouched source section alongside it. Callers that
/// edit the typed value use [`replace_payload_preserving_unknown`] rather than
/// constructing a fresh section.
#[derive(Clone, Debug, PartialEq)]
pub struct DecodedSection<T> {
    pub value: T,
    pub raw: ReadingSection,
}

pub fn decode_section<T: DeserializeOwned>(
    reading: &ReadingFile,
    name: &str,
    supported_major: u32,
) -> Result<DecodedSection<T>, ReadingCodecError> {
    let section = reading
        .sections
        .iter()
        .find(|section| section.name == name)
        .ok_or_else(|| ReadingCodecError::MissingSection(name.into()))?;
    if section.schema_major != supported_major {
        return Err(ReadingCodecError::UnsupportedSection {
            name: name.into(),
            major: section.schema_major,
        });
    }
    let value = serde_json::from_value(section.payload.clone()).map_err(|error| {
        ReadingCodecError::SectionJson {
            name: name.into(),
            message: error.to_string(),
        }
    })?;
    Ok(DecodedSection {
        value,
        raw: section.clone(),
    })
}

/// Replace known object members while retaining every member the typed
/// producer did not mention. This is intentionally object-only: an opaque
/// array/scalar schema cannot be safely patched by an older build.
pub fn replace_payload_preserving_unknown<T: Serialize>(
    section: &ReadingSection,
    value: &T,
) -> Result<ReadingSection, ReadingCodecError> {
    let new_payload =
        serde_json::to_value(value).map_err(|error| ReadingCodecError::SectionJson {
            name: section.name.clone(),
            message: error.to_string(),
        })?;
    let mut merged = match &section.payload {
        Value::Object(map) => map.clone(),
        _ => {
            return Err(ReadingCodecError::OpaquePayloadCannotBePatched(
                section.name.clone(),
            ))
        }
    };
    let Value::Object(new_members) = new_payload else {
        return Err(ReadingCodecError::TypedPayloadMustBeObject(
            section.name.clone(),
        ));
    };
    merge_object(&mut merged, new_members);
    let mut updated = section.clone();
    updated.payload = Value::Object(merged);
    Ok(updated)
}

fn merge_object(target: &mut Map<String, Value>, replacement: Map<String, Value>) {
    for (key, value) in replacement {
        target.insert(key, value);
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum ReadingCodecError {
    Json(String),
    Invalid(ReadingError),
    MissingSection(String),
    UnsupportedSection {
        name: String,
        major: u32,
    },
    SectionJson {
        name: String,
        message: String,
    },
    OpaquePayloadCannotBePatched(String),
    TypedPayloadMustBeObject(String),
    WeakManifestDigest(PortableDigest),
    ManifestMismatch {
        expected: PortableDigest,
        actual: PortableDigest,
    },
}

impl fmt::Display for ReadingCodecError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "reading codec error: {self:?}")
    }
}

impl std::error::Error for ReadingCodecError {}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde::{Deserialize, Serialize};
    use serde_json::json;

    use super::*;
    use crate::reading::{
        PortableDigest, PortableDigestAlgorithm, ProducerDto, ProvenanceDto, ReadingId,
        ReadingSource, READING_FORMAT, READING_FORMAT_VERSION,
    };

    fn file() -> ReadingFile {
        ReadingFile {
            format: READING_FORMAT.into(),
            version: READING_FORMAT_VERSION,
            reading_id: ReadingId::new([1; 16]).unwrap(),
            revision: 1,
            parents: Vec::new(),
            author: ProvenanceDto {
                producer: ProducerDto::Human { name: None },
                created_unix_ms: None,
                source_revision: None,
                note: None,
            },
            source: ReadingSource {
                fingerprints: vec![PortableDigest {
                    algorithm: PortableDigestAlgorithm::Sha256,
                    bytes: [2; 32],
                }],
                sample_rate: 48_000,
                channels: 2,
                frame_count: 10,
                declared_title: None,
                extensions: BTreeMap::new(),
            },
            sections: vec![ReadingSection {
                name: "comparisons".into(),
                schema_major: 1,
                schema_minor: 7,
                payload: json!({"known": 1, "future": {"shape": "retained"}}),
                extensions: BTreeMap::from([("future_section_meta".into(), json!(true))]),
            }],
            attachments: Vec::new(),
            extensions: BTreeMap::from([("future_envelope".into(), json!([1, 2, 3]))]),
        }
    }

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Known {
        known: u64,
    }

    #[test]
    fn unknown_envelope_and_section_values_survive_round_trip_and_patch() {
        let encoded = encode_reading(&file()).unwrap();
        let decoded = decode_reading(&encoded).unwrap();
        assert_eq!(decoded.extensions["future_envelope"], json!([1, 2, 3]));
        let section = decode_section::<Known>(&decoded, "comparisons", 1).unwrap();
        assert_eq!(section.value, Known { known: 1 });
        let patched =
            replace_payload_preserving_unknown(&section.raw, &Known { known: 9 }).unwrap();
        assert_eq!(patched.payload["known"], 9);
        assert_eq!(patched.payload["future"]["shape"], "retained");
        assert_eq!(patched.extensions["future_section_meta"], true);
    }

    #[test]
    fn manifest_verification_is_canonical_and_refuses_tampering() {
        let encoded = encode_reading_with_manifest(&file()).unwrap();
        let mut reordered = file();
        reordered.sections.push(ReadingSection {
            name: "air".into(),
            schema_major: 1,
            schema_minor: 0,
            payload: json!({"claims": []}),
            extensions: BTreeMap::new(),
        });
        reordered.sections.reverse();
        let reordered = encode_reading_with_manifest(&reordered).unwrap();
        let decoded = decode_verified_reading(&reordered.bytes, reordered.manifest_digest).unwrap();
        assert_eq!(decoded.sections.len(), 2);

        let mut tampered = decoded;
        tampered.sections[0].payload = json!({"claims": ["fabricated"]});
        let tampered_bytes = encode_reading(&tampered).unwrap();
        assert!(matches!(
            decode_verified_reading(&tampered_bytes, reordered.manifest_digest),
            Err(ReadingCodecError::ManifestMismatch { .. })
        ));

        assert_eq!(
            decode_verified_reading(&encoded.bytes, encoded.manifest_digest).unwrap(),
            file()
        );
    }
}
