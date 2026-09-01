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

use crate::assets::ProjectRelativePath;
use crate::command_journal::{encode_frame, recover_prefix, JournalFrame, JournalTail};
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
    pub recovery_checkpoints_pruned: usize,
    pub maintenance_diagnostics: Vec<StoreDiagnostic>,
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
    /// Verified, user-presentable facts about every journal segment. A
    /// damaged tail does not hide the complete prefix which precedes it.
    pub journal_candidates: Vec<JournalRecoveryCandidate>,
    pub diagnostics: Vec<StoreDiagnostic>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JournalRecoveryCandidate {
    pub path: PathBuf,
    pub label: String,
    pub verified_frames: usize,
    pub valid_bytes: usize,
    pub total_bytes: usize,
    pub first_sequence: Option<u64>,
    pub through_sequence: Option<u64>,
    pub base_revision: Option<u64>,
    pub resulting_revision: Option<u64>,
    pub tail: JournalTail,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JournalCompactionResult {
    pub compacted_path: Option<PathBuf>,
    pub source_segments: Vec<PathBuf>,
    pub active_segments: Vec<PathBuf>,
    pub frames_preserved: usize,
    pub skipped_reason: Option<String>,
}

pub const DEFAULT_MAX_ACTIVE_JOURNAL_SEGMENTS: usize = 8;
pub const DEFAULT_MAX_RECOVERY_CHECKPOINTS: usize = 8;

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

    /// Publish content-addressed project media before a manifest is allowed to
    /// reference it. Generated media is immutable under its canonical PCM
    /// identity; retries accept byte-identical files and refuse collisions.
    pub fn publish_generated_media(
        &self,
        relative: &ProjectRelativePath,
        bytes: &[u8],
    ) -> Result<PathBuf, ProjectStoreError> {
        let relative_path = Path::new(relative.as_str());
        if relative_path.components().next()
            != Some(std::path::Component::Normal(std::ffi::OsStr::new("media")))
        {
            return Err(ProjectStoreError::InvalidPath(relative_path.into()));
        }
        let path = relative.resolve_from(self.package.root());
        write_immutable(&path, bytes)?;
        Ok(path)
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
        let recovery_root = self.package.recovery_root();
        let leaf = candidate
            .manifest_path
            .file_name()
            .and_then(|name| name.to_str());
        if candidate.manifest_path.parent() != Some(recovery_root.as_path())
            || candidate
                .manifest_path
                .extension()
                .and_then(|ext| ext.to_str())
                != Some("json")
            || !leaf.is_some_and(safe_leaf_name)
        {
            return Err(ProjectStoreError::InvalidRecoveryCheckpoint(
                candidate.manifest_path.clone(),
            ));
        }
        if fs::symlink_metadata(&candidate.manifest_path)
            .map(|metadata| metadata.file_type().is_symlink())
            .unwrap_or(false)
        {
            return Err(ProjectStoreError::InvalidRecoveryCheckpoint(
                candidate.manifest_path.clone(),
            ));
        }
        let loaded = self.load_manifest(&candidate.manifest_path)?;
        let recovery = &loaded.checkpoint.file.recovery;
        if !recovery.is_autosave
            || recovery.saved_unix_ms != candidate.saved_unix_ms
            || recovery.base_project_revision != candidate.base_project_revision
        {
            return Err(ProjectStoreError::RecoveryCheckpointMismatch {
                path: candidate.manifest_path.clone(),
                expected_saved_unix_ms: candidate.saved_unix_ms,
                expected_base_revision: candidate.base_project_revision,
                actual_saved_unix_ms: recovery.saved_unix_ms,
                actual_base_revision: recovery.base_project_revision,
                is_autosave: recovery.is_autosave,
            });
        }
        Ok(loaded)
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
                    .filter(|path| {
                        path.is_file()
                            && path.extension().and_then(|extension| extension.to_str())
                                == Some("audecj")
                    })
                    .collect();
                discovery.journals.sort();
                for path in &discovery.journals {
                    match journal_candidate(path) {
                        Ok(candidate) => discovery.journal_candidates.push(candidate),
                        Err(error) => discovery.diagnostics.push(StoreDiagnostic {
                            path: path.clone(),
                            code: "journal-unreadable",
                            message: error.to_string(),
                        }),
                    }
                }
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
        // Segment names describe their exact revision/sequence interval and
        // therefore act as idempotent save markers. Never let a retry replace
        // different bytes under the same durable name.
        write_immutable(&path, bytes)?;
        Ok(path)
    }

    /// Bound active segment count without discarding command provenance.
    /// Older clean segments are decoded and re-framed into one ordinary
    /// `.audecj` segment, then removed only after the compacted replacement is
    /// durable. Corrupt/truncated candidates are never deleted automatically.
    pub fn compact_journal_segments(
        &self,
        max_active_segments: usize,
    ) -> Result<JournalCompactionResult, ProjectStoreError> {
        if max_active_segments < 2 {
            return Err(ProjectStoreError::InvalidJournalRetention(
                max_active_segments,
            ));
        }
        let discovery = self.discover_recovery();
        let mut clean = discovery
            .journal_candidates
            .iter()
            .filter(|candidate| candidate.tail == JournalTail::Clean)
            .filter_map(|candidate| Some((candidate.first_sequence?, candidate.path.clone())))
            .collect::<Vec<_>>();
        clean.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));
        if clean.len() <= max_active_segments {
            return Ok(JournalCompactionResult {
                compacted_path: None,
                source_segments: Vec::new(),
                active_segments: discovery.journals,
                frames_preserved: 0,
                skipped_reason: Some("active journal segment count is already bounded".into()),
            });
        }

        let compact_count = clean.len() - (max_active_segments - 1);
        let source_segments = clean
            .into_iter()
            .take(compact_count)
            .map(|(_, path)| path)
            .collect::<Vec<_>>();
        let mut frames = std::collections::BTreeMap::<u64, JournalFrame>::new();
        for path in &source_segments {
            let bytes = fs::read(path).map_err(|source| ProjectStoreError::Io {
                path: path.clone(),
                source,
            })?;
            let recovered = recover_prefix(&bytes);
            if recovered.tail != JournalTail::Clean {
                return Ok(JournalCompactionResult {
                    compacted_path: None,
                    source_segments: Vec::new(),
                    active_segments: discovery.journals,
                    frames_preserved: 0,
                    skipped_reason: Some(format!(
                        "journal candidate {} changed while compaction was prepared",
                        path.display()
                    )),
                });
            }
            for frame in recovered.frames {
                match frames.get(&frame.sequence) {
                    Some(existing) if existing != &frame => {
                        return Ok(JournalCompactionResult {
                            compacted_path: None,
                            source_segments: Vec::new(),
                            active_segments: discovery.journals,
                            frames_preserved: 0,
                            skipped_reason: Some(format!(
                                "conflicting journal sequence {} prevents compaction",
                                frame.sequence
                            )),
                        })
                    }
                    Some(_) => {}
                    None => {
                        frames.insert(frame.sequence, frame);
                    }
                }
            }
        }
        let ordered = frames.into_values().collect::<Vec<_>>();
        if ordered.is_empty() {
            return Ok(JournalCompactionResult {
                compacted_path: None,
                source_segments: Vec::new(),
                active_segments: discovery.journals,
                frames_preserved: 0,
                skipped_reason: Some("selected journal segments have no verified frames".into()),
            });
        }
        if ordered.windows(2).any(|pair| {
            pair[0].sequence.checked_add(1) != Some(pair[1].sequence)
                || pair[0].resulting_revision != pair[1].base_revision
        }) {
            return Ok(JournalCompactionResult {
                compacted_path: None,
                source_segments: Vec::new(),
                active_segments: discovery.journals,
                frames_preserved: 0,
                skipped_reason: Some(
                    "selected journal segments are not one contiguous history".into(),
                ),
            });
        }
        let first = ordered.first().expect("non-empty checked above");
        let last = ordered.last().expect("non-empty checked above");
        let name = format!(
            "compacted-r{:020}-r{:020}-s{:020}-s{:020}.audecj",
            first.base_revision, last.resulting_revision, first.sequence, last.sequence
        );
        let mut encoded = Vec::new();
        for frame in &ordered {
            encoded.extend(encode_frame(frame).map_err(ProjectStoreError::JournalEncode)?);
        }
        let compacted_path = self.write_journal_segment(&name, &encoded)?;
        for path in &source_segments {
            if path != &compacted_path {
                match fs::remove_file(path) {
                    Ok(()) => {}
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(source) => {
                        return Err(ProjectStoreError::Io {
                            path: path.clone(),
                            source,
                        })
                    }
                }
            }
        }
        if let Ok(directory) = File::open(self.package.journal_root()) {
            let _ = directory.sync_all();
        }
        let mut active_segments = self.discover_recovery().journals;
        active_segments.sort();
        Ok(JournalCompactionResult {
            compacted_path: Some(compacted_path),
            source_segments,
            active_segments,
            frames_preserved: ordered.len(),
            skipped_reason: None,
        })
    }

    /// Retain the newest valid recovery manifests. Immutable payloads and all
    /// journal provenance remain untouched; malformed candidates remain
    /// visible for diagnosis rather than being silently deleted.
    pub fn rotate_recovery_checkpoints(
        &self,
        max_checkpoints: usize,
    ) -> Result<Vec<PathBuf>, ProjectStoreError> {
        if max_checkpoints == 0 {
            return Err(ProjectStoreError::InvalidRecoveryRetention(0));
        }
        let mut checkpoints = self.discover_recovery().checkpoints;
        checkpoints.sort_by(|left, right| {
            right
                .base_project_revision
                .cmp(&left.base_project_revision)
                .then_with(|| right.saved_unix_ms.cmp(&left.saved_unix_ms))
                .then_with(|| left.manifest_path.cmp(&right.manifest_path))
        });
        let mut removed = Vec::new();
        for checkpoint in checkpoints.into_iter().skip(max_checkpoints) {
            match fs::remove_file(&checkpoint.manifest_path) {
                Ok(()) => removed.push(checkpoint.manifest_path),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(source) => {
                    return Err(ProjectStoreError::Io {
                        path: checkpoint.manifest_path,
                        source,
                    })
                }
            }
        }
        if !removed.is_empty() {
            if let Ok(directory) = File::open(self.package.recovery_root()) {
                let _ = directory.sync_all();
            }
        }
        Ok(removed)
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
        match kind {
            CheckpointKind::Primary => atomic_replace(&manifest_path, &bytes)?,
            CheckpointKind::Autosave => {
                // Revision+timestamp names are idempotent recovery markers.
                // A retry may confirm identical bytes but cannot rewrite a
                // previously published candidate with different contents.
                write_immutable(&manifest_path, &bytes)?;
            }
        }
        let (recovery_checkpoints_pruned, maintenance_diagnostics) = match kind {
            CheckpointKind::Primary => (0, Vec::new()),
            CheckpointKind::Autosave => {
                match self.rotate_recovery_checkpoints(DEFAULT_MAX_RECOVERY_CHECKPOINTS) {
                    Ok(removed) => (removed.len(), Vec::new()),
                    Err(error) => (
                        0,
                        vec![StoreDiagnostic {
                            path: self.package.recovery_root(),
                            code: "recovery-rotation-failed",
                            message: error.to_string(),
                        }],
                    ),
                }
            }
        };
        Ok(SaveResult {
            kind,
            manifest_path,
            revision_guard: SaveRevisionGuard {
                revision: expected_revision,
            },
            payloads_published: published,
            recovery_checkpoints_pruned,
            maintenance_diagnostics,
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

fn journal_candidate(path: &Path) -> Result<JournalRecoveryCandidate, ProjectStoreError> {
    let bytes = fs::read(path).map_err(|source| ProjectStoreError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let recovered = recover_prefix(&bytes);
    let first = recovered.frames.first();
    let last = recovered.frames.last();
    let disposition = match &recovered.tail {
        JournalTail::Clean => "clean",
        JournalTail::Truncated { .. } => "crash-truncated tail",
        JournalTail::Corrupt { .. } => "corrupt tail",
    };
    let label = match (first, last) {
        (Some(first), Some(last)) => format!(
            "Commands {}–{} · revisions {}–{} · {}",
            first.sequence,
            last.sequence,
            first.base_revision,
            last.resulting_revision,
            disposition
        ),
        _ => format!("No verified commands · {disposition}"),
    };
    Ok(JournalRecoveryCandidate {
        path: path.to_path_buf(),
        label,
        verified_frames: recovered.frames.len(),
        valid_bytes: recovered.valid_bytes,
        total_bytes: bytes.len(),
        first_sequence: first.map(|frame| frame.sequence),
        through_sequence: last.map(|frame| frame.sequence),
        base_revision: first.map(|frame| frame.base_revision),
        resulting_revision: last.map(|frame| frame.resulting_revision),
        tail: recovered.tail,
    })
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
    RevisionMismatch {
        expected: u64,
        checkpoint: u64,
    },
    ImmutablePayloadConflict(PathBuf),
    DuplicatePayload(PathBuf),
    InvalidPath(PathBuf),
    InvalidJournalName(String),
    InvalidJournalRetention(usize),
    InvalidRecoveryRetention(usize),
    InvalidRecoveryCheckpoint(PathBuf),
    RecoveryCheckpointMismatch {
        path: PathBuf,
        expected_saved_unix_ms: u64,
        expected_base_revision: u64,
        actual_saved_unix_ms: u64,
        actual_base_revision: u64,
        is_autosave: bool,
    },
    JournalEncode(crate::command_journal::JournalEncodeError),
    Io {
        path: PathBuf,
        source: io::Error,
    },
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
            Self::InvalidJournalRetention(limit) => {
                write!(
                    formatter,
                    "journal retention limit must be at least two, got {limit}"
                )
            }
            Self::InvalidRecoveryRetention(limit) => write!(
                formatter,
                "recovery checkpoint retention limit must be positive, got {limit}"
            ),
            Self::InvalidRecoveryCheckpoint(path) => write!(
                formatter,
                "recovery checkpoint is outside this package's recovery namespace: {}",
                path.display()
            ),
            Self::RecoveryCheckpointMismatch {
                path,
                expected_saved_unix_ms,
                expected_base_revision,
                actual_saved_unix_ms,
                actual_base_revision,
                is_autosave,
            } => write!(
                formatter,
                "recovery checkpoint metadata at {} does not match its candidate (expected autosave at {expected_saved_unix_ms} from revision {expected_base_revision}; found autosave={is_autosave}, saved at {actual_saved_unix_ms} from revision {actual_base_revision})",
                path.display()
            ),
            Self::JournalEncode(error) => {
                write!(formatter, "encoding compacted journal failed: {error}")
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
            Self::JournalEncode(error) => Some(error),
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

#[cfg(test)]
mod journal_tests {
    use super::*;
    use crate::command_journal::{encode_frame, JournalFrame};
    use crate::command_record::{DurableCommandBatch, OpaqueCommandRecord};
    use serde_json::json;
    use std::collections::{BTreeMap, BTreeSet};

    struct TempStore {
        root: PathBuf,
        store: ProjectStore,
    }

    impl TempStore {
        fn new(label: &str) -> Self {
            let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let root = std::env::temp_dir().join(format!(
                "audec-project-store-{label}-{}-{sequence}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&root);
            let package = ProjectPackage::new(&root).unwrap();
            Self {
                root,
                store: ProjectStore::new(package),
            }
        }
    }

    impl Drop for TempStore {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn frame(sequence: u64) -> JournalFrame {
        JournalFrame::new(
            sequence,
            sequence - 1,
            "execute",
            DurableCommandBatch::new(
                format!("command {sequence}"),
                vec![OpaqueCommandRecord {
                    domain: "test".into(),
                    kind: "marker".into(),
                    schema_version: 1,
                    payload: json!({"sequence": sequence}),
                    extensions: BTreeMap::new(),
                }],
            ),
        )
        .unwrap()
    }

    #[test]
    fn segment_names_are_idempotent_and_never_overwritten() {
        let temp = TempStore::new("idempotent-journal");
        let bytes = encode_frame(&frame(1)).unwrap();
        let first = temp
            .store
            .write_journal_segment("commands.audecj", &bytes)
            .unwrap();
        let second = temp
            .store
            .write_journal_segment("commands.audecj", &bytes)
            .unwrap();
        assert_eq!(first, second);
        assert!(matches!(
            temp.store
                .write_journal_segment("commands.audecj", b"different"),
            Err(ProjectStoreError::ImmutablePayloadConflict(_))
        ));
        assert_eq!(fs::read(first).unwrap(), bytes);
    }

    #[test]
    fn recovery_load_refuses_a_checkpoint_outside_the_package_namespace() {
        let temp = TempStore::new("foreign-recovery");
        let candidate = RecoveryCheckpoint {
            manifest_path: temp.root.join("foreign.json"),
            saved_unix_ms: 42,
            base_project_revision: 7,
        };

        assert!(matches!(
            temp.store.load_recovery(&candidate),
            Err(ProjectStoreError::InvalidRecoveryCheckpoint(path))
                if path == candidate.manifest_path
        ));
    }

    #[test]
    fn discovery_labels_verified_prefix_before_crash_tail() {
        let temp = TempStore::new("crash-tail");
        let mut bytes = encode_frame(&frame(1)).unwrap();
        bytes.extend_from_slice(&encode_frame(&frame(2)).unwrap()[..7]);
        temp.store
            .write_journal_segment("crashed.audecj", &bytes)
            .unwrap();
        let discovery = temp.store.discover_recovery();
        let candidate = &discovery.journal_candidates[0];
        assert_eq!(candidate.verified_frames, 1);
        assert_eq!(candidate.first_sequence, Some(1));
        assert_eq!(candidate.through_sequence, Some(1));
        assert!(candidate.label.contains("crash-truncated tail"));
        assert!(matches!(candidate.tail, JournalTail::Truncated { .. }));
    }

    #[test]
    fn compaction_bounds_segments_without_losing_frames() {
        let temp = TempStore::new("compact-journal");
        for sequence in 1..=10 {
            temp.store
                .write_journal_segment(
                    &format!("commands-{sequence:02}.audecj"),
                    &encode_frame(&frame(sequence)).unwrap(),
                )
                .unwrap();
        }
        let compacted = temp.store.compact_journal_segments(4).unwrap();
        assert_eq!(compacted.frames_preserved, 7);
        assert_eq!(compacted.active_segments.len(), 4);
        assert!(compacted.compacted_path.is_some());

        let mut sequences = BTreeSet::new();
        for path in compacted.active_segments {
            let recovered = recover_prefix(&fs::read(path).unwrap());
            assert_eq!(recovered.tail, JournalTail::Clean);
            sequences.extend(recovered.frames.into_iter().map(|frame| frame.sequence));
        }
        assert_eq!(sequences, (1..=10).collect());
    }
}
