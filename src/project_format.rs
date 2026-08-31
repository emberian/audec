//! Durable checkpoint contracts for a portable audec project package.
//!
//! This module deliberately does not know about GPUI, [`LiveProject`], or a
//! particular domain codec.  It closes the gap between `project_io`'s JSON
//! envelope and `project_codecs`' in-memory payload map: a checkpoint is one
//! validated envelope plus the exact bytes named by every section.  Unknown
//! sections are opaque, not disposable.  A future build may not edit one, but
//! it must be able to carry it forward unchanged.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Component, Path, PathBuf};

use serde_json::Value;

use crate::project_codecs::DomainPayloads;
use crate::project_io::{DomainSectionRecord, ProjectFile, ProjectIoError};

/// The directory name of the portable manifest inside an `.audec` package.
/// Payload paths in the manifest are relative to this package root.
pub const PACKAGE_MANIFEST_NAME: &str = "project.json";
pub const PACKAGE_PAYLOAD_DIRECTORY: &str = "payloads";
pub const PACKAGE_RECOVERY_DIRECTORY: &str = "recovery";
pub const PACKAGE_JOURNAL_DIRECTORY: &str = "journal";

/// A package root, normally a path ending in `.audec`.  It is a directory so
/// immutable revision-scoped payloads can be published before the small
/// manifest pointer is atomically replaced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectPackage {
    root: PathBuf,
}

impl ProjectPackage {
    pub fn new(root: impl Into<PathBuf>) -> Result<Self, ProjectFormatError> {
        let root = root.into();
        if root.as_os_str().is_empty() {
            return Err(ProjectFormatError::InvalidPackagePath(
                "package root is empty",
            ));
        }
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn manifest_path(&self) -> PathBuf {
        self.root.join(PACKAGE_MANIFEST_NAME)
    }

    pub fn payload_root(&self) -> PathBuf {
        self.root.join(PACKAGE_PAYLOAD_DIRECTORY)
    }

    pub fn recovery_root(&self) -> PathBuf {
        self.root.join(PACKAGE_RECOVERY_DIRECTORY)
    }

    pub fn journal_root(&self) -> PathBuf {
        self.root.join(PACKAGE_JOURNAL_DIRECTORY)
    }

    pub fn payload_path(&self, key: &Path) -> Result<PathBuf, ProjectFormatError> {
        validate_payload_key(key)?;
        Ok(self.root.join(key))
    }
}

/// Explicit provenance for bytes a build does not interpret.  The section
/// descriptor travels with the bytes so a newer domain can be restored by a
/// newer build without renumbering, re-encoding, or silently dropping it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreservedSection {
    pub descriptor: DomainSectionRecord,
    pub bytes: Vec<u8>,
}

/// Data retained during a load/save cycle even when this build cannot edit it.
///
/// Envelope extensions are retained as JSON values.  Unknown sections are
/// retained byte-for-byte.  Known codecs should never place their own section
/// here: a collision is a caller-visible error rather than an arbitrary
/// choice about which authoring state wins.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PreservedProjectData {
    pub envelope_extensions: BTreeMap<String, Value>,
    pub sections: BTreeMap<String, PreservedSection>,
}

impl PreservedProjectData {
    pub fn from_unrecognized(
        file: &ProjectFile,
        payloads: &DomainPayloads,
        recognized_domains: &BTreeSet<String>,
    ) -> Result<Self, ProjectFormatError> {
        let mut sections = BTreeMap::new();
        for descriptor in &file.sections {
            if recognized_domains.contains(&descriptor.domain) {
                continue;
            }
            let bytes = payloads
                .get(&descriptor.payload_key)
                .ok_or_else(|| ProjectFormatError::MissingPayload(descriptor.payload_key.clone()))?
                .to_vec();
            if sections
                .insert(
                    descriptor.domain.clone(),
                    PreservedSection {
                        descriptor: descriptor.clone(),
                        bytes,
                    },
                )
                .is_some()
            {
                return Err(ProjectFormatError::DuplicateSection(
                    descriptor.domain.clone(),
                ));
            }
        }
        Ok(Self {
            envelope_extensions: file.extensions.clone(),
            sections,
        })
    }
}

/// One saveable/recoverable project state before it is written to disk.
#[derive(Clone, Debug, PartialEq)]
pub struct ProjectCheckpoint {
    pub file: ProjectFile,
    pub payloads: DomainPayloads,
    pub preserved: PreservedProjectData,
}

impl ProjectCheckpoint {
    /// Combine domain bytes with retained foreign sections, then ensure the
    /// manifest names exactly one safe payload for every section.
    pub fn new(
        mut file: ProjectFile,
        mut known_payloads: DomainPayloads,
        preserved: PreservedProjectData,
    ) -> Result<Self, ProjectFormatError> {
        for (key, value) in &preserved.envelope_extensions {
            match file.extensions.get(key) {
                None => {
                    file.extensions.insert(key.clone(), value.clone());
                }
                Some(existing) if existing == value => {}
                Some(_) => return Err(ProjectFormatError::ExtensionCollision(key.clone())),
            }
        }
        let known_domains = file
            .sections
            .iter()
            .map(|section| section.domain.clone())
            .collect::<BTreeSet<_>>();
        for (domain, section) in &preserved.sections {
            if known_domains.contains(domain) {
                return Err(ProjectFormatError::PreservedDomainCollision(domain.clone()));
            }
            if known_payloads
                .0
                .insert(
                    section.descriptor.payload_key.clone(),
                    section.bytes.clone(),
                )
                .is_some()
            {
                return Err(ProjectFormatError::PayloadCollision(
                    section.descriptor.payload_key.clone(),
                ));
            }
            file.sections.push(section.descriptor.clone());
        }
        file.sections
            .sort_by(|left, right| left.domain.cmp(&right.domain));
        let checkpoint = Self {
            file,
            payloads: known_payloads,
            preserved,
        };
        checkpoint.validate()?;
        Ok(checkpoint)
    }

    pub fn revision(&self) -> u64 {
        self.file.aggregate_revision
    }

    pub fn validate(&self) -> Result<(), ProjectFormatError> {
        self.file.validate().map_err(ProjectFormatError::Envelope)?;
        let mut domains = BTreeSet::new();
        let mut payload_keys = BTreeSet::new();
        for section in &self.file.sections {
            if !domains.insert(&section.domain) {
                return Err(ProjectFormatError::DuplicateSection(section.domain.clone()));
            }
            validate_payload_key(&section.payload_key)?;
            if !payload_keys.insert(&section.payload_key) {
                return Err(ProjectFormatError::PayloadCollision(
                    section.payload_key.clone(),
                ));
            }
            if self.payloads.get(&section.payload_key).is_none() {
                return Err(ProjectFormatError::MissingPayload(
                    section.payload_key.clone(),
                ));
            }
        }
        for key in self.payloads.0.keys() {
            validate_payload_key(key)?;
            if !payload_keys.contains(key) {
                return Err(ProjectFormatError::UnreferencedPayload(key.clone()));
            }
        }
        Ok(())
    }

    /// Allocate revision-scoped immutable payload names.  The manifest is the
    /// sole mutable pointer: after all these bytes are durable, replacing it
    /// atomically publishes a coherent checkpoint.
    pub fn revision_scoped(&self) -> Result<Self, ProjectFormatError> {
        let revision = self.revision();
        let mut file = self.file.clone();
        let mut payloads = BTreeMap::new();
        for section in &mut file.sections {
            let old_key = section.payload_key.clone();
            let leaf = old_key
                .file_name()
                .ok_or_else(|| ProjectFormatError::InvalidPayloadKey(old_key.clone()))?;
            let new_key = PathBuf::from(PACKAGE_PAYLOAD_DIRECTORY)
                .join(format!("r{revision}"))
                .join(leaf);
            let bytes = self
                .payloads
                .get(&old_key)
                .ok_or_else(|| ProjectFormatError::MissingPayload(old_key.clone()))?
                .to_vec();
            if payloads.insert(new_key.clone(), bytes).is_some() {
                return Err(ProjectFormatError::PayloadCollision(new_key));
            }
            section.payload_key = new_key;
        }
        Self::new(
            file,
            DomainPayloads(payloads),
            PreservedProjectData::default(),
        )
    }
}

pub fn validate_payload_key(key: &Path) -> Result<(), ProjectFormatError> {
    let valid = !key.as_os_str().is_empty()
        && !key.is_absolute()
        && !key.components().any(|part| {
            matches!(
                part,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        });
    if valid {
        Ok(())
    } else {
        Err(ProjectFormatError::InvalidPayloadKey(key.to_path_buf()))
    }
}

#[derive(Debug)]
pub enum ProjectFormatError {
    InvalidPackagePath(&'static str),
    Envelope(ProjectIoError),
    InvalidPayloadKey(PathBuf),
    MissingPayload(PathBuf),
    UnreferencedPayload(PathBuf),
    PayloadCollision(PathBuf),
    DuplicateSection(String),
    PreservedDomainCollision(String),
    ExtensionCollision(String),
}

impl fmt::Display for ProjectFormatError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidPackagePath(message) => formatter.write_str(message),
            Self::Envelope(error) => write!(formatter, "invalid project envelope: {error}"),
            Self::InvalidPayloadKey(key) => {
                write!(
                    formatter,
                    "payload key is not package-relative: {}",
                    key.display()
                )
            }
            Self::MissingPayload(key) => write!(formatter, "missing payload: {}", key.display()),
            Self::UnreferencedPayload(key) => {
                write!(
                    formatter,
                    "payload is not named by the envelope: {}",
                    key.display()
                )
            }
            Self::PayloadCollision(key) => {
                write!(
                    formatter,
                    "more than one section owns payload {}",
                    key.display()
                )
            }
            Self::DuplicateSection(domain) => write!(formatter, "duplicate section {domain}"),
            Self::PreservedDomainCollision(domain) => {
                write!(
                    formatter,
                    "preserved section collides with a known {domain} section"
                )
            }
            Self::ExtensionCollision(key) => {
                write!(
                    formatter,
                    "preserved extension {key} conflicts with a new value"
                )
            }
        }
    }
}

impl std::error::Error for ProjectFormatError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Envelope(error) => Some(error),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project_io::{PROJECT_FILE_FORMAT, PROJECT_FILE_VERSION};

    fn section(domain: &str, key: &str) -> DomainSectionRecord {
        DomainSectionRecord {
            domain: domain.into(),
            schema_version: 1,
            revision: 7,
            payload_key: key.into(),
            encoding: "json".into(),
        }
    }

    fn file(sections: Vec<DomainSectionRecord>) -> ProjectFile {
        ProjectFile {
            format: PROJECT_FILE_FORMAT.into(),
            version: PROJECT_FILE_VERSION,
            project_name: "checkpoint".into(),
            aggregate_revision: 7,
            sections,
            bindings: Vec::new(),
            assets: Vec::new(),
            workspace: None,
            recovery: Default::default(),
            extensions: BTreeMap::new(),
        }
    }

    #[test]
    fn preserves_unknown_section_bytes_when_a_known_manifest_is_rebuilt() {
        let loaded = file(vec![
            section("arrangement", "arrangement.json"),
            section("vendor", "vendor.bin"),
        ]);
        let payloads = DomainPayloads(BTreeMap::from([
            (
                PathBuf::from("arrangement.json"),
                br#"{\"schema_version\":1}"#.to_vec(),
            ),
            (PathBuf::from("vendor.bin"), vec![0, 255, 7]),
        ]));
        let preserved = PreservedProjectData::from_unrecognized(
            &loaded,
            &payloads,
            &BTreeSet::from(["arrangement".into()]),
        )
        .unwrap();
        let rebuilt = ProjectCheckpoint::new(
            file(vec![section("arrangement", "arrangement.json")]),
            DomainPayloads(BTreeMap::from([(
                PathBuf::from("arrangement.json"),
                br#"{\"schema_version\":2}"#.to_vec(),
            )])),
            preserved,
        )
        .unwrap();
        assert_eq!(
            rebuilt.payloads.get(Path::new("vendor.bin")),
            Some(&[0, 255, 7][..])
        );
        assert!(rebuilt
            .file
            .sections
            .iter()
            .any(|section| section.domain == "vendor"));
    }

    #[test]
    fn revision_scoping_makes_manifest_the_only_mutable_pointer() {
        let checkpoint = ProjectCheckpoint::new(
            file(vec![section("arrangement", "arrangement.json")]),
            DomainPayloads(BTreeMap::from([(
                PathBuf::from("arrangement.json"),
                vec![1, 2, 3],
            )])),
            Default::default(),
        )
        .unwrap();
        let scoped = checkpoint.revision_scoped().unwrap();
        assert_eq!(
            scoped.file.sections[0].payload_key,
            PathBuf::from("payloads/r7/arrangement.json")
        );
        assert_eq!(
            scoped
                .payloads
                .get(Path::new("payloads/r7/arrangement.json")),
            Some(&[1, 2, 3][..])
        );
    }
}
