use std::{
    collections::{HashMap, HashSet},
    fmt, fs, io,
    path::{Path, PathBuf},
    sync::{Arc, OnceLock, Weak},
    time::{Duration, Instant, SystemTime},
};

use parking_lot::{ArcMutexGuard, Mutex, MutexGuard, RawMutex};

use super::{
    model::{
        CaptureByteBucket, CaptureLimits, CaptureRef, PrivateCaptureV1, SafeArtifactReview,
        SafeCaptureRef,
    },
    security::{
        open_regular_private_file, prepare_private_root, reject_link_or_reparse, secure_new_file,
        verify_private_root, verify_regular_private_file, CapturePassphrase, SecurityError,
    },
    writer::{
        read_encrypted, read_encrypted_artifact, safe_review, DecryptedCaptureArtifact,
        EncryptedCaptureStream, VaultFormatError,
    },
};

const ARTIFACT_EXTENSION: &str = "age";
const PARTIAL_EXTENSION: &str = "partial";
const STORE_LOCK_FILE: &str = ".private-capture-store.lock";
const MAX_DIRECTORY_ENTRIES: usize = 256;
#[allow(clippy::duration_suboptimal_units)]
const MAX_PERIODIC_MAINTENANCE_INTERVAL: Duration = Duration::from_secs(15 * 60);

pub(crate) trait StoreClock: Send + Sync {
    fn wall_now(&self) -> SystemTime;
}

#[derive(Debug, Default)]
pub(crate) struct SystemStoreClock;

impl StoreClock for SystemStoreClock {
    fn wall_now(&self) -> SystemTime {
        SystemTime::now()
    }
}

pub(crate) struct CaptureStore {
    root: PathBuf,
    limits: CaptureLimits,
    clock: Arc<dyn StoreClock>,
    process_lock: Arc<Mutex<()>>,
    lock_file: fs::File,
    creation_evidence: Arc<Mutex<HashMap<CaptureRef, SystemTime>>>,
    #[cfg(test)]
    test_hooks: StoreTestHooks,
}

impl fmt::Debug for CaptureStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CaptureStore")
            .field("root", &"[private]")
            .field("limits", &self.limits)
            .finish_non_exhaustive()
    }
}

impl CaptureStore {
    pub(crate) fn open(
        root: impl AsRef<Path>,
        limits: CaptureLimits,
    ) -> Result<(Self, MaintenanceReport), StoreError> {
        Self::with_clock(root, limits, Arc::new(SystemStoreClock))
    }

    pub(crate) fn with_clock(
        root: impl AsRef<Path>,
        limits: CaptureLimits,
        clock: Arc<dyn StoreClock>,
    ) -> Result<(Self, MaintenanceReport), StoreError> {
        let limits = limits.validate().map_err(|_| StoreError::InvalidLimits)?;
        prepare_private_root(root.as_ref()).map_err(StoreError::from)?;
        let root = fs::canonicalize(root.as_ref()).map_err(|_| StoreError::Io)?;
        reject_link_or_reparse(&root).map_err(StoreError::from)?;
        verify_private_root(&root).map_err(StoreError::from)?;
        let process_lock = process_lock_for(&root);
        let lock_file = open_store_lock(&root)?;
        let store = Self {
            root,
            limits,
            clock,
            process_lock,
            lock_file,
            creation_evidence: Arc::new(Mutex::new(HashMap::new())),
            #[cfg(test)]
            test_hooks: StoreTestHooks::default(),
        };
        let report = store.maintain()?;
        Ok((store, report))
    }

    pub(crate) fn write(
        &self,
        capture: &PrivateCaptureV1,
        passphrase: &CapturePassphrase,
    ) -> Result<StoredArtifact, StoreError> {
        self.write_inner(capture, passphrase, None)
    }

    pub(crate) fn write_before(
        &self,
        capture: &PrivateCaptureV1,
        passphrase: &CapturePassphrase,
        deadline: Instant,
    ) -> Result<StoredArtifact, StoreError> {
        self.write_inner(capture, passphrase, Some(deadline))
    }

    fn write_inner(
        &self,
        capture: &PrivateCaptureV1,
        passphrase: &CapturePassphrase,
        deadline: Option<Instant>,
    ) -> Result<StoredArtifact, StoreError> {
        let mut pending = self.begin_stream(capture.capture_ref(), passphrase)?;
        for record in &capture.records {
            pending.write_record(record).map_err(StoreError::Format)?;
        }
        self.finish_stream(pending, capture, deadline)
    }

    pub(super) fn begin_stream(
        &self,
        capture_ref: CaptureRef,
        passphrase: &CapturePassphrase,
    ) -> Result<PendingCaptureWrite, StoreError> {
        let process_guard = self.process_lock.lock_arc();
        let lock_file = self.lock_file.try_clone().map_err(|_| StoreError::Io)?;
        lock_file.lock().map_err(|_| StoreError::Io)?;
        self.verify_root()?;
        let partial = self.partial_path(capture_ref);
        let final_path = self.artifact_path(capture_ref);
        if partial.exists() || final_path.exists() {
            return Err(StoreError::AlreadyExists);
        }
        if self
            .artifact_refs_locked()?
            .into_iter()
            .any(|candidate| candidate.safe() == capture_ref.safe())
        {
            return Err(StoreError::AmbiguousSafeReference);
        }
        let file = secure_new_file(&partial).map_err(StoreError::from)?;
        let writer = match EncryptedCaptureStream::begin(file, capture_ref, passphrase, self.limits)
        {
            Ok(writer) => writer,
            Err(error) => {
                self.remove_partial_best_effort(&partial);
                return Err(StoreError::Format(error));
            }
        };
        Ok(PendingCaptureWrite {
            writer: Some(writer),
            capture_ref,
            root: self.root.clone(),
            partial,
            final_path,
            lock_file,
            _process_guard: process_guard,
            committed: false,
        })
    }

    pub(super) fn finish_stream(
        &self,
        mut pending: PendingCaptureWrite,
        capture: &PrivateCaptureV1,
        deadline: Option<Instant>,
    ) -> Result<StoredArtifact, StoreError> {
        if pending.capture_ref() != capture.capture_ref() {
            return Err(StoreError::UnsafePath);
        }
        let writer = pending.writer.take().ok_or(StoreError::Io)?;
        let written = writer.finish(capture).map_err(StoreError::Format)?;
        let capture_ref = capture.capture_ref();
        if pending.final_path.exists() {
            return Err(StoreError::AlreadyExists);
        }
        if let Err(error) = verify_regular_private_file(&pending.partial) {
            return Err(StoreError::Security(error));
        }
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            return Err(StoreError::FinalizationDeadline);
        }
        self.verify_root()?;
        if fs::hard_link(&pending.partial, &pending.final_path).is_err() {
            return if pending.final_path.exists() {
                Err(StoreError::AlreadyExists)
            } else {
                Err(StoreError::Io)
            };
        }
        let stored = StoredArtifact {
            capture_ref: written.capture_ref.safe(),
            encrypted_size: CaptureByteBucket::from_bytes(written.encrypted_bytes),
        };
        pending.committed = true;
        if self.verify_root().is_err() {
            return Err(StoreError::Committed(stored));
        }
        if let Err(error) = verify_regular_private_file(&pending.final_path) {
            return Err(self.rollback_or_report_committed(
                capture_ref,
                stored,
                StoreError::Security(error),
            ));
        }
        if fs::remove_file(&pending.partial).is_err() {
            return Err(StoreError::Committed(stored));
        }
        if self.verify_root().is_err() {
            return Err(StoreError::Committed(stored));
        }
        self.remember_manifest_creation(capture_ref, capture.created_unix_ms);

        #[cfg(test)]
        if self
            .test_hooks
            .fail_once_after_rename
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            return Err(StoreError::Committed(stored));
        }

        if self.verify_root().is_err() || sync_directory(&self.root).is_err() {
            return Err(StoreError::Committed(stored));
        }
        let report = match self.maintain_locked() {
            Ok(report) => report,
            Err(_error) if self.is_verified_artifact(capture_ref) => {
                return Err(StoreError::Committed(stored));
            }
            Err(error) => return Err(error),
        };
        if !pending.final_path.exists() || !report.quota_satisfied {
            if pending.final_path.exists() && self.rollback_committed_artifact(capture_ref).is_err()
            {
                return Err(StoreError::Committed(stored));
            }
            return Err(StoreError::QuotaExceeded);
        }
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            return Err(StoreError::Committed(stored));
        }
        Ok(stored)
    }

    pub(crate) fn review(
        &self,
        capture_ref: CaptureRef,
        passphrase: &CapturePassphrase,
    ) -> Result<SafeArtifactReview, StoreError> {
        let _guard = self.operation_lock()?;
        let capture = self.read_private_locked(capture_ref, passphrase)?;
        Ok(safe_review(&capture))
    }

    pub(crate) fn review_by_safe_ref(
        &self,
        capture_ref: SafeCaptureRef,
        passphrase: &CapturePassphrase,
    ) -> Result<SafeArtifactReview, StoreError> {
        let _guard = self.operation_lock()?;
        let capture_ref = self.resolve_safe_ref_locked(capture_ref)?;
        let capture = self.read_private_locked(capture_ref, passphrase)?;
        Ok(safe_review(&capture))
    }

    pub(crate) fn read_private(
        &self,
        capture_ref: CaptureRef,
        passphrase: &CapturePassphrase,
    ) -> Result<PrivateCaptureV1, StoreError> {
        let _guard = self.operation_lock()?;
        self.read_private_locked(capture_ref, passphrase)
    }

    pub(crate) fn list_safe_refs(&self) -> Result<Vec<SafeCaptureRef>, StoreError> {
        let _guard = self.operation_lock()?;
        let references = self.artifact_refs_locked()?;
        let mut safe = Vec::with_capacity(references.len());
        let mut seen = HashSet::with_capacity(references.len());
        for capture_ref in references {
            let capture_ref = capture_ref.safe();
            if !seen.insert(capture_ref) {
                return Err(StoreError::AmbiguousSafeReference);
            }
            safe.push(capture_ref);
        }
        safe.sort_unstable();
        Ok(safe)
    }

    pub(super) fn read_private_by_safe_ref(
        &self,
        capture_ref: SafeCaptureRef,
        passphrase: &CapturePassphrase,
    ) -> Result<PrivateCaptureV1, StoreError> {
        let _guard = self.operation_lock()?;
        let capture_ref = self.resolve_safe_ref_locked(capture_ref)?;
        self.read_private_locked(capture_ref, passphrase)
    }

    pub(super) fn read_private_artifact_by_safe_ref(
        &self,
        capture_ref: SafeCaptureRef,
        passphrase: &CapturePassphrase,
    ) -> Result<DecryptedCaptureArtifact, StoreError> {
        let _guard = self.operation_lock()?;
        let capture_ref = self.resolve_safe_ref_locked(capture_ref)?;
        self.verify_root()?;
        let file = open_regular_private_file(&self.artifact_path(capture_ref))
            .map_err(StoreError::from)?;
        let artifact = read_encrypted_artifact(file, capture_ref, passphrase, self.limits)
            .map_err(StoreError::Format)?;
        self.remember_manifest_creation(capture_ref, artifact.capture().created_unix_ms());
        Ok(artifact)
    }

    pub(super) fn contains_safe_ref(
        &self,
        capture_ref: SafeCaptureRef,
    ) -> Result<bool, StoreError> {
        let _guard = self.operation_lock()?;
        match self.resolve_safe_ref_locked(capture_ref) {
            Ok(_) => Ok(true),
            Err(StoreError::ArtifactNotFound) => Ok(false),
            Err(error) => Err(error),
        }
    }

    pub(super) const fn finalization_timeout(&self) -> Duration {
        self.limits.writer_finalization
    }

    fn read_private_locked(
        &self,
        capture_ref: CaptureRef,
        passphrase: &CapturePassphrase,
    ) -> Result<PrivateCaptureV1, StoreError> {
        self.verify_root()?;
        let file = open_regular_private_file(&self.artifact_path(capture_ref))
            .map_err(StoreError::from)?;
        let capture = read_encrypted(file, capture_ref, passphrase, self.limits)
            .map_err(StoreError::Format)?;
        self.remember_manifest_creation(capture_ref, capture.created_unix_ms);
        Ok(capture)
    }

    fn resolve_safe_ref_locked(&self, safe_ref: SafeCaptureRef) -> Result<CaptureRef, StoreError> {
        let mut matching = self
            .artifact_refs_locked()?
            .into_iter()
            .filter(|capture_ref| capture_ref.safe() == safe_ref);
        let capture_ref = matching.next().ok_or(StoreError::ArtifactNotFound)?;
        if matching.next().is_some() {
            return Err(StoreError::AmbiguousSafeReference);
        }
        Ok(capture_ref)
    }

    fn artifact_refs_locked(&self) -> Result<Vec<CaptureRef>, StoreError> {
        self.verify_root()?;
        let mut references = Vec::new();
        for (index, entry) in fs::read_dir(&self.root)
            .map_err(|_| StoreError::Io)?
            .enumerate()
        {
            if index >= MAX_DIRECTORY_ENTRIES {
                return Err(StoreError::DirectoryEntryLimit);
            }
            let entry = entry.map_err(|_| StoreError::Io)?;
            let Some(EntryKind::Artifact(capture_ref)) = classify_entry(&entry.path()) else {
                continue;
            };
            self.checked_artifact_path(capture_ref)?;
            references.push(capture_ref);
        }
        references.sort_unstable();
        Ok(references)
    }

    pub(crate) fn delete(&self, capture_ref: CaptureRef) -> Result<bool, StoreError> {
        let _guard = self.operation_lock()?;
        self.verify_root()?;
        let path = self.artifact_path(capture_ref);
        if !path.exists() {
            return Ok(false);
        }
        self.checked_artifact_path(capture_ref)?;
        fs::remove_file(path).map_err(|_| StoreError::Io)?;
        self.verify_root()?;
        self.creation_evidence.lock().remove(&capture_ref);
        self.verify_root()?;
        sync_directory(&self.root).map_err(|_| StoreError::Io)?;
        Ok(true)
    }

    pub(crate) fn delete_by_safe_ref(
        &self,
        capture_ref: SafeCaptureRef,
    ) -> Result<bool, StoreError> {
        let _guard = self.operation_lock()?;
        let capture_ref = match self.resolve_safe_ref_locked(capture_ref) {
            Ok(capture_ref) => capture_ref,
            Err(StoreError::ArtifactNotFound) => return Ok(false),
            Err(error) => return Err(error),
        };
        self.verify_root()?;
        self.remove_known_artifact(capture_ref)?;
        self.creation_evidence.lock().remove(&capture_ref);
        self.verify_root()?;
        sync_directory(&self.root).map_err(|_| StoreError::Io)?;
        Ok(true)
    }

    pub(crate) fn maintain(&self) -> Result<MaintenanceReport, StoreError> {
        let _guard = self.operation_lock()?;
        self.maintain_locked()
    }

    /// Attempts one maintenance pass without waiting for an active writer or another process.
    /// A periodic runtime worker can skip a busy pass and retry at the next interval.
    pub(crate) fn try_maintain(&self) -> Result<Option<MaintenanceReport>, StoreError> {
        let Some(_guard) = self.try_operation_lock()? else {
            return Ok(None);
        };
        self.maintain_locked().map(Some)
    }

    pub(crate) fn periodic_maintenance_interval(&self) -> Duration {
        self.limits.retention.min(MAX_PERIODIC_MAINTENANCE_INTERVAL)
    }

    fn maintain_locked(&self) -> Result<MaintenanceReport, StoreError> {
        self.verify_root()?;
        let mut report = MaintenanceReport::default();
        let mut artifacts = Vec::new();
        let mut retained_partial_bytes = 0_u64;
        let creation_evidence = self.creation_evidence.lock();
        for (index, entry) in fs::read_dir(&self.root)
            .map_err(|_| StoreError::Io)?
            .enumerate()
        {
            if index >= MAX_DIRECTORY_ENTRIES {
                return Err(StoreError::DirectoryEntryLimit);
            }
            let entry = entry.map_err(|_| StoreError::Io)?;
            let path = entry.path();
            let Some(kind) = classify_entry(&path) else {
                continue;
            };
            reject_link_or_reparse(&path).map_err(StoreError::from)?;
            match kind {
                EntryKind::Partial => {
                    let metadata = verify_regular_private_file(&path).map_err(StoreError::from)?;
                    self.verify_root()?;
                    if fs::remove_file(&path).is_ok() {
                        self.verify_root()?;
                        report.partials_removed = report.partials_removed.saturating_add(1);
                    } else {
                        report.failed_removals = report.failed_removals.saturating_add(1);
                        retained_partial_bytes =
                            retained_partial_bytes.saturating_add(metadata.len());
                    }
                }
                EntryKind::Artifact(capture_ref) => {
                    let metadata = verify_regular_private_file(&path).map_err(StoreError::from)?;
                    artifacts.push(ArtifactMetadata {
                        capture_ref,
                        created: creation_evidence
                            .get(&capture_ref)
                            .copied()
                            .unwrap_or_else(|| artifact_creation_time(&metadata)),
                        bytes: metadata.len(),
                        removal_failed: false,
                    });
                }
            }
        }
        drop(creation_evidence);

        let live_refs = artifacts
            .iter()
            .map(|artifact| artifact.capture_ref)
            .collect::<HashSet<_>>();
        self.creation_evidence
            .lock()
            .retain(|capture_ref, _| live_refs.contains(capture_ref));

        artifacts.sort_by_key(|artifact| (artifact.created, artifact.capture_ref.bytes()));

        let now = self.clock.wall_now();
        let mut retained = Vec::with_capacity(artifacts.len());
        for mut artifact in artifacts {
            let expired = now
                .duration_since(artifact.created)
                .is_ok_and(|age| age >= self.limits.retention);
            if expired {
                if self.remove_known_artifact(artifact.capture_ref).is_ok() {
                    report.expired_removed = report.expired_removed.saturating_add(1);
                    self.creation_evidence.lock().remove(&artifact.capture_ref);
                    continue;
                }
                report.failed_removals = report.failed_removals.saturating_add(1);
                artifact.removal_failed = true;
            }
            retained.push(artifact);
        }
        let mut artifacts = retained;

        let mut total_bytes = artifacts
            .iter()
            .fold(retained_partial_bytes, |total, artifact| {
                total.saturating_add(artifact.bytes)
            });
        let retained_limit = usize::from(self.limits.retained_artifacts);
        let mut candidate = 0;
        while quota_exceeded(
            artifacts.len(),
            total_bytes,
            retained_limit,
            self.limits.total_storage_bytes,
        ) && candidate < artifacts.len()
        {
            if artifacts[candidate].removal_failed {
                candidate = candidate.saturating_add(1);
                continue;
            }
            let artifact = artifacts[candidate];
            if self.remove_known_artifact(artifact.capture_ref).is_ok() {
                artifacts.remove(candidate);
                total_bytes = total_bytes.saturating_sub(artifact.bytes);
                report.quota_removed = report.quota_removed.saturating_add(1);
                self.creation_evidence.lock().remove(&artifact.capture_ref);
            } else {
                report.failed_removals = report.failed_removals.saturating_add(1);
                artifacts[candidate].removal_failed = true;
                candidate = candidate.saturating_add(1);
            }
        }
        report.retained_artifacts = u16::try_from(artifacts.len()).unwrap_or(u16::MAX);
        report.retained_bytes = total_bytes;
        report.quota_satisfied = !quota_exceeded(
            artifacts.len(),
            total_bytes,
            retained_limit,
            self.limits.total_storage_bytes,
        );
        if report.partials_removed > 0 || report.expired_removed > 0 || report.quota_removed > 0 {
            self.verify_root()?;
            sync_directory(&self.root).map_err(|_| StoreError::Io)?;
        }
        Ok(report)
    }

    fn operation_lock(&self) -> Result<StoreOperationGuard<'_>, StoreError> {
        let process_guard = self.process_lock.lock();
        self.lock_file.lock().map_err(|_| StoreError::Io)?;
        Ok(StoreOperationGuard {
            lock_file: &self.lock_file,
            _process_guard: process_guard,
        })
    }

    fn try_operation_lock(&self) -> Result<Option<StoreOperationGuard<'_>>, StoreError> {
        let Some(process_guard) = self.process_lock.try_lock() else {
            return Ok(None);
        };
        match self.lock_file.try_lock() {
            Ok(()) => Ok(Some(StoreOperationGuard {
                lock_file: &self.lock_file,
                _process_guard: process_guard,
            })),
            Err(fs::TryLockError::WouldBlock) => Ok(None),
            Err(_) => Err(StoreError::Io),
        }
    }

    fn remember_manifest_creation(&self, capture_ref: CaptureRef, created_unix_ms: u64) {
        if let Some(created) =
            SystemTime::UNIX_EPOCH.checked_add(Duration::from_millis(created_unix_ms))
        {
            self.creation_evidence
                .lock()
                .insert(capture_ref, created.min(self.clock.wall_now()));
        }
    }

    fn rollback_or_report_committed(
        &self,
        capture_ref: CaptureRef,
        stored: StoredArtifact,
        original: StoreError,
    ) -> StoreError {
        let path = self.artifact_path(capture_ref);
        if self.verify_root().is_ok() && (fs::remove_file(&path).is_ok() || !path.exists()) {
            self.creation_evidence.lock().remove(&capture_ref);
            if self.verify_root().is_ok() {
                let _ = sync_directory(&self.root);
            }
            original
        } else {
            StoreError::Committed(stored)
        }
    }

    fn rollback_committed_artifact(&self, capture_ref: CaptureRef) -> Result<(), StoreError> {
        self.remove_known_artifact(capture_ref)?;
        self.creation_evidence.lock().remove(&capture_ref);
        // The namespace no longer exposes the artifact even if durability syncing fails.
        if self.verify_root().is_ok() {
            let _ = sync_directory(&self.root);
        }
        Ok(())
    }

    fn is_verified_artifact(&self, capture_ref: CaptureRef) -> bool {
        self.checked_artifact_path(capture_ref).is_ok()
    }

    fn checked_artifact_path(&self, capture_ref: CaptureRef) -> Result<PathBuf, StoreError> {
        self.verify_root()?;
        let path = self.artifact_path(capture_ref);
        verify_regular_private_file(&path).map_err(StoreError::from)?;
        let canonical = fs::canonicalize(&path).map_err(|_| StoreError::Io)?;
        if canonical.parent() != Some(self.root.as_path()) || canonical != path {
            return Err(StoreError::UnsafePath);
        }
        Ok(canonical)
    }

    fn remove_known_artifact(&self, capture_ref: CaptureRef) -> Result<(), StoreError> {
        #[cfg(test)]
        if self
            .test_hooks
            .forced_removal_failures
            .lock()
            .contains(&capture_ref)
        {
            return Err(StoreError::Io);
        }
        self.verify_root()?;
        let unchecked = self.artifact_path(capture_ref);
        if !unchecked.exists() {
            return Ok(());
        }
        let path = self.checked_artifact_path(capture_ref)?;
        self.verify_root()?;
        match fs::remove_file(path) {
            Ok(()) => {
                self.verify_root()?;
                Ok(())
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                self.verify_root()?;
                Ok(())
            }
            Err(_) => Err(StoreError::Io),
        }
    }

    fn verify_root(&self) -> Result<(), StoreError> {
        verify_private_root(&self.root).map_err(StoreError::from)
    }

    fn remove_partial_best_effort(&self, path: &Path) {
        if self.verify_root().is_ok() {
            let _ = fs::remove_file(path);
            let _ = self.verify_root();
        }
    }

    fn artifact_path(&self, capture_ref: CaptureRef) -> PathBuf {
        self.root.join(format!(
            "{}.{}",
            capture_ref.file_stem(),
            ARTIFACT_EXTENSION
        ))
    }

    fn partial_path(&self, capture_ref: CaptureRef) -> PathBuf {
        self.root
            .join(format!("{}.{}", capture_ref.file_stem(), PARTIAL_EXTENSION))
    }
}

pub(super) struct PendingCaptureWrite {
    writer: Option<EncryptedCaptureStream>,
    capture_ref: CaptureRef,
    root: PathBuf,
    partial: PathBuf,
    final_path: PathBuf,
    lock_file: fs::File,
    _process_guard: ArcMutexGuard<RawMutex, ()>,
    committed: bool,
}

impl PendingCaptureWrite {
    pub(super) const fn capture_ref(&self) -> CaptureRef {
        self.capture_ref
    }

    pub(super) fn write_record(
        &mut self,
        record: &super::model::CaptureRecordV1,
    ) -> Result<(), VaultFormatError> {
        self.writer
            .as_mut()
            .ok_or(VaultFormatError::EncryptionFailed)?
            .write_record(record)
    }
}

impl fmt::Debug for PendingCaptureWrite {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PendingCaptureWrite")
            .field("capture_ref", &self.capture_ref.safe())
            .field("committed", &self.committed)
            .finish_non_exhaustive()
    }
}

impl Drop for PendingCaptureWrite {
    fn drop(&mut self) {
        self.writer.take();
        if !self.committed && verify_private_root(&self.root).is_ok() {
            let _ = fs::remove_file(&self.partial);
            let _ = verify_private_root(&self.root);
        }
        let _ = self.lock_file.unlock();
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct StoredArtifact {
    pub(crate) capture_ref: SafeCaptureRef,
    pub(crate) encrypted_size: CaptureByteBucket,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct MaintenanceReport {
    pub(crate) partials_removed: u16,
    pub(crate) expired_removed: u16,
    pub(crate) quota_removed: u16,
    pub(crate) failed_removals: u16,
    pub(crate) retained_artifacts: u16,
    pub(crate) retained_bytes: u64,
    pub(crate) quota_satisfied: bool,
}

#[derive(Debug)]
pub(crate) enum StoreError {
    AlreadyExists,
    AmbiguousSafeReference,
    ArtifactNotFound,
    Committed(StoredArtifact),
    DirectoryEntryLimit,
    FinalizationDeadline,
    Format(VaultFormatError),
    InvalidLimits,
    Io,
    QuotaExceeded,
    Security(SecurityError),
    UnsafePath,
}

impl StoreError {
    pub(crate) const fn committed_artifact(&self) -> Option<StoredArtifact> {
        match self {
            Self::Committed(artifact) => Some(*artifact),
            _ => None,
        }
    }
}

impl fmt::Display for StoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::AlreadyExists => "private capture artifact already exists",
            Self::AmbiguousSafeReference => "private capture reference is ambiguous",
            Self::ArtifactNotFound => "private capture artifact was not found",
            Self::Committed(_) => {
                "private capture committed but post-commit storage maintenance failed"
            }
            Self::DirectoryEntryLimit => {
                "private capture storage contains too many directory entries"
            }
            Self::FinalizationDeadline => {
                "private capture finalization exceeded its local deadline"
            }
            Self::Format(_) => "private capture artifact validation failed",
            Self::InvalidLimits => "private capture limits are invalid",
            Self::Io => "private capture storage operation failed",
            Self::QuotaExceeded => "private capture storage quota could not be satisfied",
            Self::Security(_) => "private capture storage security check failed",
            Self::UnsafePath => "private capture artifact path is unsafe",
        })
    }
}

impl std::error::Error for StoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Format(error) => Some(error),
            Self::Security(error) => Some(error),
            _ => None,
        }
    }
}

impl From<SecurityError> for StoreError {
    fn from(error: SecurityError) -> Self {
        match error {
            SecurityError::UnsafePath => Self::UnsafePath,
            _ => Self::Security(error),
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct ArtifactMetadata {
    capture_ref: CaptureRef,
    created: SystemTime,
    bytes: u64,
    removal_failed: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EntryKind {
    Artifact(CaptureRef),
    Partial,
}

struct StoreOperationGuard<'a> {
    lock_file: &'a fs::File,
    _process_guard: MutexGuard<'a, ()>,
}

impl Drop for StoreOperationGuard<'_> {
    fn drop(&mut self) {
        let _ = self.lock_file.unlock();
    }
}

#[cfg(test)]
#[derive(Debug, Default)]
struct StoreTestHooks {
    forced_removal_failures: Mutex<HashSet<CaptureRef>>,
    fail_once_after_rename: std::sync::atomic::AtomicBool,
}

fn process_lock_for(root: &Path) -> Arc<Mutex<()>> {
    static PROCESS_LOCKS: OnceLock<Mutex<HashMap<PathBuf, Weak<Mutex<()>>>>> = OnceLock::new();

    let mut locks = PROCESS_LOCKS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock();
    locks.retain(|_, lock| lock.strong_count() > 0);
    if let Some(lock) = locks.get(root).and_then(Weak::upgrade) {
        return lock;
    }
    let lock = Arc::new(Mutex::new(()));
    locks.insert(root.to_path_buf(), Arc::downgrade(&lock));
    lock
}

fn open_store_lock(root: &Path) -> Result<fs::File, StoreError> {
    verify_private_root(root).map_err(StoreError::from)?;
    let path = root.join(STORE_LOCK_FILE);
    if !path.exists() {
        match secure_new_file(&path) {
            Ok(file) => {
                file.sync_all().map_err(|_| StoreError::Io)?;
                drop(file);
                verify_private_root(root).map_err(StoreError::from)?;
                sync_directory(root).map_err(|_| StoreError::Io)?;
            }
            Err(error) if !path.exists() => return Err(StoreError::from(error)),
            Err(_) => {}
        }
    }
    verify_private_root(root).map_err(StoreError::from)?;
    let file = open_regular_private_file(&path).map_err(StoreError::from)?;
    verify_private_root(root).map_err(StoreError::from)?;
    Ok(file)
}

fn artifact_creation_time(metadata: &fs::Metadata) -> SystemTime {
    metadata
        .created()
        .or_else(|_| metadata.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

const fn quota_exceeded(
    artifact_count: usize,
    total_bytes: u64,
    retained_limit: usize,
    storage_limit: u64,
) -> bool {
    artifact_count > retained_limit || total_bytes > storage_limit
}

fn classify_entry(path: &Path) -> Option<EntryKind> {
    let extension = path.extension()?.to_str()?;
    let stem = path.file_stem()?.to_str()?;
    let capture_ref = CaptureRef::from_file_stem(stem).ok()?;
    match extension {
        ARTIFACT_EXTENSION => Some(EntryKind::Artifact(capture_ref)),
        PARTIAL_EXTENSION => Some(EntryKind::Partial),
        _ => None,
    }
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> io::Result<()> {
    fs::File::open(path)?.sync_all()
}

#[cfg(not(unix))]
#[allow(clippy::unnecessary_wraps)]
const fn sync_directory(_: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        CaptureStore, StoreClock, StoreError, MAX_DIRECTORY_ENTRIES,
        MAX_PERIODIC_MAINTENANCE_INTERVAL,
    };
    use crate::developer_capture::{
        model::{
            CaptureCompleteness, CaptureLimits, CapturePurpose, CaptureRecordKind, CaptureRecordV1,
            CaptureRef, PrivateCaptureV1, SafeOperationRef, SafeTerminalCategory, SensitiveBytes,
        },
        security::{secure_new_file, CapturePassphrase},
    };
    use std::{
        io::Write as _,
        sync::{atomic::Ordering, Arc, Barrier, Mutex},
        time::{Duration, Instant, SystemTime},
    };

    #[derive(Debug)]
    struct FakeClock(Mutex<SystemTime>);

    impl FakeClock {
        fn new(now: SystemTime) -> Self {
            Self(Mutex::new(now))
        }

        fn advance(&self, duration: Duration) {
            let mut now = self.0.lock().unwrap();
            *now = now.checked_add(duration).unwrap();
        }
    }

    impl StoreClock for FakeClock {
        fn wall_now(&self) -> SystemTime {
            *self.0.lock().unwrap()
        }
    }

    fn unix_ms(time: SystemTime) -> u64 {
        u64::try_from(
            time.duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_millis(),
        )
        .unwrap()
    }

    fn fixture_capture_at(capture_ref: CaptureRef, created: SystemTime) -> PrivateCaptureV1 {
        let created_unix_ms = unix_ms(created);
        PrivateCaptureV1::new(
            capture_ref,
            created_unix_ms,
            created_unix_ms.saturating_add(100),
            CapturePurpose::InteractivePlayback,
            SafeOperationRef::from_bytes([1, 2, 3, 4]),
            vec![CaptureRecordV1::new(
                0,
                10,
                CaptureRecordKind::HttpRequest,
                SensitiveBytes::new(b"seeded-private-store-payload".to_vec()),
            )],
            CaptureCompleteness::Complete,
            Vec::new(),
            0,
            SafeTerminalCategory::Failed,
        )
    }

    fn fixture_capture(capture_ref: CaptureRef) -> PrivateCaptureV1 {
        fixture_capture_at(capture_ref, SystemTime::now())
    }

    fn create_artifact(store: &CaptureStore, capture_ref: CaptureRef, bytes: usize) {
        let path = store.artifact_path(capture_ref);
        let mut file = secure_new_file(&path).unwrap();
        file.write_all(&vec![capture_ref.bytes()[0]; bytes])
            .unwrap();
        file.sync_all().unwrap();
    }

    fn quota_test_limits(retained_artifacts: u16, total_storage_bytes: u64) -> CaptureLimits {
        CaptureLimits {
            record_capacity: 1,
            exchange_capacity: 1,
            player_request_bytes: 1,
            player_response_bytes: 1,
            transport_error_bytes: 1,
            plaintext_bytes: 1,
            encrypted_artifact_bytes: 192 * 1024,
            retained_artifacts,
            total_storage_bytes,
            ..CaptureLimits::default()
        }
    }

    #[test]
    fn encrypted_capture_can_be_written_reviewed_reopened_and_deleted() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("vault");
        let (store, report) = CaptureStore::open(&root, CaptureLimits::default()).unwrap();
        assert!(report.quota_satisfied);
        let capture_ref = CaptureRef::from_bytes([1; 16]);
        let passphrase = CapturePassphrase::new("test vault passphrase".to_owned()).unwrap();
        let stored = store
            .write(&fixture_capture(capture_ref), &passphrase)
            .unwrap();
        assert_eq!(stored.capture_ref, capture_ref.safe());
        let review = store.review(capture_ref, &passphrase).unwrap();
        assert_eq!(review.record_count, 1);
        assert!(review.checksum_valid);
        let private = store.read_private(capture_ref, &passphrase).unwrap();
        assert_eq!(
            private.records[0].payload.expose(),
            b"seeded-private-store-payload"
        );
        assert!(store.delete(capture_ref).unwrap());
        assert!(!store.delete(capture_ref).unwrap());
    }

    #[test]
    fn abandoned_partial_is_removed_on_open() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("vault");
        let (store, _) = CaptureStore::open(&root, CaptureLimits::default()).unwrap();
        let capture_ref = CaptureRef::from_bytes([2; 16]);
        let partial = store.partial_path(capture_ref);
        let mut file = secure_new_file(&partial).unwrap();
        file.write_all(b"incomplete encrypted stream").unwrap();
        drop(file);
        drop(store);

        let (_, report) = CaptureStore::open(&root, CaptureLimits::default()).unwrap();
        assert_eq!(report.partials_removed, 1);
        assert!(!partial.exists());
    }

    #[test]
    fn retention_expiry_uses_an_injected_wall_clock() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("vault");
        let now = SystemTime::now();
        let clock = Arc::new(FakeClock::new(now));
        let (store, _) =
            CaptureStore::with_clock(&root, CaptureLimits::default(), clock.clone()).unwrap();
        let capture_ref = CaptureRef::from_bytes([3; 16]);
        let path = store.artifact_path(capture_ref);
        let file = secure_new_file(&path).unwrap();
        file.sync_all().unwrap();
        drop(file);
        clock.advance(Duration::from_secs(24 * 60 * 60 + 1));
        let report = store.maintain().unwrap();
        assert_eq!(report.expired_removed, 1);
        assert!(!path.exists());
    }

    #[test]
    fn count_and_byte_quotas_use_stable_reference_order_for_equal_creation_times() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("vault");
        const ARTIFACT_BYTES: usize = 96 * 1024;
        let limits = quota_test_limits(2, 192 * 1024);
        let (store, _) = CaptureStore::open(&root, limits).unwrap();
        let same_creation = SystemTime::now();
        for byte in [3, 1, 2] {
            let capture_ref = CaptureRef::from_bytes([byte; 16]);
            create_artifact(&store, capture_ref, ARTIFACT_BYTES);
            store
                .creation_evidence
                .lock()
                .insert(capture_ref, same_creation);
        }
        let report = store.maintain().unwrap();
        assert_eq!(report.quota_removed, 1);
        assert_eq!(report.retained_artifacts, 2);
        assert_eq!(report.retained_bytes, 192 * 1024);
        assert!(!store
            .artifact_path(CaptureRef::from_bytes([1; 16]))
            .exists());
    }

    #[test]
    fn failed_deletions_remain_in_quota_accounting_and_do_not_block_later_candidates() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("vault");
        let limits = quota_test_limits(1, 192 * 1024);
        let (store, _) = CaptureStore::open(&root, limits).unwrap();
        let same_creation = SystemTime::now();
        let references = [
            CaptureRef::from_bytes([1; 16]),
            CaptureRef::from_bytes([2; 16]),
            CaptureRef::from_bytes([3; 16]),
        ];
        for capture_ref in references {
            create_artifact(&store, capture_ref, 5);
            store
                .creation_evidence
                .lock()
                .insert(capture_ref, same_creation);
        }
        store
            .test_hooks
            .forced_removal_failures
            .lock()
            .extend([references[0], references[1]]);

        let report = store.maintain().unwrap();
        assert_eq!(report.failed_removals, 2);
        assert_eq!(report.quota_removed, 1);
        assert_eq!(report.retained_artifacts, 2);
        assert_eq!(report.retained_bytes, 10);
        assert!(!report.quota_satisfied);
        assert!(store.artifact_path(references[0]).exists());
        assert!(store.artifact_path(references[1]).exists());
        assert!(!store.artifact_path(references[2]).exists());

        store.test_hooks.forced_removal_failures.lock().clear();
        let recovered = store.maintain().unwrap();
        assert_eq!(recovered.quota_removed, 1);
        assert_eq!(recovered.retained_artifacts, 1);
        assert_eq!(recovered.retained_bytes, 5);
        assert!(recovered.quota_satisfied);
    }

    #[test]
    fn a_busy_writer_prevents_periodic_maintenance_from_deleting_its_partial() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("vault");
        let (writer_store, _) = CaptureStore::open(&root, CaptureLimits::default()).unwrap();
        let (maintenance_store, _) = CaptureStore::open(&root, CaptureLimits::default()).unwrap();
        let guard = writer_store.operation_lock().unwrap();
        let capture_ref = CaptureRef::from_bytes([4; 16]);
        let partial = writer_store.partial_path(capture_ref);
        let mut file = secure_new_file(&partial).unwrap();
        file.write_all(b"active encrypted stream").unwrap();
        file.sync_all().unwrap();
        drop(file);

        let result = std::thread::spawn(move || maintenance_store.try_maintain().unwrap())
            .join()
            .unwrap();
        assert!(result.is_none());
        assert!(partial.exists());

        drop(guard);
        let report = writer_store.maintain().unwrap();
        assert_eq!(report.partials_removed, 1);
        assert!(!partial.exists());
        assert_eq!(
            writer_store.periodic_maintenance_interval(),
            MAX_PERIODIC_MAINTENANCE_INTERVAL
        );
    }

    #[test]
    fn operation_guard_holds_the_cross_process_lock_file() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("vault");
        let (store, _) = CaptureStore::open(&root, CaptureLimits::default()).unwrap();
        let external = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(root.join(super::STORE_LOCK_FILE))
            .unwrap();

        let guard = store.operation_lock().unwrap();
        assert!(matches!(
            external.try_lock(),
            Err(std::fs::TryLockError::WouldBlock)
        ));
        drop(guard);

        external.try_lock().unwrap();
        external.unlock().unwrap();
    }

    #[test]
    fn concurrent_maintenance_passes_are_serialized_and_converge() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("vault");
        let limits = quota_test_limits(2, 192 * 1024);
        let (store, _) = CaptureStore::open(&root, limits).unwrap();
        let same_creation = SystemTime::now();
        for byte in 1..=8_u8 {
            let capture_ref = CaptureRef::from_bytes([byte; 16]);
            create_artifact(&store, capture_ref, 5);
            store
                .creation_evidence
                .lock()
                .insert(capture_ref, same_creation);
        }
        let store = Arc::new(store);
        let barrier = Arc::new(Barrier::new(8));
        let workers = (0..8)
            .map(|_| {
                let store = store.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    store.maintain()
                })
            })
            .collect::<Vec<_>>();
        for worker in workers {
            assert!(worker.join().unwrap().is_ok());
        }
        let report = store.maintain().unwrap();
        assert_eq!(report.retained_artifacts, 2);
        assert_eq!(report.retained_bytes, 10);
        assert!(report.quota_satisfied);
    }

    #[test]
    fn manifest_creation_time_drives_retention_when_it_is_available() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("vault");
        let now = SystemTime::UNIX_EPOCH
            .checked_add(Duration::from_secs(3 * 24 * 60 * 60))
            .unwrap();
        let clock = Arc::new(FakeClock::new(now));
        let (store, _) = CaptureStore::with_clock(&root, CaptureLimits::default(), clock).unwrap();
        let capture_ref = CaptureRef::from_bytes([5; 16]);
        let created = now
            .checked_sub(CaptureLimits::default().retention + Duration::from_millis(1))
            .unwrap();
        let passphrase = CapturePassphrase::new("retention test passphrase".to_owned()).unwrap();

        let error = store
            .write(&fixture_capture_at(capture_ref, created), &passphrase)
            .unwrap_err();
        assert!(matches!(error, StoreError::QuotaExceeded));
        assert!(!store.artifact_path(capture_ref).exists());
    }

    #[test]
    fn post_rename_failure_reports_the_discoverable_committed_artifact() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("vault");
        let (store, _) = CaptureStore::open(&root, CaptureLimits::default()).unwrap();
        let capture_ref = CaptureRef::from_bytes([6; 16]);
        let passphrase = CapturePassphrase::new("commit state passphrase".to_owned()).unwrap();
        store
            .test_hooks
            .fail_once_after_rename
            .store(true, Ordering::SeqCst);

        let error = store
            .write(&fixture_capture(capture_ref), &passphrase)
            .unwrap_err();
        let committed = error.committed_artifact().unwrap();
        assert_eq!(committed.capture_ref, capture_ref.safe());
        assert!(store.artifact_path(capture_ref).exists());
        assert!(
            store
                .review(capture_ref, &passphrase)
                .unwrap()
                .checksum_valid
        );
    }

    #[test]
    fn expired_finalization_deadline_never_publishes_an_artifact() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("vault");
        let (store, _) = CaptureStore::open(&root, CaptureLimits::default()).unwrap();
        let capture_ref = CaptureRef::from_bytes([16; 16]);
        let passphrase = CapturePassphrase::new("deadline test passphrase".to_owned()).unwrap();

        assert!(matches!(
            store.write_before(&fixture_capture(capture_ref), &passphrase, Instant::now(),),
            Err(StoreError::FinalizationDeadline)
        ));
        assert!(!store.artifact_path(capture_ref).exists());
        assert!(!store.partial_path(capture_ref).exists());
    }

    #[test]
    fn safe_reference_review_and_delete_resolve_under_the_store_lock() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("vault");
        let (store, _) = CaptureStore::open(&root, CaptureLimits::default()).unwrap();
        let capture_ref = CaptureRef::from_bytes([17; 16]);
        let passphrase =
            CapturePassphrase::new("safe reference test passphrase".to_owned()).unwrap();
        store
            .write(&fixture_capture(capture_ref), &passphrase)
            .unwrap();

        let review = store
            .review_by_safe_ref(capture_ref.safe(), &passphrase)
            .unwrap();
        assert_eq!(review.capture_ref, capture_ref.safe());
        assert!(store.contains_safe_ref(capture_ref.safe()).unwrap());
        assert!(store.delete_by_safe_ref(capture_ref.safe()).unwrap());
        assert!(!store.contains_safe_ref(capture_ref.safe()).unwrap());
        assert!(!store.delete_by_safe_ref(capture_ref.safe()).unwrap());
    }

    #[test]
    fn ambiguous_safe_reference_is_rejected_before_private_read_or_delete() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("vault");
        let (store, _) = CaptureStore::open(&root, CaptureLimits::default()).unwrap();
        let left = CaptureRef::from_bytes([18; 16]);
        let mut right_bytes = [18; 16];
        right_bytes[15] = 19;
        let right = CaptureRef::from_bytes(right_bytes);
        create_artifact(&store, left, 1);
        create_artifact(&store, right, 1);

        assert!(matches!(
            store.contains_safe_ref(left.safe()),
            Err(StoreError::AmbiguousSafeReference)
        ));
        assert!(matches!(
            store.delete_by_safe_ref(left.safe()),
            Err(StoreError::AmbiguousSafeReference)
        ));
    }

    #[test]
    fn hostile_directory_enumeration_fails_closed_at_a_fixed_bound() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("vault");
        let (store, _) = CaptureStore::open(&root, CaptureLimits::default()).unwrap();
        for index in 0..MAX_DIRECTORY_ENTRIES {
            std::fs::write(root.join(format!("unrelated-{index:04}.tmp")), b"x").unwrap();
        }

        assert!(matches!(
            store.list_safe_refs(),
            Err(StoreError::DirectoryEntryLimit)
        ));
        assert!(matches!(
            store.maintain(),
            Err(StoreError::DirectoryEntryLimit)
        ));
    }

    #[cfg(windows)]
    #[test]
    fn windows_locked_artifact_is_counted_while_newer_candidates_are_removed() {
        use std::os::windows::fs::OpenOptionsExt as _;

        const FILE_SHARE_READ: u32 = 1;
        const FILE_SHARE_WRITE: u32 = 2;

        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("vault");
        let limits = quota_test_limits(1, 192 * 1024);
        let (store, _) = CaptureStore::open(&root, limits).unwrap();
        let older = CaptureRef::from_bytes([7; 16]);
        let newer = CaptureRef::from_bytes([8; 16]);
        create_artifact(&store, older, 5);
        create_artifact(&store, newer, 5);
        let same_creation = SystemTime::now();
        store.creation_evidence.lock().insert(older, same_creation);
        store.creation_evidence.lock().insert(newer, same_creation);
        let locked = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .open(store.artifact_path(older))
            .unwrap();

        let report = store.maintain().unwrap();
        assert_eq!(report.failed_removals, 1);
        assert_eq!(report.quota_removed, 1);
        assert_eq!(report.retained_artifacts, 1);
        assert_eq!(report.retained_bytes, 5);
        assert!(report.quota_satisfied);
        assert!(store.artifact_path(older).exists());
        assert!(!store.artifact_path(newer).exists());
        drop(locked);
    }

    #[cfg(unix)]
    #[test]
    fn artifact_symlink_escape_is_rejected() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("vault");
        let (store, _) = CaptureStore::open(&root, CaptureLimits::default()).unwrap();
        let outside = directory.path().join("outside");
        std::fs::write(&outside, b"outside").unwrap();
        let capture_ref = CaptureRef::from_bytes([4; 16]);
        symlink(&outside, store.artifact_path(capture_ref)).unwrap();
        assert!(store.maintain().is_err());
        assert_eq!(std::fs::read(&outside).unwrap(), b"outside");
    }
}
