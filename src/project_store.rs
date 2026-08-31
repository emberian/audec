//! Atomic package storage for [`crate::project_format::ProjectCheckpoint`].
//!
//! Payload files are immutable and revision-scoped.  Saving first makes every
//! referenced payload durable, then atomically replaces the compact manifest
//! pointer.  A crash can therefore leave unreferenced payloads behind, but it
//! cannot publish a manifest that points at a half-written checkpoint.

use std::error::Error;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::project_codecs::DomainPayloads;
use crate::project_format::{ProjectCheckpoint, ProjectFormatError, ProjectPackage};
use crate::project_io::{self, ProjectFile, ProjectIoDiagnostic, RecoveryMetadata};

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CheckpointKind {
    Primary,
    Autosave,
}

/// A revision token carried across background I/O.  The document controller
/// compares it to its current revision before marking the live document saved;
/// this prevents a save of N from incorrectly clearing dirty state for N+1.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SaveRevisionGuard {
    pub revision: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SaveResult {
    pub kind: CheckpointKind,
    pub manifest_path: PathBuf,
    pub revision_guard: SaveRevisionGuard,
    pub payloads_published: usize,
}

#[derive(Clone, Debug)]
pub struct LoadedCheckpoint {
    pub checkpoint: ProjectCheckpoint,
    pub diagnostics: Vec<ProjectIoDiagnostic>,
    pub manifest_path: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecoveryCheckpoint {
    pub manifest_path: PathBuf,
    pub saved_unix_ms: u64,
    pub base_project_revision: u64,
}

#[derive(Clone, Debug, Default)]
pub struct RecoveryDiscovery {
    pub primary: Option<PathBuf>,
    pub checkpoints: Vec<RecoveryCheckpoint>,
    /// Journal segments are deliberately discovered but not interpreted here.
    /// ENVELOPE owns their frame codec; storage only keeps their bytes and
    /// makes recoverability visible to the document controller.
    pub journals: Vec<PathBuf>,
    pub diagnostics: Vec<StoreDiagnostic>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoreDiagnostic {
    pub path: PathBuf,
    pub code: &'static str,
    pub message: String,
}

/// UI-independent filesystem service for one directory-backed project
/// package.  It does no mutation of live project state.
#[derive(Clone, Debug)]
pub struct ProjectStore {
    package: ProjectPackage,
}

impl ProjectStore {
    pub fn new(package: ProjectPackage) -> Self {
        Self { package }
    }

    pub fn package(&self) -> &ProjectPackage {
        &self.package
    }

    /// Publish a manual checkpoint.  The caller supplies the revision it
    /// snapshotted; a mismatch catches wiring errors before disk is touched.
    pub fn save_primary(
        &self,
        checkpoint: &ProjectCheckpoint,
        expected_revision: u64,
    ) -> Result<SaveResult, ProjectStoreError> {
        self.save(checkpoint, expected_revision, CheckpointKind::Primary, None)
    }

    /// Publish a separately labelled recovery checkpoint.  It references the
    /// same immutable payloads as the checkpoint and is never promoted over a
    /// primary without an explicit document-controller action.
    pub fn save_autosave(
        &self,
        checkpoint: &ProjectCheckpoint,
        expected_revision: u64,
        saved_unix_ms: u64,
    ) -> Result<SaveResult, ProjectStoreError> {
        self.save(
            checkpoint,
            expected_revision,
            CheckpointKind::Autosave,
            Some(saved_unix_ms),
        )
    }

    pub fn load_primary(&self) -> Result<LoadedCheckpoint, ProjectStoreError> {
        self.load_manifest(&self.package.manifest_path())
    }

    pub fn load_recovery(
        &self,
        candidate: &RecoveryCheckpoint,
    ) -> Result<LoadedCheckpoint, ProjectStoreError> {
        self.load_manifest(&candidate.manifest_path)
    }

    /// Scan recovery checkpoints and opaque journal segments without treating
    /// malformed recovery data as a failure to open the known-good primary.
    pub fn discover_recovery(&self) -> RecoveryDiscovery {
        let mut discovery = RecoveryDiscovery::default();
        let primary = self.package.manifest_path();
        if primary.is_file() {
            discovery.primary = Some(primary);
        }

        let recovery_root = self.package.recovery_root();
        match fs::read_dir(&recovery_root) {
            Ok(entries) => {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
                        continue;
                    }
                    match read_file(&path) {
                        Ok((file, _)) if file.recovery.is_autosave => {
                            discovery.checkpoints.push(RecoveryCheckpoint {
                                manifest_path: path,
                                saved_unix_ms: file.recovery.saved_unix_ms,
                                base_project_revision: file.recovery.base_project_revision,
                            });
                        }
                        Ok(_) => discovery.diagnostics.push(StoreDiagnostic {
                            path,
                            code: "not-autosave",
                            message: "recovery manifest does not identify itself as an autosave"
                                .into(),
                        }),
                        Err(error) => discovery.diagnostics.push(StoreDiagnostic {
                            path,
                            code: "invalid-recovery",
                            message: error.to_string(),
                        }),
                    }
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => discovery.diagnostics.push(StoreDiagnostic {
                path: recovery_root,
                code: "recovery-directory-unreadable",
                message: error.to_string(),
            }),
        }
        discovery.checkpoints.sort_by(|left, right| {
            right
                .base_project_revision
                .cmp(&left.base_project_revision)
                .then_with(|| right.saved_unix_ms.cmp(&left.saved_unix_ms))
                .then_with(|| left.manifest_path.cmp(&right.manifest_path))
        });

        let journal_root = self.package.journal_root();
        match fs::read_dir(&journal_root) {
            Ok(entries) => {
                discovery.journals = entries
                    .flatten()
                    .map(|entry| entry.path())
                    .filter(|path| path.is_file())
                    .collect();
                discovery.journals.sort();
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => discovery.diagnostics.push(StoreDiagnostic {
                path: journal_root,
                code: "journal-directory-unreadable",
                message: error.to_string(),
            }),
        }
        discovery
    }

    /// Store opaque framed journal bytes.  The ENVELOPE lane owns frame
    /// construction/checksums; this layer supplies atomic publication and a
    /// stable recovery location.
    pub fn write_journal_segment(
        &self,
        name: &str,
        bytes: &[u8],
    ) -> Result<PathBuf, ProjectStoreError> {
        if !safe_leaf_name(name) {
            return Err(ProjectStoreError::InvalidJournalName(name.into()));
        }
        let root = self.package.journal_root();
        fs::create_dir_all(&root).map_err(|source| ProjectStoreError::Io {
            path: root.clone(),
            source,
        })?;
        let path = root.join(name);
        atomic_replace(&path, bytes)?;
        Ok(path)
    }

    fn save(
        &self,
        checkpoint: &ProjectCheckpoint,
        expected_revision: u64,
        kind: CheckpointKind,
        saved_unix_ms: Option<u64>,
    ) -> Result<SaveResult, ProjectStoreError> {
        if checkpoint.revision() != expected_revision {
            return Err(ProjectStoreError::RevisionMismatch {
                expected: expected_revision,
                checkpoint: checkpoint.revision(),
            });
        }
        checkpoint.validate().map_err(ProjectStoreError::Format)?;
        fs::create_dir_all(self.package.root()).map_err(|source| ProjectStoreError::Io {
            path: self.package.root().to_path_buf(),
            source,
        })?;

        let mut scoped = checkpoint
            .revision_scoped()
            .map_err(ProjectStoreError::Format)?;
        match kind {
            CheckpointKind::Primary => scoped.file.recovery = RecoveryMetadata::default(),
            CheckpointKind::Autosave => {
                let saved_unix_ms = saved_unix_ms.expect("autosave supplies a timestamp");
                scoped.file.recovery = RecoveryMetadata {
                    is_autosave: true,
                    saved_unix_ms,
                    base_project_revision: expected_revision,
                    primary_file_name: self
                        .package
                        .root()
                        .file_name()
                        .and_then(|name| name.to_str())
                        .map(str::to_owned),
                };
            }
        }

        let mut published = 0;
        for (key, bytes) in &scoped.payloads.0 {
            let path = self
                .package
                .payload_path(key)
                .map_err(ProjectStoreError::Format)?;
            if write_immutable(&path, bytes)? {
                published += 1;
            }
        }
        let manifest_path = match kind {
            CheckpointKind::Primary => self.package.manifest_path(),
            CheckpointKind::Autosave => self.package.recovery_root().join(format!(
                "r{}-{}.json",
                expected_revision,
                saved_unix_ms.expect("autosave supplies a timestamp")
            )),
        };
        let bytes = scoped
            .file
            .encode_pretty()
            .map_err(ProjectStoreError::Envelope)?;
        atomic_replace(&manifest_path, &bytes)?;
        Ok(SaveResult {
            kind,
            manifest_path,
            revision_guard: SaveRevisionGuard {
                revision: expected_revision,
            },
            payloads_published: published,
        })
    }

    fn load_manifest(&self, manifest_path: &Path) -> Result<LoadedCheckpoint, ProjectStoreError> {
        let (file, diagnostics) = read_file(manifest_path)?;
        let mut payloads = DomainPayloads::default();
        for section in &file.sections {
            let path = self
                .package
                .payload_path(&section.payload_key)
                .map_err(ProjectStoreError::Format)?;
            let bytes = fs::read(&path).map_err(|source| ProjectStoreError::Io {
                path: path.clone(),
                source,
            })?;
            if payloads
                .0
                .insert(section.payload_key.clone(), bytes)
                .is_some()
            {
                return Err(ProjectStoreError::DuplicatePayload(
                    section.payload_key.clone(),
                ));
            }
        }
        let checkpoint = ProjectCheckpoint::new(file, payloads, Default::default())
            .map_err(ProjectStoreError::Format)?;
        Ok(LoadedCheckpoint {
            checkpoint,
            diagnostics,
            manifest_path: manifest_path.to_path_buf(),
        })
    }
}

fn read_file(path: &Path) -> Result<(ProjectFile, Vec<ProjectIoDiagnostic>), ProjectStoreError> {
    let bytes = fs::read(path).map_err(|source| ProjectStoreError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    ProjectFile::decode(&bytes).map_err(ProjectStoreError::Envelope)
}

fn write_immutable(path: &Path, bytes: &[u8]) -> Result<bool, ProjectStoreError> {
    match fs::read(path) {
        Ok(existing) if existing == bytes => return Ok(false),
        Ok(_) => {
            return Err(ProjectStoreError::ImmutablePayloadConflict(
                path.to_path_buf(),
            ))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(source) => {
            return Err(ProjectStoreError::Io {
                path: path.to_path_buf(),
                source,
            })
        }
    }
    atomic_create_immutable(path, bytes)
}

/// Publish a new payload without ever replacing an existing one.  Linking a
/// fully synced sibling temporary file is an atomic create on the package
/// filesystem; if another writer won the race, compare its bytes instead of
/// overwriting a revision address.
fn atomic_create_immutable(path: &Path, bytes: &[u8]) -> Result<bool, ProjectStoreError> {
    let parent = path
        .parent()
        .ok_or_else(|| ProjectStoreError::InvalidPath(path.into()))?;
    fs::create_dir_all(parent).map_err(|source| ProjectStoreError::Io {
        path: parent.to_path_buf(),
        source,
    })?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| ProjectStoreError::InvalidPath(path.into()))?;
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temporary = parent.join(format!(
        ".{name}.create-{}-{sequence}.tmp",
        std::process::id()
    ));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(|source| ProjectStoreError::Io {
                path: temporary.clone(),
                source,
            })?;
        file.write_all(bytes)
            .map_err(|source| ProjectStoreError::Io {
                path: temporary.clone(),
                source,
            })?;
        file.sync_all().map_err(|source| ProjectStoreError::Io {
            path: temporary.clone(),
            source,
        })?;
        drop(file);
        match fs::hard_link(&temporary, path) {
            Ok(()) => {
                fs::remove_file(&temporary).map_err(|source| ProjectStoreError::Io {
                    path: temporary.clone(),
                    source,
                })?;
                if let Ok(directory) = File::open(parent) {
                    let _ = directory.sync_all();
                }
                Ok(true)
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                let existing = fs::read(path).map_err(|source| ProjectStoreError::Io {
                    path: path.to_path_buf(),
                    source,
                })?;
                if existing == bytes {
                    Ok(false)
                } else {
                    Err(ProjectStoreError::ImmutablePayloadConflict(
                        path.to_path_buf(),
                    ))
                }
            }
            Err(source) => Err(ProjectStoreError::Io {
                path: path.to_path_buf(),
                source,
            }),
        }
    })();
    let _ = fs::remove_file(&temporary);
    result
}

fn atomic_replace(path: &Path, bytes: &[u8]) -> Result<(), ProjectStoreError> {
    let parent = path
        .parent()
        .ok_or_else(|| ProjectStoreError::InvalidPath(path.into()))?;
    fs::create_dir_all(parent).map_err(|source| ProjectStoreError::Io {
        path: parent.to_path_buf(),
        source,
    })?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| ProjectStoreError::InvalidPath(path.into()))?;
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temporary = parent.join(format!(
        ".{name}.write-{}-{sequence}.tmp",
        std::process::id()
    ));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(|source| ProjectStoreError::Io {
                path: temporary.clone(),
                source,
            })?;
        file.write_all(bytes)
            .map_err(|source| ProjectStoreError::Io {
                path: temporary.clone(),
                source,
            })?;
        file.sync_all().map_err(|source| ProjectStoreError::Io {
            path: temporary.clone(),
            source,
        })?;
        drop(file);
        fs::rename(&temporary, path).map_err(|source| ProjectStoreError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        if let Ok(directory) = File::open(parent) {
            let _ = directory.sync_all();
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn safe_leaf_name(name: &str) -> bool {
    !name.is_empty()
        && Path::new(name).file_name().and_then(|leaf| leaf.to_str()) == Some(name)
        && !name.contains('/')
        && !name.contains('\\')
}

#[derive(Debug)]
pub enum ProjectStoreError {
    Format(ProjectFormatError),
    Envelope(project_io::ProjectIoError),
    RevisionMismatch { expected: u64, checkpoint: u64 },
    ImmutablePayloadConflict(PathBuf),
    DuplicatePayload(PathBuf),
    InvalidPath(PathBuf),
    InvalidJournalName(String),
    Io { path: PathBuf, source: io::Error },
}

impl fmt::Display for ProjectStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Format(error) => write!(formatter, "invalid checkpoint: {error}"),
            Self::Envelope(error) => write!(formatter, "invalid project JSON: {error}"),
            Self::RevisionMismatch {
                expected,
                checkpoint,
            } => write!(
                formatter,
                "save was built for revision {checkpoint}, expected revision {expected}"
            ),
            Self::ImmutablePayloadConflict(path) => write!(
                formatter,
                "immutable payload already exists with different bytes: {}",
                path.display()
            ),
            Self::DuplicatePayload(path) => {
                write!(
                    formatter,
                    "manifest names payload more than once: {}",
                    path.display()
                )
            }
            Self::InvalidPath(path) => {
                write!(formatter, "invalid storage path: {}", path.display())
            }
            Self::InvalidJournalName(name) => {
                write!(formatter, "invalid journal file name: {name}")
            }
            Self::Io { path, source } => write!(formatter, "I/O at {}: {source}", path.display()),
        }
    }
}

impl Error for ProjectStoreError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Format(error) => Some(error),
            Self::Envelope(error) => Some(error),
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}
