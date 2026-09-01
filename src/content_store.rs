//! Filesystem content-addressed store for immutable Audec objects.
//!
//! Publication is supervisor-owned, verified, fsynced, and made visible by an
//! atomic create-if-absent hard link. Workers and panes never receive cache
//! destinations. The store refuses symlinks, malformed headers, stale GC
//! plans, and uncertain pin state; it does not infer reachability from project
//! files or delete an object merely because it has not been read recently.

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::UNIX_EPOCH;

use crate::content_identity::{
    ContentClass, Digest, IdentityError, SchemaHasher, SchemaTag, Sha256Digest,
};

const OBJECTS: &str = "objects";
const LOCKS: &str = "locks";
const STAGING: &str = "staging";
const PINS: &str = "pins";
const OBJECT_MAGIC: &[u8; 12] = b"AUDEC-CAS\0\x01\0";
const PIN_MAGIC: &str = "audec-cas-pin-v1";
const OBJECT_SUFFIX: &str = ".audec-object";
const MAX_SCHEMA_BYTES: usize = 512;
const IO_BUFFER_BYTES: usize = 64 * 1024;
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ObjectRef {
    pub digest: Digest,
    pub byte_len: u64,
}

impl ObjectRef {
    pub fn for_bytes(schema: SchemaTag, bytes: &[u8]) -> Self {
        Self {
            digest: Digest::of_bytes(schema, bytes),
            byte_len: bytes.len() as u64,
        }
    }

    /// Canonical pointer form used inside durable product manifests. It names
    /// immutable bytes; it is never interpreted as a filesystem path.
    pub fn encode_canonical(&self) -> Vec<u8> {
        let digest = self.digest.encode_canonical();
        let mut output = Vec::with_capacity(8 + digest.len() + 8);
        output.extend_from_slice(&(digest.len() as u64).to_le_bytes());
        output.extend_from_slice(&digest);
        output.extend_from_slice(&self.byte_len.to_le_bytes());
        output
    }

    pub fn decode_canonical(bytes: &[u8]) -> Result<Self, StoreError> {
        if bytes.len() < 16 {
            return Err(StoreError::MalformedReference(
                "object reference is truncated".into(),
            ));
        }
        let digest_len = u64::from_le_bytes(bytes[..8].try_into().unwrap());
        let digest_len = usize::try_from(digest_len)
            .map_err(|_| StoreError::MalformedReference("digest length overflows".into()))?;
        let digest_end = 8_usize
            .checked_add(digest_len)
            .ok_or_else(|| StoreError::MalformedReference("digest length overflows".into()))?;
        let byte_len_end = digest_end
            .checked_add(8)
            .ok_or_else(|| StoreError::MalformedReference("reference length overflows".into()))?;
        if byte_len_end != bytes.len() {
            return Err(StoreError::MalformedReference(
                "object reference has trailing or missing bytes".into(),
            ));
        }
        let digest = Digest::decode_canonical(&bytes[8..digest_end])?;
        let byte_len = u64::from_le_bytes(bytes[digest_end..byte_len_end].try_into().unwrap());
        Ok(Self { digest, byte_len })
    }
}

#[derive(Clone, Debug)]
pub struct StoredObject {
    pub object: ObjectRef,
    pub path: PathBuf,
    pub physical_bytes: u64,
    pub modified_unix_ms: u64,
}

#[derive(Clone, Debug, Default)]
pub struct StoreInventory {
    pub objects: Vec<StoredObject>,
    pub payload_bytes: u64,
    pub physical_bytes: u64,
    pub payload_bytes_by_class: BTreeMap<ContentClass, u64>,
    pub staging_bytes: u64,
    pub publication_locks: usize,
    pub pins: usize,
    pub diagnostics: Vec<StoreDiagnostic>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoreDiagnostic {
    pub path: PathBuf,
    pub code: &'static str,
    pub message: String,
}

#[derive(Clone, Debug)]
pub struct FsContentStore {
    root: PathBuf,
}

impl FsContentStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn ensure_layout(&self) -> Result<(), StoreError> {
        for name in [OBJECTS, LOCKS, STAGING, PINS] {
            let path = self.root.join(name);
            fs::create_dir_all(&path).map_err(|source| StoreError::Io {
                action: "create store layout",
                path,
                source,
            })?;
        }
        Ok(())
    }

    /// Convenience publication for resident bytes. An existing valid object is
    /// an idempotent hit; an existing corrupt object is never overwritten.
    pub fn put_bytes(&self, schema: SchemaTag, bytes: &[u8]) -> Result<PublishResult, StoreError> {
        let object = ObjectRef::for_bytes(schema, bytes);
        match self.acquire(object.clone())? {
            PublishAcquire::Hit(stored) => Ok(PublishResult {
                stored,
                newly_published: false,
            }),
            PublishAcquire::Busy { .. } => Err(StoreError::Busy(object)),
            PublishAcquire::Acquired(lease) => lease.publish_reader(self, bytes),
        }
    }

    /// Acquire the filesystem-visible single-flight warrant for an expected
    /// object. The lease alone cannot publish: bytes must still verify.
    pub fn acquire(&self, object: ObjectRef) -> Result<PublishAcquire, StoreError> {
        self.ensure_layout()?;
        if let Some(stored) = self.inspect_exact(&object, true)? {
            return Ok(PublishAcquire::Hit(stored));
        }
        let lock_path = self.lock_path(&object.digest);
        if let Some(parent) = lock_path.parent() {
            fs::create_dir_all(parent).map_err(|source| StoreError::Io {
                action: "create lock shard",
                path: parent.into(),
                source,
            })?;
        }
        match OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&lock_path)
        {
            Ok(mut file) => {
                writeln!(file, "{}", object.digest).map_err(|source| StoreError::Io {
                    action: "write publication lock",
                    path: lock_path.clone(),
                    source,
                })?;
                file.sync_all().map_err(|source| StoreError::Io {
                    action: "sync publication lock",
                    path: lock_path.clone(),
                    source,
                })?;
                Ok(PublishAcquire::Acquired(PublicationLease {
                    object,
                    lock_path,
                    released: false,
                }))
            }
            Err(source) if source.kind() == io::ErrorKind::AlreadyExists => {
                Ok(PublishAcquire::Busy { object, lock_path })
            }
            Err(source) => Err(StoreError::Io {
                action: "acquire publication lock",
                path: lock_path,
                source,
            }),
        }
    }

    pub fn contains_verified(&self, object: &ObjectRef) -> Result<bool, StoreError> {
        Ok(self.inspect_exact(object, true)?.is_some())
    }

    pub fn verify(&self, object: &ObjectRef) -> Result<StoredObject, StoreError> {
        self.inspect_exact(object, true)?
            .ok_or_else(|| StoreError::Missing(object.clone()))
    }

    pub fn read_verified(
        &self,
        object: &ObjectRef,
        maximum_bytes: u64,
    ) -> Result<Vec<u8>, StoreError> {
        if object.byte_len > maximum_bytes {
            return Err(StoreError::ReadLimit {
                requested: object.byte_len,
                maximum: maximum_bytes,
            });
        }
        let mut file = self.open_payload(object, true)?;
        let capacity = usize::try_from(object.byte_len).map_err(|_| StoreError::ReadLimit {
            requested: object.byte_len,
            maximum: usize::MAX as u64,
        })?;
        let mut bytes = Vec::with_capacity(capacity);
        file.read_to_end(&mut bytes)
            .map_err(|source| StoreError::Io {
                action: "read object payload",
                path: self.object_path(&object.digest),
                source,
            })?;
        if bytes.len() as u64 != object.byte_len {
            return Err(StoreError::Corrupt {
                object: object.clone(),
                detail: "payload length changed during read".into(),
            });
        }
        Ok(bytes)
    }

    /// Returns a seekable file positioned at the payload after fully verifying
    /// it once, then rewinding. Callers stream without a second allocation.
    pub fn open_verified(&self, object: &ObjectRef) -> Result<File, StoreError> {
        self.open_payload(object, true)
    }

    pub fn inventory(&self) -> Result<StoreInventory, StoreError> {
        self.ensure_layout()?;
        let mut inventory = StoreInventory::default();
        for path in regular_files_below(&self.root.join(OBJECTS), &mut inventory.diagnostics)? {
            match self.inspect_path(&path, false) {
                Ok(stored) => {
                    inventory.payload_bytes = inventory
                        .payload_bytes
                        .saturating_add(stored.object.byte_len);
                    inventory.physical_bytes = inventory
                        .physical_bytes
                        .saturating_add(stored.physical_bytes);
                    let class_bytes = inventory
                        .payload_bytes_by_class
                        .entry(stored.object.digest.schema().class())
                        .or_default();
                    *class_bytes = class_bytes.saturating_add(stored.object.byte_len);
                    inventory.objects.push(stored);
                }
                Err(error) => inventory.diagnostics.push(StoreDiagnostic {
                    path,
                    code: "invalid-object",
                    message: error.to_string(),
                }),
            }
        }
        inventory.objects.sort_by(|a, b| a.object.cmp(&b.object));
        inventory.staging_bytes =
            tree_regular_bytes(&self.root.join(STAGING), &mut inventory.diagnostics)?;
        inventory.publication_locks =
            regular_files_below(&self.root.join(LOCKS), &mut inventory.diagnostics)?.len();
        inventory.pins =
            regular_files_below(&self.root.join(PINS), &mut inventory.diagnostics)?.len();
        Ok(inventory)
    }

    /// A scoped pin is a cache root until its guard is released or dropped.
    /// Its marker survives a crash conservatively; recovery policy may later
    /// inspect such orphaned owners rather than silently collecting them.
    pub fn pin(&self, owner: &str, object: ObjectRef) -> Result<ObjectPin, StoreError> {
        self.create_pin(owner, object, None)
    }

    pub fn lease(
        &self,
        owner: &str,
        object: ObjectRef,
        expires_unix_ms: u64,
    ) -> Result<ObjectPin, StoreError> {
        if expires_unix_ms == 0 {
            return Err(StoreError::InvalidPin(
                "lease expiration must be nonzero".into(),
            ));
        }
        self.create_pin(owner, object, Some(expires_unix_ms))
    }

    fn create_pin(
        &self,
        owner: &str,
        object: ObjectRef,
        expires: Option<u64>,
    ) -> Result<ObjectPin, StoreError> {
        validate_owner(owner)?;
        self.ensure_layout()?;
        let gate = self.acquire_gc_gate()?;
        self.verify(&object)?;
        let owner_root = self.root.join(PINS).join(owner);
        fs::create_dir_all(&owner_root).map_err(|source| StoreError::Io {
            action: "create pin owner directory",
            path: owner_root.clone(),
            source,
        })?;
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = owner_root.join(format!("{}-{sequence:020}.pin", object.digest.sha256()));
        let record = PinRecord {
            object: object.clone(),
            expires_unix_ms: expires,
        };
        write_new_sync(&path, record.encode().as_bytes())?;
        sync_directory(&owner_root)?;
        drop(gate);
        Ok(ObjectPin {
            path,
            object,
            released: false,
        })
    }

    /// Builds a deterministic deletion proposal. Malformed pins or object
    /// metadata make planning fail closed rather than widening collection.
    pub fn plan_gc(&self, policy: GcPolicy, now_unix_ms: u64) -> Result<GcPlan, StoreError> {
        let inventory = self.inventory()?;
        if !inventory.diagnostics.is_empty() {
            return Err(StoreError::UncertainInventory(inventory.diagnostics));
        }
        let roots = self.active_roots(now_unix_ms)?;
        let mut candidates = inventory
            .objects
            .iter()
            .filter(|stored| !roots.contains(&stored.object))
            .filter(|stored| {
                now_unix_ms.saturating_sub(stored.modified_unix_ms) >= policy.minimum_age_ms
            })
            .cloned()
            .collect::<Vec<_>>();
        candidates.sort_by(|a, b| {
            a.modified_unix_ms
                .cmp(&b.modified_unix_ms)
                .then_with(|| a.object.cmp(&b.object))
        });
        let mut projected = inventory.payload_bytes;
        let mut removals = Vec::new();
        for candidate in candidates {
            if projected <= policy.target_payload_bytes {
                break;
            }
            projected = projected.saturating_sub(candidate.object.byte_len);
            removals.push(GcCandidate {
                object: candidate.object,
                path: candidate.path,
                observed_physical_bytes: candidate.physical_bytes,
                observed_modified_unix_ms: candidate.modified_unix_ms,
            });
        }
        Ok(GcPlan {
            root: canonicalish(&self.root),
            target_payload_bytes: policy.target_payload_bytes,
            before_payload_bytes: inventory.payload_bytes,
            projected_payload_bytes: projected,
            protected_objects: roots.len(),
            removals,
        })
    }

    /// Executes a plan under the same gate used to create pins, rechecking
    /// roots and every observed file fact before each deletion.
    pub fn execute_gc(&self, plan: &GcPlan, now_unix_ms: u64) -> Result<GcReport, StoreError> {
        if plan.root != canonicalish(&self.root) {
            return Err(StoreError::WrongStorePlan);
        }
        let _gate = self.acquire_gc_gate()?;
        let roots = self.active_roots_without_gate(now_unix_ms)?;
        let mut report = GcReport::default();
        let mut changed_directories = BTreeSet::new();
        for candidate in &plan.removals {
            if roots.contains(&candidate.object) {
                report.skipped_pinned.push(candidate.object.clone());
                continue;
            }
            let actual = match self.inspect_path(&candidate.path, false) {
                Ok(actual) => actual,
                Err(StoreError::Io { source, .. }) if source.kind() == io::ErrorKind::NotFound => {
                    report.already_absent.push(candidate.object.clone());
                    continue;
                }
                Err(error) => return Err(error),
            };
            if actual.object != candidate.object
                || actual.physical_bytes != candidate.observed_physical_bytes
                || actual.modified_unix_ms != candidate.observed_modified_unix_ms
            {
                report.skipped_changed.push(candidate.object.clone());
                continue;
            }
            fs::remove_file(&candidate.path).map_err(|source| StoreError::Io {
                action: "remove collected object",
                path: candidate.path.clone(),
                source,
            })?;
            if let Some(parent) = candidate.path.parent() {
                changed_directories.insert(parent.to_path_buf());
            }
            report.payload_bytes_removed = report
                .payload_bytes_removed
                .saturating_add(candidate.object.byte_len);
            report.objects_removed.push(candidate.object.clone());
        }
        for directory in changed_directories {
            sync_directory(&directory)?;
        }
        Ok(report)
    }

    fn active_roots(&self, now: u64) -> Result<BTreeSet<ObjectRef>, StoreError> {
        let _gate = self.acquire_gc_gate()?;
        self.active_roots_without_gate(now)
    }

    fn active_roots_without_gate(&self, now: u64) -> Result<BTreeSet<ObjectRef>, StoreError> {
        let mut roots = BTreeSet::new();
        let mut diagnostics = Vec::new();
        for path in regular_files_below(&self.root.join(PINS), &mut diagnostics)? {
            let bytes = fs::read(&path).map_err(|source| StoreError::Io {
                action: "read pin",
                path: path.clone(),
                source,
            })?;
            let record = PinRecord::decode(&bytes).map_err(|detail| {
                StoreError::InvalidPin(format!("{}: {detail}", path.display()))
            })?;
            if record.expires_unix_ms.map_or(true, |expires| expires > now) {
                roots.insert(record.object);
            }
        }
        if !diagnostics.is_empty() {
            return Err(StoreError::UncertainInventory(diagnostics));
        }
        Ok(roots)
    }

    fn inspect_exact(
        &self,
        object: &ObjectRef,
        verify_payload: bool,
    ) -> Result<Option<StoredObject>, StoreError> {
        let path = self.object_path(&object.digest);
        match self.inspect_path_mode(&path, verify_payload) {
            Ok(stored) if stored.object == *object => Ok(Some(stored)),
            Ok(stored) => Err(StoreError::Corrupt {
                object: object.clone(),
                detail: format!("path contains descriptor for {}", stored.object.digest),
            }),
            Err(StoreError::Io { source, .. }) if source.kind() == io::ErrorKind::NotFound => {
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }

    fn inspect_path(&self, path: &Path, verify_payload: bool) -> Result<StoredObject, StoreError> {
        self.inspect_path_mode(path, verify_payload)
    }
    fn inspect_path_mode(
        &self,
        path: &Path,
        verify_payload: bool,
    ) -> Result<StoredObject, StoreError> {
        let metadata = fs::symlink_metadata(path).map_err(|source| StoreError::Io {
            action: "inspect object",
            path: path.into(),
            source,
        })?;
        if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
            return Err(StoreError::UnsafePath(path.into()));
        }
        let mut file = File::open(path).map_err(|source| StoreError::Io {
            action: "open object",
            path: path.into(),
            source,
        })?;
        let (object, payload_offset) = read_header(&mut file, path)?;
        if metadata.len() != payload_offset.saturating_add(object.byte_len) {
            return Err(StoreError::Corrupt {
                object,
                detail: "file length differs from header".into(),
            });
        }
        if self.object_path(&object.digest) != path {
            return Err(StoreError::Corrupt {
                object,
                detail: "object is stored under a noncanonical path".into(),
            });
        }
        if verify_payload {
            verify_payload_reader(&mut file, &object, path)?;
        }
        Ok(StoredObject {
            object,
            path: path.into(),
            physical_bytes: metadata.len(),
            modified_unix_ms: modified_ms(&metadata),
        })
    }

    fn open_payload(&self, object: &ObjectRef, verify: bool) -> Result<File, StoreError> {
        let path = self.object_path(&object.digest);
        let metadata = fs::symlink_metadata(&path).map_err(|source| StoreError::Io {
            action: "inspect object",
            path: path.clone(),
            source,
        })?;
        if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
            return Err(StoreError::UnsafePath(path));
        }
        let mut file = File::open(&path).map_err(|source| StoreError::Io {
            action: "open object",
            path: path.clone(),
            source,
        })?;
        let (actual, offset) = read_header(&mut file, &path)?;
        if actual != *object {
            return Err(StoreError::Corrupt {
                object: object.clone(),
                detail: "stored descriptor differs from requested object".into(),
            });
        }
        if verify {
            verify_payload_reader(&mut file, object, &path)?;
            file.seek(SeekFrom::Start(offset))
                .map_err(|source| StoreError::Io {
                    action: "rewind object payload",
                    path: path.clone(),
                    source,
                })?;
        }
        Ok(file)
    }

    fn object_path(&self, digest: &Digest) -> PathBuf {
        let schema_key = schema_storage_key(digest.schema());
        let hex = digest.sha256().to_hex();
        self.root
            .join(OBJECTS)
            .join(Digest::ALGORITHM)
            .join(schema_key)
            .join(&hex[..2])
            .join(format!("{hex}{OBJECT_SUFFIX}"))
    }
    fn lock_path(&self, digest: &Digest) -> PathBuf {
        self.root
            .join(LOCKS)
            .join(schema_storage_key(digest.schema()))
            .join(format!("{}.lock", digest.sha256()))
    }
    fn acquire_gc_gate(&self) -> Result<GcGate, StoreError> {
        self.ensure_layout()?;
        let path = self.root.join(PINS).join(".gc-gate");
        match fs::create_dir(&path) {
            Ok(()) => Ok(GcGate { path }),
            Err(source) if source.kind() == io::ErrorKind::AlreadyExists => Err(StoreError::GcBusy),
            Err(source) => Err(StoreError::Io {
                action: "acquire pin/GC gate",
                path,
                source,
            }),
        }
    }
}

#[derive(Debug)]
pub enum PublishAcquire {
    Hit(StoredObject),
    Acquired(PublicationLease),
    Busy {
        object: ObjectRef,
        lock_path: PathBuf,
    },
}

#[derive(Clone, Debug)]
pub struct PublishResult {
    pub stored: StoredObject,
    pub newly_published: bool,
}

#[derive(Debug)]
pub struct PublicationLease {
    object: ObjectRef,
    lock_path: PathBuf,
    released: bool,
}

impl PublicationLease {
    pub fn object(&self) -> &ObjectRef {
        &self.object
    }
    pub fn publish_reader(
        mut self,
        store: &FsContentStore,
        mut reader: impl Read,
    ) -> Result<PublishResult, StoreError> {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let staging = store.root.join(STAGING).join(format!(
            ".{}-{sequence:020}.tmp",
            self.object.digest.sha256()
        ));
        let result = (|| {
            let mut file = OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&staging)
                .map_err(|source| StoreError::Io {
                    action: "create staged object",
                    path: staging.clone(),
                    source,
                })?;
            write_header(&mut file, &self.object, &staging)?;
            let mut hasher = SchemaHasher::new(self.object.digest.schema().clone());
            let mut remaining = self.object.byte_len;
            let mut buffer = [0; IO_BUFFER_BYTES];
            while remaining > 0 {
                let requested = usize::try_from(remaining.min(buffer.len() as u64)).unwrap();
                let count =
                    reader
                        .read(&mut buffer[..requested])
                        .map_err(|source| StoreError::Io {
                            action: "read publication payload",
                            path: staging.clone(),
                            source,
                        })?;
                if count == 0 {
                    return Err(StoreError::LengthMismatch {
                        expected: self.object.byte_len,
                        actual: self.object.byte_len - remaining,
                    });
                }
                file.write_all(&buffer[..count])
                    .map_err(|source| StoreError::Io {
                        action: "write staged payload",
                        path: staging.clone(),
                        source,
                    })?;
                hasher.update(&buffer[..count]);
                remaining -= count as u64;
            }
            let mut extra = [0; 1];
            if reader.read(&mut extra).map_err(|source| StoreError::Io {
                action: "check publication payload end",
                path: staging.clone(),
                source,
            })? != 0
            {
                return Err(StoreError::LengthMismatch {
                    expected: self.object.byte_len,
                    actual: self.object.byte_len.saturating_add(1),
                });
            }
            let actual =
                Digest::from_verified_parts(self.object.digest.schema().clone(), hasher.finish());
            if actual != self.object.digest {
                return Err(StoreError::DigestMismatch {
                    expected: self.object.digest.clone(),
                    actual,
                });
            }
            file.sync_all().map_err(|source| StoreError::Io {
                action: "sync staged object",
                path: staging.clone(),
                source,
            })?;
            let mut permissions = file
                .metadata()
                .map_err(|source| StoreError::Io {
                    action: "inspect staged object",
                    path: staging.clone(),
                    source,
                })?
                .permissions();
            permissions.set_readonly(true);
            fs::set_permissions(&staging, permissions).map_err(|source| StoreError::Io {
                action: "make staged object read-only",
                path: staging.clone(),
                source,
            })?;
            let destination = store.object_path(&self.object.digest);
            let parent = destination.parent().expect("object path has parent");
            fs::create_dir_all(parent).map_err(|source| StoreError::Io {
                action: "create object shard",
                path: parent.into(),
                source,
            })?;
            match fs::hard_link(&staging, &destination) {
                Ok(()) => {
                    sync_directory(parent)?;
                    fs::remove_file(&staging).map_err(|source| StoreError::Io {
                        action: "remove published staging link",
                        path: staging.clone(),
                        source,
                    })?;
                    let stored = store.verify(&self.object)?;
                    Ok(PublishResult {
                        stored,
                        newly_published: true,
                    })
                }
                Err(source) if source.kind() == io::ErrorKind::AlreadyExists => {
                    let stored = store.verify(&self.object)?;
                    let _ = fs::remove_file(&staging);
                    Ok(PublishResult {
                        stored,
                        newly_published: false,
                    })
                }
                Err(source) => Err(StoreError::Io {
                    action: "atomically publish object",
                    path: destination,
                    source,
                }),
            }
        })();
        if result.is_err() {
            let _ = fs::remove_file(&staging);
        }
        self.release();
        result
    }
    pub fn abandon(mut self) {
        self.release();
    }
    fn release(&mut self) {
        if !self.released {
            let _ = fs::remove_file(&self.lock_path);
            self.released = true;
        }
    }
}
impl Drop for PublicationLease {
    fn drop(&mut self) {
        self.release();
    }
}

#[derive(Debug)]
pub struct ObjectPin {
    path: PathBuf,
    object: ObjectRef,
    released: bool,
}
impl ObjectPin {
    pub fn object(&self) -> &ObjectRef {
        &self.object
    }
    pub fn release(mut self) -> Result<(), StoreError> {
        self.remove()
    }
    fn remove(&mut self) -> Result<(), StoreError> {
        if self.released {
            return Ok(());
        }
        match fs::remove_file(&self.path) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(StoreError::Io {
                    action: "release object pin",
                    path: self.path.clone(),
                    source,
                });
            }
        }
        self.released = true;
        Ok(())
    }
}
impl Drop for ObjectPin {
    fn drop(&mut self) {
        let _ = self.remove();
    }
}

struct GcGate {
    path: PathBuf,
}
impl Drop for GcGate {
    fn drop(&mut self) {
        let _ = fs::remove_dir(&self.path);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GcPolicy {
    pub target_payload_bytes: u64,
    pub minimum_age_ms: u64,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GcCandidate {
    pub object: ObjectRef,
    pub path: PathBuf,
    pub observed_physical_bytes: u64,
    pub observed_modified_unix_ms: u64,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GcPlan {
    pub root: PathBuf,
    pub target_payload_bytes: u64,
    pub before_payload_bytes: u64,
    pub projected_payload_bytes: u64,
    pub protected_objects: usize,
    pub removals: Vec<GcCandidate>,
}
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct GcReport {
    pub objects_removed: Vec<ObjectRef>,
    pub payload_bytes_removed: u64,
    pub skipped_pinned: Vec<ObjectRef>,
    pub skipped_changed: Vec<ObjectRef>,
    pub already_absent: Vec<ObjectRef>,
}

#[derive(Debug)]
pub enum StoreError {
    Io {
        action: &'static str,
        path: PathBuf,
        source: io::Error,
    },
    Identity(IdentityError),
    UnsafePath(PathBuf),
    Missing(ObjectRef),
    Busy(ObjectRef),
    GcBusy,
    LengthMismatch {
        expected: u64,
        actual: u64,
    },
    DigestMismatch {
        expected: Digest,
        actual: Digest,
    },
    Corrupt {
        object: ObjectRef,
        detail: String,
    },
    MalformedObject {
        path: PathBuf,
        detail: String,
    },
    ReadLimit {
        requested: u64,
        maximum: u64,
    },
    InvalidPin(String),
    UncertainInventory(Vec<StoreDiagnostic>),
    WrongStorePlan,
    MalformedReference(String),
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io {
                action,
                path,
                source,
            } => write!(f, "could not {action} at {}: {source}", path.display()),
            Self::Identity(error) => error.fmt(f),
            Self::UnsafePath(path) => write!(f, "unsafe content-store path: {}", path.display()),
            Self::Missing(object) => write!(f, "content object is unavailable: {}", object.digest),
            Self::Busy(object) => write!(
                f,
                "content publication is already in flight: {}",
                object.digest
            ),
            Self::GcBusy => f.write_str("content pins or garbage collection are changing; retry"),
            Self::LengthMismatch { expected, actual } => write!(
                f,
                "payload length {actual} differs from expected {expected}"
            ),
            Self::DigestMismatch { expected, actual } => write!(
                f,
                "payload digest {actual} differs from expected {expected}"
            ),
            Self::Corrupt { object, detail } => {
                write!(f, "content object {} is corrupt: {detail}", object.digest)
            }
            Self::MalformedObject { path, detail } => {
                write!(
                    f,
                    "content object at {} is malformed: {detail}",
                    path.display()
                )
            }
            Self::ReadLimit { requested, maximum } => write!(
                f,
                "object has {requested} bytes, exceeding read limit {maximum}"
            ),
            Self::InvalidPin(detail) => write!(f, "invalid content pin: {detail}"),
            Self::UncertainInventory(d) => write!(
                f,
                "content inventory has {} uncertain entries; GC refused",
                d.len()
            ),
            Self::WrongStorePlan => f.write_str("garbage-collection plan belongs to another store"),
            Self::MalformedReference(detail) => {
                write!(f, "malformed content object reference: {detail}")
            }
        }
    }
}
impl Error for StoreError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Identity(error) => Some(error),
            _ => None,
        }
    }
}
impl From<IdentityError> for StoreError {
    fn from(value: IdentityError) -> Self {
        Self::Identity(value)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PinRecord {
    object: ObjectRef,
    expires_unix_ms: Option<u64>,
}

impl PinRecord {
    fn encode(&self) -> String {
        let mut schema = Vec::new();
        self.object.digest.schema().encode_canonical(&mut schema);
        format!(
            "{PIN_MAGIC}\nschema={}\ndigest={}\nbytes={}\nexpires={}\n",
            hex_bytes(&schema),
            self.object.digest.sha256(),
            self.object.byte_len,
            self.expires_unix_ms
                .map(|v| v.to_string())
                .unwrap_or_else(|| "never".into())
        )
    }
    fn decode(bytes: &[u8]) -> Result<Self, String> {
        let text = std::str::from_utf8(bytes).map_err(|_| "pin is not UTF-8")?;
        let mut lines = text.lines();
        if lines.next() != Some(PIN_MAGIC) {
            return Err("pin magic/version differs".into());
        }
        let mut fields = BTreeMap::new();
        for line in lines {
            let (key, value) = line
                .split_once('=')
                .ok_or_else(|| "pin field lacks '='".to_string())?;
            if fields.insert(key, value).is_some() {
                return Err(format!("duplicate pin field {key}"));
            }
        }
        if fields.len() != 4 {
            return Err("pin has missing or unknown fields".into());
        }
        let get = |key| {
            fields
                .get(key)
                .copied()
                .ok_or_else(|| format!("pin lacks {key}"))
        };
        let schema =
            SchemaTag::decode_canonical(&decode_hex(get("schema")?).map_err(|e| e.to_string())?)
                .map_err(|e| e.to_string())?;
        let sha256 = Sha256Digest::from_hex(get("digest")?).map_err(|e| e.to_string())?;
        let byte_len = get("bytes")?
            .parse()
            .map_err(|_| "pin byte length is invalid")?;
        let expires = get("expires")?;
        let expires_unix_ms = if expires == "never" {
            None
        } else {
            Some(expires.parse().map_err(|_| "pin expiry is invalid")?)
        };
        Ok(Self {
            object: ObjectRef {
                digest: Digest::from_verified_parts(schema, sha256),
                byte_len,
            },
            expires_unix_ms,
        })
    }
}

fn schema_storage_key(schema: &SchemaTag) -> String {
    let mut bytes = Vec::new();
    schema.encode_canonical(&mut bytes);
    Sha256Digest::hash_raw(&bytes).to_hex()
}

fn write_header(file: &mut File, object: &ObjectRef, path: &Path) -> Result<(), StoreError> {
    let mut schema = Vec::new();
    object.digest.schema().encode_canonical(&mut schema);
    if schema.len() > MAX_SCHEMA_BYTES {
        return Err(StoreError::Corrupt {
            object: object.clone(),
            detail: "schema header is too large".into(),
        });
    }
    file.write_all(OBJECT_MAGIC)
        .and_then(|_| file.write_all(&(schema.len() as u16).to_le_bytes()))
        .and_then(|_| file.write_all(&schema))
        .and_then(|_| file.write_all(&object.digest.sha256().bytes()))
        .and_then(|_| file.write_all(&object.byte_len.to_le_bytes()))
        .map_err(|source| StoreError::Io {
            action: "write object header",
            path: path.into(),
            source,
        })
}

fn read_header(file: &mut File, path: &Path) -> Result<(ObjectRef, u64), StoreError> {
    let mut magic = [0; 12];
    file.read_exact(&mut magic)
        .map_err(|source| StoreError::Io {
            action: "read object magic",
            path: path.into(),
            source,
        })?;
    if &magic != OBJECT_MAGIC {
        return Err(StoreError::MalformedObject {
            path: path.into(),
            detail: "unknown object format".into(),
        });
    }
    let mut u16b = [0; 2];
    file.read_exact(&mut u16b)
        .map_err(|source| StoreError::Io {
            action: "read schema length",
            path: path.into(),
            source,
        })?;
    let schema_len = u16::from_le_bytes(u16b) as usize;
    if schema_len == 0 || schema_len > MAX_SCHEMA_BYTES {
        return Err(StoreError::MalformedObject {
            path: path.into(),
            detail: "invalid schema length".into(),
        });
    }
    let mut schema_bytes = vec![0; schema_len];
    file.read_exact(&mut schema_bytes)
        .map_err(|source| StoreError::Io {
            action: "read object schema",
            path: path.into(),
            source,
        })?;
    let schema = SchemaTag::decode_canonical(&schema_bytes)?;
    let mut digest = [0; 32];
    file.read_exact(&mut digest)
        .map_err(|source| StoreError::Io {
            action: "read object digest",
            path: path.into(),
            source,
        })?;
    let mut length = [0; 8];
    file.read_exact(&mut length)
        .map_err(|source| StoreError::Io {
            action: "read object length",
            path: path.into(),
            source,
        })?;
    Ok((
        ObjectRef {
            digest: Digest::from_verified_parts(schema, Sha256Digest::from_bytes(digest)),
            byte_len: u64::from_le_bytes(length),
        },
        12 + 2 + schema_len as u64 + 32 + 8,
    ))
}

fn verify_payload_reader(
    file: &mut File,
    object: &ObjectRef,
    path: &Path,
) -> Result<(), StoreError> {
    let mut hasher = SchemaHasher::new(object.digest.schema().clone());
    let mut remaining = object.byte_len;
    let mut buffer = [0; IO_BUFFER_BYTES];
    while remaining > 0 {
        let requested = usize::try_from(remaining.min(buffer.len() as u64)).unwrap();
        let count = file
            .read(&mut buffer[..requested])
            .map_err(|source| StoreError::Io {
                action: "verify object payload",
                path: path.into(),
                source,
            })?;
        if count == 0 {
            return Err(StoreError::Corrupt {
                object: object.clone(),
                detail: "truncated payload".into(),
            });
        }
        hasher.update(&buffer[..count]);
        remaining -= count as u64;
    }
    let mut extra = [0; 1];
    if file.read(&mut extra).map_err(|source| StoreError::Io {
        action: "verify object end",
        path: path.into(),
        source,
    })? != 0
    {
        return Err(StoreError::Corrupt {
            object: object.clone(),
            detail: "trailing payload bytes".into(),
        });
    }
    let actual = Digest::from_verified_parts(object.digest.schema().clone(), hasher.finish());
    if actual != object.digest {
        return Err(StoreError::DigestMismatch {
            expected: object.digest.clone(),
            actual,
        });
    }
    Ok(())
}

fn regular_files_below(
    root: &Path,
    diagnostics: &mut Vec<StoreDiagnostic>,
) -> Result<Vec<PathBuf>, StoreError> {
    let mut pending = vec![root.to_path_buf()];
    let mut files = Vec::new();
    while let Some(directory) = pending.pop() {
        let entries = match fs::read_dir(&directory) {
            Ok(e) => e,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(source) => {
                return Err(StoreError::Io {
                    action: "scan store directory",
                    path: directory,
                    source,
                });
            }
        };
        for entry in entries {
            let entry = entry.map_err(|source| StoreError::Io {
                action: "read store entry",
                path: directory.clone(),
                source,
            })?;
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path).map_err(|source| StoreError::Io {
                action: "inspect store entry",
                path: path.clone(),
                source,
            })?;
            if metadata.file_type().is_symlink() {
                diagnostics.push(StoreDiagnostic {
                    path,
                    code: "symlink-refused",
                    message: "symlinks are never traversed by the content store".into(),
                });
            } else if metadata.is_dir() {
                pending.push(path);
            } else if metadata.is_file() {
                files.push(path);
            } else {
                diagnostics.push(StoreDiagnostic {
                    path,
                    code: "special-file-refused",
                    message: "special filesystem entries are never content objects".into(),
                });
            }
        }
    }
    files.sort();
    Ok(files)
}

fn tree_regular_bytes(
    root: &Path,
    diagnostics: &mut Vec<StoreDiagnostic>,
) -> Result<u64, StoreError> {
    let mut total = 0u64;
    for path in regular_files_below(root, diagnostics)? {
        let meta = fs::symlink_metadata(&path).map_err(|source| StoreError::Io {
            action: "account staged object",
            path,
            source,
        })?;
        total = total.saturating_add(meta.len());
    }
    Ok(total)
}

fn write_new_sync(path: &Path, bytes: &[u8]) -> Result<(), StoreError> {
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)
        .map_err(|source| StoreError::Io {
            action: "create immutable record",
            path: path.into(),
            source,
        })?;
    file.write_all(bytes).map_err(|source| StoreError::Io {
        action: "write immutable record",
        path: path.into(),
        source,
    })?;
    file.sync_all().map_err(|source| StoreError::Io {
        action: "sync immutable record",
        path: path.into(),
        source,
    })
}
fn sync_directory(path: &Path) -> Result<(), StoreError> {
    File::open(path)
        .and_then(|file| file.sync_all())
        .map_err(|source| StoreError::Io {
            action: "sync directory",
            path: path.into(),
            source,
        })
}
fn modified_ms(metadata: &fs::Metadata) -> u64 {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}
fn canonicalish(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}
fn validate_owner(owner: &str) -> Result<(), StoreError> {
    let valid = !owner.is_empty()
        && owner.len() <= 96
        && owner
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
        && owner != "."
        && owner != "..";
    if valid {
        Ok(())
    } else {
        Err(StoreError::InvalidPin(format!(
            "invalid pin owner {owner:?}"
        )))
    }
}
fn hex_bytes(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    use std::fmt::Write as _;
    for b in bytes {
        write!(&mut out, "{b:02x}").unwrap();
    }
    out
}
fn decode_hex(value: &str) -> Result<Vec<u8>, IdentityError> {
    if value.len() % 2 != 0 {
        return Err(IdentityError::InvalidDigestHex(value.into()));
    }
    let mut out = Vec::with_capacity(value.len() / 2);
    for pair in value.as_bytes().chunks_exact(2) {
        let text =
            std::str::from_utf8(pair).map_err(|_| IdentityError::InvalidDigestHex(value.into()))?;
        out.push(
            u8::from_str_radix(text, 16)
                .map_err(|_| IdentityError::InvalidDigestHex(value.into()))?,
        );
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestRoot(PathBuf);
    impl TestRoot {
        fn new(label: &str) -> Self {
            let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "audec-cas-{label}-{}-{sequence}",
                std::process::id()
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn store(&self) -> FsContentStore {
            FsContentStore::new(&self.0)
        }
    }
    impl Drop for TestRoot {
        fn drop(&mut self) {
            if self
                .0
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("audec-cas-"))
            {
                let _ = fs::remove_dir_all(&self.0);
            }
        }
    }
    fn schema() -> SchemaTag {
        SchemaTag::analysis_artifact("test/artifact", 1).unwrap()
    }

    #[test]
    fn publication_is_verified_immutable_and_idempotent() {
        let root = TestRoot::new("publish");
        let store = root.store();
        let first = store.put_bytes(schema(), b"evidence").unwrap();
        assert!(first.newly_published);
        let second = store.put_bytes(schema(), b"evidence").unwrap();
        assert!(!second.newly_published);
        assert_eq!(first.stored.object, second.stored.object);
        assert_eq!(
            store.read_verified(&first.stored.object, 100).unwrap(),
            b"evidence"
        );
        assert!(store.read_verified(&first.stored.object, 2).is_err());
    }

    #[test]
    fn durable_object_reference_round_trips_and_refuses_trailing_bytes() {
        let object = ObjectRef::for_bytes(schema(), b"persistent");
        let encoded = object.encode_canonical();
        assert_eq!(ObjectRef::decode_canonical(&encoded).unwrap(), object);
        let mut malformed = encoded;
        malformed.push(0);
        assert!(ObjectRef::decode_canonical(&malformed).is_err());
    }

    #[test]
    fn single_flight_and_mismatched_payload_refuse_publication() {
        let root = TestRoot::new("lease");
        let store = root.store();
        let object = ObjectRef::for_bytes(schema(), b"right");
        let lease = match store.acquire(object.clone()).unwrap() {
            PublishAcquire::Acquired(v) => v,
            _ => panic!(),
        };
        assert!(matches!(
            store.acquire(object.clone()).unwrap(),
            PublishAcquire::Busy { .. }
        ));
        assert!(matches!(
            lease.publish_reader(&store, &b"wrong"[..]),
            Err(StoreError::DigestMismatch { .. })
        ));
        assert!(!store.contains_verified(&object).unwrap());
        assert!(store.put_bytes(schema(), b"right").unwrap().newly_published);
    }

    #[test]
    fn pins_protect_and_stale_leases_expire_during_gc() {
        let root = TestRoot::new("gc");
        let store = root.store();
        let pinned = store.put_bytes(schema(), b"pinned").unwrap().stored.object;
        let stale = store.put_bytes(schema(), b"stale").unwrap().stored.object;
        let durable = store.pin("project", pinned.clone()).unwrap();
        let expired = store.lease("transport", stale.clone(), 10).unwrap();
        let plan = store
            .plan_gc(
                GcPolicy {
                    target_payload_bytes: 0,
                    minimum_age_ms: 0,
                },
                11,
            )
            .unwrap();
        assert!(plan.removals.iter().any(|c| c.object == stale));
        assert!(!plan.removals.iter().any(|c| c.object == pinned));
        let report = store.execute_gc(&plan, 11).unwrap();
        assert_eq!(report.objects_removed, vec![stale.clone()]);
        assert!(store.contains_verified(&pinned).unwrap());
        assert!(!store.contains_verified(&stale).unwrap());
        drop(expired);
        durable.release().unwrap();
    }

    #[test]
    fn gc_rechecks_pin_state_at_execution() {
        let root = TestRoot::new("gc-race");
        let store = root.store();
        let object = store
            .put_bytes(schema(), b"candidate")
            .unwrap()
            .stored
            .object;
        let plan = store
            .plan_gc(
                GcPolicy {
                    target_payload_bytes: 0,
                    minimum_age_ms: 0,
                },
                1,
            )
            .unwrap();
        let pin = store.pin("playback", object.clone()).unwrap();
        let report = store.execute_gc(&plan, 1).unwrap();
        assert_eq!(report.skipped_pinned, vec![object.clone()]);
        assert!(store.contains_verified(&object).unwrap());
        pin.release().unwrap();
    }
}
