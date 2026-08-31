//! Versioned, deterministic project-manifest persistence.
//!
//! The audio, analysis tiles, and editable AIR can be large, so the project
//! file is a compact manifest which names content-addressed artifacts.  The
//! codec is deliberately dependency-free and record based: unknown records
//! survive a read/write cycle, while a newer schema version is rejected
//! explicitly instead of being guessed at.

use std::collections::BTreeMap;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, Cursor, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const MAGIC: &[u8; 8] = b"AUDECPJ\0";
pub const PROJECT_SCHEMA_VERSION: u32 = 1;
const MAX_RECORD_BYTES: usize = 64 * 1024 * 1024;
const MAX_RECORDS: usize = 1_000_000;

const PROJECT_NAME: u16 = 1;
const MATERIAL: u16 = 2;
const ACTIVE_WORKSPACE: u16 = 3;
const WORKSPACE: u16 = 4;
const ARTIFACT: u16 = 5;

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MaterialReference {
    /// The path as chosen by the user. Relative paths remain relocatable.
    pub path: PathBuf,
    /// Algorithm-prefixed digest, for example `blake3:…`.
    pub content_hash: String,
    pub sample_rate: u32,
    pub channels: u16,
    pub frames: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct WindowGeometry {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct LensRecord {
    pub id: String,
    pub kind: String,
    pub floating: bool,
    pub geometry: WindowGeometry,
    pub visible_start_sample: u64,
    pub visible_end_sample: u64,
    /// Lens-specific values are stable strings so older audec builds can
    /// preserve controls which they do not understand.
    pub settings: BTreeMap<String, String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct WorkspaceRecord {
    pub id: String,
    pub title: String,
    /// A versioned layout expression owned by the workspace implementation.
    pub layout: String,
    pub lenses: Vec<LensRecord>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArtifactRecord {
    pub kind: String,
    pub relative_path: PathBuf,
    pub content_hash: String,
    /// Exact algorithm/model/config revision which authored the artifact.
    pub producer_revision: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpaqueRecord {
    pub tag: u16,
    pub payload: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ProjectManifest {
    pub project_name: String,
    pub material: MaterialReference,
    pub active_workspace: String,
    pub workspaces: Vec<WorkspaceRecord>,
    pub artifacts: Vec<ArtifactRecord>,
    /// Future top-level records preserved byte-for-byte by this schema.
    pub opaque_records: Vec<OpaqueRecord>,
}

impl ProjectManifest {
    pub fn validate(&self) -> Result<(), PersistError> {
        if self.project_name.trim().is_empty() {
            return Err(PersistError::Invalid("project name is empty".into()));
        }
        if self.material.path.as_os_str().is_empty() {
            return Err(PersistError::Invalid("material path is empty".into()));
        }
        if self.material.content_hash.trim().is_empty() {
            return Err(PersistError::Invalid(
                "material content hash is empty".into(),
            ));
        }
        if self.material.sample_rate == 0 || self.material.channels == 0 {
            return Err(PersistError::Invalid(
                "material sample rate and channels must be non-zero".into(),
            ));
        }

        let mut workspace_ids = BTreeMap::new();
        for workspace in &self.workspaces {
            if workspace.id.is_empty() {
                return Err(PersistError::Invalid("workspace id is empty".into()));
            }
            if workspace_ids.insert(&workspace.id, ()).is_some() {
                return Err(PersistError::Invalid(format!(
                    "duplicate workspace id {}",
                    workspace.id
                )));
            }
            let mut lens_ids = BTreeMap::new();
            for lens in &workspace.lenses {
                if lens.id.is_empty() || lens.kind.is_empty() {
                    return Err(PersistError::Invalid(
                        "lens id and kind must not be empty".into(),
                    ));
                }
                if lens_ids.insert(&lens.id, ()).is_some() {
                    return Err(PersistError::Invalid(format!(
                        "duplicate lens id {} in workspace {}",
                        lens.id, workspace.id
                    )));
                }
                if lens.visible_start_sample > lens.visible_end_sample {
                    return Err(PersistError::Invalid(format!(
                        "lens {} has a reversed visible range",
                        lens.id
                    )));
                }
                let geometry = lens.geometry;
                if ![geometry.x, geometry.y, geometry.width, geometry.height]
                    .into_iter()
                    .all(f32::is_finite)
                    || geometry.width < 0.0
                    || geometry.height < 0.0
                {
                    return Err(PersistError::Invalid(format!(
                        "lens {} has invalid window geometry",
                        lens.id
                    )));
                }
            }
        }
        if !self.active_workspace.is_empty() && !workspace_ids.contains_key(&self.active_workspace)
        {
            return Err(PersistError::Invalid(format!(
                "active workspace {} does not exist",
                self.active_workspace
            )));
        }
        for artifact in &self.artifacts {
            if artifact.kind.is_empty()
                || artifact.relative_path.as_os_str().is_empty()
                || artifact.content_hash.is_empty()
            {
                return Err(PersistError::Invalid(
                    "artifact kind, path, and hash must not be empty".into(),
                ));
            }
            if artifact.relative_path.is_absolute() {
                return Err(PersistError::Invalid(format!(
                    "artifact path must be project-relative: {}",
                    artifact.relative_path.display()
                )));
            }
        }
        Ok(())
    }

    pub fn encode(&self) -> Result<Vec<u8>, PersistError> {
        self.validate()?;
        let mut records = Vec::new();
        records.push(Record::new(PROJECT_NAME, string_bytes(&self.project_name)?));
        records.push(Record::new(MATERIAL, encode_material(&self.material)?));
        records.push(Record::new(
            ACTIVE_WORKSPACE,
            string_bytes(&self.active_workspace)?,
        ));

        let mut workspaces: Vec<_> = self.workspaces.iter().collect();
        workspaces.sort_by(|left, right| left.id.cmp(&right.id));
        for workspace in workspaces {
            records.push(Record::new(WORKSPACE, encode_workspace(workspace)?));
        }
        let mut artifacts: Vec<_> = self.artifacts.iter().collect();
        artifacts.sort_by(|left, right| {
            (&left.kind, &left.relative_path, &left.content_hash).cmp(&(
                &right.kind,
                &right.relative_path,
                &right.content_hash,
            ))
        });
        for artifact in artifacts {
            records.push(Record::new(ARTIFACT, encode_artifact(artifact)?));
        }
        let mut opaque = self.opaque_records.clone();
        opaque.sort_by(|left, right| (left.tag, &left.payload).cmp(&(right.tag, &right.payload)));
        records.extend(opaque.into_iter().map(|record| Record {
            tag: record.tag,
            payload: record.payload,
        }));

        let mut output = Vec::new();
        output.extend_from_slice(MAGIC);
        put_u32(&mut output, PROJECT_SCHEMA_VERSION);
        put_u32(&mut output, checked_u32(records.len(), "record count")?);
        for record in records {
            put_u16(&mut output, record.tag);
            put_u32(
                &mut output,
                checked_u32(record.payload.len(), "record length")?,
            );
            output.extend_from_slice(&record.payload);
        }
        Ok(output)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, PersistError> {
        let mut input = Cursor::new(bytes);
        let mut magic = [0_u8; 8];
        input.read_exact(&mut magic)?;
        if &magic != MAGIC {
            return Err(PersistError::Corrupt("not an audec project".into()));
        }
        let version = read_u32(&mut input)?;
        if version > PROJECT_SCHEMA_VERSION {
            return Err(PersistError::NewerVersion {
                found: version,
                supported: PROJECT_SCHEMA_VERSION,
            });
        }
        if version == 0 {
            return Err(PersistError::Corrupt("schema version zero".into()));
        }
        let count = read_u32(&mut input)? as usize;
        if count > MAX_RECORDS {
            return Err(PersistError::Corrupt("implausible record count".into()));
        }

        let mut name = None;
        let mut material = None;
        let mut active_workspace = None;
        let mut workspaces = Vec::new();
        let mut artifacts = Vec::new();
        let mut opaque_records = Vec::new();
        for _ in 0..count {
            let tag = read_u16(&mut input)?;
            let length = read_u32(&mut input)? as usize;
            if length > MAX_RECORD_BYTES {
                return Err(PersistError::Corrupt("record is too large".into()));
            }
            let mut payload = vec![0_u8; length];
            input.read_exact(&mut payload)?;
            match tag {
                PROJECT_NAME => set_once(&mut name, decode_string(&payload)?, "project name")?,
                MATERIAL => set_once(&mut material, decode_material(&payload)?, "material")?,
                ACTIVE_WORKSPACE => set_once(
                    &mut active_workspace,
                    decode_string(&payload)?,
                    "active workspace",
                )?,
                WORKSPACE => workspaces.push(decode_workspace(&payload)?),
                ARTIFACT => artifacts.push(decode_artifact(&payload)?),
                _ => opaque_records.push(OpaqueRecord { tag, payload }),
            }
        }
        if input.position() as usize != bytes.len() {
            return Err(PersistError::Corrupt(
                "trailing bytes after final record".into(),
            ));
        }
        let manifest = Self {
            project_name: name
                .ok_or_else(|| PersistError::Corrupt("missing project name".into()))?,
            material: material.ok_or_else(|| PersistError::Corrupt("missing material".into()))?,
            active_workspace: active_workspace.unwrap_or_default(),
            workspaces,
            artifacts,
            opaque_records,
        };
        manifest.validate()?;
        Ok(manifest)
    }

    pub fn load(path: &Path) -> Result<Self, PersistError> {
        Self::decode(&fs::read(path)?)
    }

    /// Write beside the destination, flush it, then atomically replace the
    /// manifest. A failed write leaves the previous project file untouched.
    pub fn save_atomic(&self, path: &Path) -> Result<(), PersistError> {
        let bytes = self.encode()?;
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| PersistError::Invalid("project path has no UTF-8 file name".into()))?;
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let temporary = parent.join(format!(
            ".{file_name}.audec-tmp-{}-{sequence}",
            std::process::id()
        ));

        let result = (|| -> Result<(), PersistError> {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            drop(file);
            fs::rename(&temporary, path)?;
            if let Ok(directory) = OpenOptions::new().read(true).open(parent) {
                let _ = directory.sync_all();
            }
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }
}

#[derive(Debug)]
pub enum PersistError {
    Io(io::Error),
    Corrupt(String),
    Invalid(String),
    NewerVersion { found: u32, supported: u32 },
}

impl fmt::Display for PersistError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "project I/O failed: {error}"),
            Self::Corrupt(message) => write!(formatter, "project file is corrupt: {message}"),
            Self::Invalid(message) => write!(formatter, "project is invalid: {message}"),
            Self::NewerVersion { found, supported } => write!(
                formatter,
                "project schema {found} is newer than supported schema {supported}"
            ),
        }
    }
}

impl std::error::Error for PersistError {}

impl From<io::Error> for PersistError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

struct Record {
    tag: u16,
    payload: Vec<u8>,
}

impl Record {
    fn new(tag: u16, payload: Vec<u8>) -> Self {
        Self { tag, payload }
    }
}

fn encode_material(material: &MaterialReference) -> Result<Vec<u8>, PersistError> {
    let mut output = Vec::new();
    put_string(&mut output, &material.path.to_string_lossy())?;
    put_string(&mut output, &material.content_hash)?;
    put_u32(&mut output, material.sample_rate);
    put_u16(&mut output, material.channels);
    put_u64(&mut output, material.frames);
    Ok(output)
}

fn decode_material(bytes: &[u8]) -> Result<MaterialReference, PersistError> {
    let mut input = Cursor::new(bytes);
    let value = MaterialReference {
        path: PathBuf::from(read_string(&mut input)?),
        content_hash: read_string(&mut input)?,
        sample_rate: read_u32(&mut input)?,
        channels: read_u16(&mut input)?,
        frames: read_u64(&mut input)?,
    };
    finish_nested(&input, bytes)?;
    Ok(value)
}

fn encode_workspace(workspace: &WorkspaceRecord) -> Result<Vec<u8>, PersistError> {
    let mut output = Vec::new();
    put_string(&mut output, &workspace.id)?;
    put_string(&mut output, &workspace.title)?;
    put_string(&mut output, &workspace.layout)?;
    let mut lenses: Vec<_> = workspace.lenses.iter().collect();
    lenses.sort_by(|left, right| left.id.cmp(&right.id));
    put_u32(&mut output, checked_u32(lenses.len(), "lens count")?);
    for lens in lenses {
        let encoded = encode_lens(lens)?;
        put_bytes(&mut output, &encoded)?;
    }
    Ok(output)
}

fn decode_workspace(bytes: &[u8]) -> Result<WorkspaceRecord, PersistError> {
    let mut input = Cursor::new(bytes);
    let id = read_string(&mut input)?;
    let title = read_string(&mut input)?;
    let layout = read_string(&mut input)?;
    let count = read_count(&mut input, "lens")?;
    let mut lenses = Vec::with_capacity(count);
    for _ in 0..count {
        lenses.push(decode_lens(&read_bytes(&mut input)?)?);
    }
    finish_nested(&input, bytes)?;
    Ok(WorkspaceRecord {
        id,
        title,
        layout,
        lenses,
    })
}

fn encode_lens(lens: &LensRecord) -> Result<Vec<u8>, PersistError> {
    let mut output = Vec::new();
    put_string(&mut output, &lens.id)?;
    put_string(&mut output, &lens.kind)?;
    output.push(u8::from(lens.floating));
    for value in [
        lens.geometry.x,
        lens.geometry.y,
        lens.geometry.width,
        lens.geometry.height,
    ] {
        put_u32(&mut output, value.to_bits());
    }
    put_u64(&mut output, lens.visible_start_sample);
    put_u64(&mut output, lens.visible_end_sample);
    put_u32(
        &mut output,
        checked_u32(lens.settings.len(), "lens setting count")?,
    );
    for (key, value) in &lens.settings {
        put_string(&mut output, key)?;
        put_string(&mut output, value)?;
    }
    Ok(output)
}

fn decode_lens(bytes: &[u8]) -> Result<LensRecord, PersistError> {
    let mut input = Cursor::new(bytes);
    let id = read_string(&mut input)?;
    let kind = read_string(&mut input)?;
    let floating = match read_u8(&mut input)? {
        0 => false,
        1 => true,
        _ => return Err(PersistError::Corrupt("invalid lens floating flag".into())),
    };
    let geometry = WindowGeometry {
        x: f32::from_bits(read_u32(&mut input)?),
        y: f32::from_bits(read_u32(&mut input)?),
        width: f32::from_bits(read_u32(&mut input)?),
        height: f32::from_bits(read_u32(&mut input)?),
    };
    let visible_start_sample = read_u64(&mut input)?;
    let visible_end_sample = read_u64(&mut input)?;
    let count = read_count(&mut input, "lens setting")?;
    let mut settings = BTreeMap::new();
    for _ in 0..count {
        let key = read_string(&mut input)?;
        let value = read_string(&mut input)?;
        if settings.insert(key.clone(), value).is_some() {
            return Err(PersistError::Corrupt(format!(
                "duplicate lens setting {key}"
            )));
        }
    }
    finish_nested(&input, bytes)?;
    Ok(LensRecord {
        id,
        kind,
        floating,
        geometry,
        visible_start_sample,
        visible_end_sample,
        settings,
    })
}

fn encode_artifact(artifact: &ArtifactRecord) -> Result<Vec<u8>, PersistError> {
    let mut output = Vec::new();
    put_string(&mut output, &artifact.kind)?;
    put_string(&mut output, &artifact.relative_path.to_string_lossy())?;
    put_string(&mut output, &artifact.content_hash)?;
    put_string(&mut output, &artifact.producer_revision)?;
    Ok(output)
}

fn decode_artifact(bytes: &[u8]) -> Result<ArtifactRecord, PersistError> {
    let mut input = Cursor::new(bytes);
    let value = ArtifactRecord {
        kind: read_string(&mut input)?,
        relative_path: PathBuf::from(read_string(&mut input)?),
        content_hash: read_string(&mut input)?,
        producer_revision: read_string(&mut input)?,
    };
    finish_nested(&input, bytes)?;
    Ok(value)
}

fn set_once<T>(slot: &mut Option<T>, value: T, name: &str) -> Result<(), PersistError> {
    if slot.replace(value).is_some() {
        return Err(PersistError::Corrupt(format!("duplicate {name} record")));
    }
    Ok(())
}

fn string_bytes(value: &str) -> Result<Vec<u8>, PersistError> {
    if value.len() > MAX_RECORD_BYTES {
        return Err(PersistError::Invalid("string is too large".into()));
    }
    Ok(value.as_bytes().to_vec())
}

fn decode_string(bytes: &[u8]) -> Result<String, PersistError> {
    String::from_utf8(bytes.to_vec())
        .map_err(|_| PersistError::Corrupt("invalid UTF-8 string".into()))
}

fn put_string(output: &mut Vec<u8>, value: &str) -> Result<(), PersistError> {
    put_bytes(output, value.as_bytes())
}

fn read_string(input: &mut Cursor<&[u8]>) -> Result<String, PersistError> {
    decode_string(&read_bytes(input)?)
}

fn put_bytes(output: &mut Vec<u8>, bytes: &[u8]) -> Result<(), PersistError> {
    put_u32(output, checked_u32(bytes.len(), "byte field length")?);
    output.extend_from_slice(bytes);
    Ok(())
}

fn read_bytes(input: &mut Cursor<&[u8]>) -> Result<Vec<u8>, PersistError> {
    let length = read_u32(input)? as usize;
    if length > MAX_RECORD_BYTES {
        return Err(PersistError::Corrupt("nested field is too large".into()));
    }
    let mut bytes = vec![0_u8; length];
    input.read_exact(&mut bytes)?;
    Ok(bytes)
}

fn read_count(input: &mut Cursor<&[u8]>, label: &str) -> Result<usize, PersistError> {
    let count = read_u32(input)? as usize;
    if count > MAX_RECORDS {
        return Err(PersistError::Corrupt(format!("implausible {label} count")));
    }
    Ok(count)
}

fn finish_nested(input: &Cursor<&[u8]>, bytes: &[u8]) -> Result<(), PersistError> {
    if input.position() as usize == bytes.len() {
        Ok(())
    } else {
        Err(PersistError::Corrupt(
            "trailing bytes in nested record".into(),
        ))
    }
}

fn checked_u32(value: usize, label: &str) -> Result<u32, PersistError> {
    u32::try_from(value)
        .map_err(|_| PersistError::Invalid(format!("{label} exceeds the project format")))
}

fn put_u16(output: &mut Vec<u8>, value: u16) {
    output.extend_from_slice(&value.to_le_bytes());
}

fn put_u32(output: &mut Vec<u8>, value: u32) {
    output.extend_from_slice(&value.to_le_bytes());
}

fn put_u64(output: &mut Vec<u8>, value: u64) {
    output.extend_from_slice(&value.to_le_bytes());
}

fn read_u8(input: &mut Cursor<&[u8]>) -> Result<u8, PersistError> {
    let mut bytes = [0_u8; 1];
    input.read_exact(&mut bytes)?;
    Ok(bytes[0])
}

fn read_u16(input: &mut Cursor<&[u8]>) -> Result<u16, PersistError> {
    let mut bytes = [0_u8; 2];
    input.read_exact(&mut bytes)?;
    Ok(u16::from_le_bytes(bytes))
}

fn read_u32(input: &mut Cursor<&[u8]>) -> Result<u32, PersistError> {
    let mut bytes = [0_u8; 4];
    input.read_exact(&mut bytes)?;
    Ok(u32::from_le_bytes(bytes))
}

fn read_u64(input: &mut Cursor<&[u8]>) -> Result<u64, PersistError> {
    let mut bytes = [0_u8; 8];
    input.read_exact(&mut bytes)?;
    Ok(u64::from_le_bytes(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn example() -> ProjectManifest {
        let mut settings = BTreeMap::new();
        settings.insert("fft_size".into(), "8192".into());
        settings.insert("window".into(), "hann".into());
        ProjectManifest {
            project_name: "Silent Shout study".into(),
            material: MaterialReference {
                path: PathBuf::from("audio/Silent Shout.flac"),
                content_hash: "blake3:0123456789abcdef".into(),
                sample_rate: 44_100,
                channels: 2,
                frames: 12_345_678,
            },
            active_workspace: "decompile".into(),
            workspaces: vec![WorkspaceRecord {
                id: "decompile".into(),
                title: "Decompile".into(),
                layout: "split(h:0.67,timeline,lenses)".into(),
                lenses: vec![LensRecord {
                    id: "waterfall-1".into(),
                    kind: "waterfall".into(),
                    floating: true,
                    geometry: WindowGeometry {
                        x: 40.0,
                        y: 80.0,
                        width: 960.0,
                        height: 540.0,
                    },
                    visible_start_sample: 44_100,
                    visible_end_sample: 220_500,
                    settings,
                }],
            }],
            artifacts: vec![ArtifactRecord {
                kind: "spectral-tiles".into(),
                relative_path: PathBuf::from("artifacts/stft-01.bin"),
                content_hash: "blake3:feedface".into(),
                producer_revision: "audec-stft-v2;fft=8192;hop=2048".into(),
            }],
            opaque_records: vec![OpaqueRecord {
                tag: 0x8001,
                payload: vec![4, 8, 15, 16, 23, 42],
            }],
        }
    }

    #[test]
    fn deterministic_roundtrip_preserves_unknown_records() {
        let manifest = example();
        let first = manifest.encode().unwrap();
        let decoded = ProjectManifest::decode(&first).unwrap();
        let second = decoded.encode().unwrap();
        assert_eq!(decoded, manifest);
        assert_eq!(first, second);
    }

    #[test]
    fn collection_order_does_not_change_encoded_project() {
        let first = example();
        let mut second = first.clone();
        second.artifacts.push(ArtifactRecord {
            kind: "air".into(),
            relative_path: "artifacts/air-01.bin".into(),
            content_hash: "blake3:aa".into(),
            producer_revision: "air-v1".into(),
        });
        let mut third = second.clone();
        third.artifacts.reverse();
        assert_eq!(second.encode().unwrap(), third.encode().unwrap());
    }

    #[test]
    fn truncated_and_trailing_files_are_rejected() {
        let bytes = example().encode().unwrap();
        for length in [0, 7, 8, 12, bytes.len() - 1] {
            assert!(ProjectManifest::decode(&bytes[..length]).is_err());
        }
        let mut trailing = bytes;
        trailing.push(0);
        assert!(matches!(
            ProjectManifest::decode(&trailing),
            Err(PersistError::Corrupt(_))
        ));
    }

    #[test]
    fn newer_versions_fail_explicitly() {
        let mut bytes = example().encode().unwrap();
        bytes[8..12].copy_from_slice(&(PROJECT_SCHEMA_VERSION + 1).to_le_bytes());
        assert!(matches!(
            ProjectManifest::decode(&bytes),
            Err(PersistError::NewerVersion { .. })
        ));
    }

    #[test]
    fn invalid_references_and_layout_state_are_rejected() {
        let mut manifest = example();
        manifest.artifacts[0].relative_path = PathBuf::from("/tmp/not-relocatable.bin");
        assert!(manifest.validate().is_err());

        let mut manifest = example();
        manifest.workspaces[0].lenses[0].geometry.width = f32::NAN;
        assert!(manifest.validate().is_err());

        let mut manifest = example();
        manifest.active_workspace = "missing".into();
        assert!(manifest.validate().is_err());
    }

    #[test]
    fn atomic_save_replaces_a_complete_file_and_cleans_temporary() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory =
            std::env::temp_dir().join(format!("audec-persistence-{}-{nonce}", std::process::id()));
        fs::create_dir(&directory).unwrap();
        let path = directory.join("study.audec");

        let first = example();
        first.save_atomic(&path).unwrap();
        assert_eq!(ProjectManifest::load(&path).unwrap(), first);

        let mut second = first.clone();
        second.project_name = "Like a Pen study".into();
        second.save_atomic(&path).unwrap();
        assert_eq!(ProjectManifest::load(&path).unwrap(), second);
        assert_eq!(fs::read_dir(&directory).unwrap().count(), 1);

        fs::remove_file(path).unwrap();
        fs::remove_dir(directory).unwrap();
    }
}
