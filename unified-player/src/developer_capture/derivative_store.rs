//! Filesystem boundary for shareable, allowlist-built diagnostic derivatives.

use std::{
    collections::BTreeSet,
    fmt, fs,
    io::{Read, Write},
    path::{Component, Path, PathBuf},
};

use super::{
    sanitize::{
        DerivativeAtomicWriterV1, DerivativeCommitStateV1, DerivativeFileNameV1,
        DerivativeFileSetV1, DerivativeFileViewV1, DerivativeIoError, DerivativeReaderV1,
    },
    security::{
        ensure_private_root_separate, open_regular_private_file, prepare_private_root,
        reject_link_or_reparse, secure_new_file, verify_private_root,
    },
};

const EXPECTED_FILE_COUNT: usize = 3;
const MAX_PATH_COMPONENT_UNITS: usize = 255;

/// A single immutable derivative directory chosen by the operator.
///
/// The path is deliberately absent from `Debug`, diagnostic state, and all safe
/// review models. Creation uses a sibling staging directory and never replaces
/// an existing destination.
pub(crate) struct DerivativeDirectoryStoreV1 {
    target: PathBuf,
}

impl DerivativeDirectoryStoreV1 {
    fn new(target: impl AsRef<Path>) -> Result<Self, DerivativeIoError> {
        let target = checked_absolute(target.as_ref())?;
        let parent = target.parent().ok_or(DerivativeIoError)?;
        if target.file_name().is_none()
            || !safe_output_name(target.file_name().ok_or(DerivativeIoError)?)
            || target.exists()
            || !parent.is_dir()
        {
            return Err(DerivativeIoError);
        }
        reject_link_or_reparse(parent).map_err(|_| DerivativeIoError)?;
        Ok(Self { target })
    }

    /// Constructs a destination only when it is disjoint from every sensitive
    /// or unrelated storage root supplied by the application.
    pub(crate) fn new_separate(
        target: impl AsRef<Path>,
        forbidden_roots: &[PathBuf],
    ) -> Result<Self, DerivativeIoError> {
        if forbidden_roots.is_empty() {
            return Err(DerivativeIoError);
        }
        let store = Self::new(target)?;
        ensure_private_root_separate(&store.target, forbidden_roots)
            .map_err(|_| DerivativeIoError)?;
        Ok(store)
    }

    pub(crate) fn open_existing(target: impl AsRef<Path>) -> Result<Self, DerivativeIoError> {
        let target = checked_absolute(target.as_ref())?;
        reject_link_or_reparse(&target).map_err(|_| DerivativeIoError)?;
        verify_private_root(&target).map_err(|_| DerivativeIoError)?;
        Ok(Self { target })
    }

    fn create_staging(&self) -> Result<PathBuf, DerivativeIoError> {
        let parent = self.target.parent().ok_or(DerivativeIoError)?;
        for _ in 0..8 {
            let suffix = hex_ref(rand::random());
            let candidate = parent.join(format!(".unified-player-derivative-{suffix}.partial"));
            match create_private_directory(&candidate) {
                Ok(()) => {
                    if prepare_private_root(&candidate).is_err() {
                        Self::cleanup_staging(&candidate);
                        return Err(DerivativeIoError);
                    }
                    return Ok(candidate);
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(_) => return Err(DerivativeIoError),
            }
        }
        Err(DerivativeIoError)
    }

    fn cleanup_staging(staging: &Path) {
        for name in [
            DerivativeFileNameV1::Evidence,
            DerivativeFileNameV1::Checksums,
            DerivativeFileNameV1::Manifest,
        ] {
            let _ = fs::remove_file(staging.join(name.as_str()));
        }
        let _ = fs::remove_dir(staging);
    }
}

#[cfg(target_vendor = "apple")]
fn safe_output_name(name: &std::ffi::OsStr) -> bool {
    name.as_encoded_bytes().is_ascii()
}

#[cfg(not(target_vendor = "apple"))]
const fn safe_output_name(_: &std::ffi::OsStr) -> bool {
    true
}

impl fmt::Debug for DerivativeDirectoryStoreV1 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DerivativeDirectoryStoreV1")
            .field("target", &"[private]")
            .finish()
    }
}

impl DerivativeAtomicWriterV1 for DerivativeDirectoryStoreV1 {
    fn write_atomically(
        &mut self,
        files: &[DerivativeFileViewV1<'_>],
    ) -> Result<DerivativeCommitStateV1, DerivativeIoError> {
        self.write_atomically_with_fault(files, PublicationFault::None)
    }
}

#[derive(Clone, Copy)]
enum PublicationFault {
    None,
    #[cfg(test)]
    CorruptStaging,
    #[cfg(test)]
    Rename,
    #[cfg(test)]
    PostCommit,
}

impl DerivativeDirectoryStoreV1 {
    fn write_atomically_with_fault(
        &mut self,
        files: &[DerivativeFileViewV1<'_>],
        fault: PublicationFault,
    ) -> Result<DerivativeCommitStateV1, DerivativeIoError> {
        #[cfg(not(test))]
        let _ = fault;
        let ordered = exact_files(files)?;
        let parent = self.target.parent().ok_or(DerivativeIoError)?;
        reject_link_or_reparse(parent).map_err(|_| DerivativeIoError)?;
        if self.target.exists() {
            return Err(DerivativeIoError);
        }

        let staging = self.create_staging()?;
        let mut committed = false;
        let result = (|| {
            for file in &ordered {
                let path = staging.join(file.name().as_str());
                let mut output = secure_new_file(&path).map_err(|_| DerivativeIoError)?;
                output
                    .write_all(file.contents())
                    .map_err(|_| DerivativeIoError)?;
                output.sync_all().map_err(|_| DerivativeIoError)?;
            }
            #[cfg(test)]
            if matches!(fault, PublicationFault::CorruptStaging) {
                fs::write(
                    staging.join(DerivativeFileNameV1::Evidence.as_str()),
                    b"corrupted",
                )
                .map_err(|_| DerivativeIoError)?;
            }
            validate_staging(&staging, &ordered)?;
            sync_directory(&staging).map_err(|_| DerivativeIoError)?;
            reject_link_or_reparse(parent).map_err(|_| DerivativeIoError)?;
            if self.target.exists() {
                return Err(DerivativeIoError);
            }
            #[cfg(test)]
            if matches!(fault, PublicationFault::Rename) {
                return Err(DerivativeIoError);
            }
            rename_directory_no_replace(&staging, &self.target)?;
            committed = true;

            #[cfg(test)]
            if matches!(fault, PublicationFault::PostCommit) {
                return Ok(DerivativeCommitStateV1::CommittedDurabilityUncertain);
            }
            if reject_link_or_reparse(&self.target).is_err()
                || verify_private_root(&self.target).is_err()
                || sync_directory(parent).is_err()
            {
                return Ok(DerivativeCommitStateV1::CommittedDurabilityUncertain);
            }
            Ok(DerivativeCommitStateV1::Committed)
        })();
        if result.is_err() && !committed {
            Self::cleanup_staging(&staging);
        }
        result
    }
}

impl DerivativeReaderV1 for DerivativeDirectoryStoreV1 {
    fn read_exact(&self, maximum_bytes: usize) -> Result<DerivativeFileSetV1, DerivativeIoError> {
        verify_private_root(&self.target).map_err(|_| DerivativeIoError)?;
        let mut unexpected_entries = 0_u16;
        let mut entry_count = 0_usize;
        for entry in fs::read_dir(&self.target).map_err(|_| DerivativeIoError)? {
            let entry = entry.map_err(|_| DerivativeIoError)?;
            entry_count = entry_count.saturating_add(1);
            reject_link_or_reparse(&entry.path()).map_err(|_| DerivativeIoError)?;
            if !is_expected_name(&entry.file_name()) {
                unexpected_entries = unexpected_entries.saturating_add(1);
            }
            if entry_count > EXPECTED_FILE_COUNT {
                break;
            }
        }
        if entry_count != EXPECTED_FILE_COUNT || unexpected_entries != 0 {
            return Ok(DerivativeFileSetV1::new(
                Vec::new(),
                Vec::new(),
                Vec::new(),
                unexpected_entries.saturating_add(u16::from(entry_count != EXPECTED_FILE_COUNT)),
            ));
        }

        let mut remaining = maximum_bytes;
        let evidence = read_bounded(
            &self.target.join(DerivativeFileNameV1::Evidence.as_str()),
            &mut remaining,
        )?;
        let checksums = read_bounded(
            &self.target.join(DerivativeFileNameV1::Checksums.as_str()),
            &mut remaining,
        )?;
        let manifest = read_bounded(
            &self.target.join(DerivativeFileNameV1::Manifest.as_str()),
            &mut remaining,
        )?;
        verify_private_root(&self.target).map_err(|_| DerivativeIoError)?;
        Ok(DerivativeFileSetV1::new(evidence, checksums, manifest, 0))
    }
}

fn exact_files<'a>(
    files: &'a [DerivativeFileViewV1<'a>],
) -> Result<[DerivativeFileViewV1<'a>; EXPECTED_FILE_COUNT], DerivativeIoError> {
    if files.len() != EXPECTED_FILE_COUNT {
        return Err(DerivativeIoError);
    }
    let find = |name| {
        let mut matches = files.iter().copied().filter(|file| file.name() == name);
        let found = matches.next().ok_or(DerivativeIoError)?;
        if matches.next().is_some() {
            return Err(DerivativeIoError);
        }
        Ok(found)
    };
    Ok([
        find(DerivativeFileNameV1::Evidence)?,
        find(DerivativeFileNameV1::Checksums)?,
        find(DerivativeFileNameV1::Manifest)?,
    ])
}

fn validate_staging(
    staging: &Path,
    expected: &[DerivativeFileViewV1<'_>; EXPECTED_FILE_COUNT],
) -> Result<(), DerivativeIoError> {
    verify_private_root(staging).map_err(|_| DerivativeIoError)?;
    let mut seen = BTreeSet::new();
    for entry in fs::read_dir(staging).map_err(|_| DerivativeIoError)? {
        let entry = entry.map_err(|_| DerivativeIoError)?;
        reject_link_or_reparse(&entry.path()).map_err(|_| DerivativeIoError)?;
        let name = expected
            .iter()
            .find(|file| entry.file_name() == file.name().as_str())
            .map(|file| file.name())
            .ok_or(DerivativeIoError)?;
        if !seen.insert(name) {
            return Err(DerivativeIoError);
        }
    }
    if seen.len() != EXPECTED_FILE_COUNT {
        return Err(DerivativeIoError);
    }
    for file in expected {
        let mut remaining = file.contents().len();
        let actual = read_bounded(&staging.join(file.name().as_str()), &mut remaining)?;
        if remaining != 0 || actual != file.contents() {
            return Err(DerivativeIoError);
        }
    }
    verify_private_root(staging).map_err(|_| DerivativeIoError)
}

fn read_bounded(path: &Path, remaining: &mut usize) -> Result<Vec<u8>, DerivativeIoError> {
    reject_link_or_reparse(path).map_err(|_| DerivativeIoError)?;
    let file = open_regular_private_file(path).map_err(|_| DerivativeIoError)?;
    let metadata = file.metadata().map_err(|_| DerivativeIoError)?;
    if metadata.len() > u64::try_from(*remaining).unwrap_or(u64::MAX) {
        return Err(DerivativeIoError);
    }
    let limit = u64::try_from(*remaining)
        .unwrap_or(u64::MAX)
        .saturating_add(1);
    let mut bytes = Vec::with_capacity(usize::try_from(metadata.len()).unwrap_or(0));
    file.take(limit)
        .read_to_end(&mut bytes)
        .map_err(|_| DerivativeIoError)?;
    if bytes.len() > *remaining {
        return Err(DerivativeIoError);
    }
    *remaining -= bytes.len();
    Ok(bytes)
}

fn checked_absolute(path: &Path) -> Result<PathBuf, DerivativeIoError> {
    if path.as_os_str().is_empty() {
        return Err(DerivativeIoError);
    }
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|_| DerivativeIoError)?
            .join(path)
    };
    for component in path.components() {
        match component {
            Component::ParentDir => return Err(DerivativeIoError),
            Component::Normal(name) if !safe_normal_component(name) => {
                return Err(DerivativeIoError);
            }
            Component::Prefix(_)
            | Component::RootDir
            | Component::CurDir
            | Component::Normal(_) => {}
        }
    }
    Ok(path)
}

#[cfg(target_os = "windows")]
fn safe_normal_component(name: &std::ffi::OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    if name.is_empty()
        || name.encode_utf16().count() > MAX_PATH_COMPONENT_UNITS
        || name.ends_with(' ')
        || name.ends_with('.')
        || name.contains(':')
        || name.chars().any(char::is_control)
    {
        return false;
    }
    let device_stem = name.split('.').next().unwrap_or(name);
    !matches!(
        device_stem.to_ascii_uppercase().as_str(),
        "CON"
            | "CONIN$"
            | "CONOUT$"
            | "PRN"
            | "AUX"
            | "NUL"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
    )
}

#[cfg(all(unix, not(target_vendor = "apple")))]
fn safe_normal_component(name: &std::ffi::OsStr) -> bool {
    use std::os::unix::ffi::OsStrExt as _;

    !name.is_empty()
        && name.as_bytes().len() <= MAX_PATH_COMPONENT_UNITS
        && !name.as_bytes().iter().any(|byte| byte.is_ascii_control())
}

#[cfg(target_vendor = "apple")]
fn safe_normal_component(name: &std::ffi::OsStr) -> bool {
    name.to_str().is_some_and(|name| {
        !name.is_empty()
            && name.len() <= MAX_PATH_COMPONENT_UNITS
            && name.chars().all(|value| !value.is_control())
    })
}

#[cfg(not(any(unix, target_os = "windows", target_vendor = "apple")))]
fn safe_normal_component(name: &std::ffi::OsStr) -> bool {
    !name.is_empty()
        && name.len() <= MAX_PATH_COMPONENT_UNITS
        && name
            .to_string_lossy()
            .chars()
            .all(|value| !value.is_control())
}

fn is_expected_name(name: &std::ffi::OsStr) -> bool {
    [
        DerivativeFileNameV1::Evidence,
        DerivativeFileNameV1::Checksums,
        DerivativeFileNameV1::Manifest,
    ]
    .into_iter()
    .any(|expected| name == expected.as_str())
}

fn hex_ref(bytes: [u8; 16]) -> String {
    use fmt::Write as _;
    let mut output = String::with_capacity(32);
    for byte in bytes {
        write!(output, "{byte:02x}").expect("write random derivative suffix");
    }
    output
}

#[cfg(unix)]
fn create_private_directory(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt as _;

    let mut builder = fs::DirBuilder::new();
    builder.mode(0o700).create(path)
}

#[cfg(not(unix))]
fn create_private_directory(path: &Path) -> std::io::Result<()> {
    fs::create_dir(path)
}

#[cfg(any(target_vendor = "apple", target_os = "android", target_os = "linux"))]
fn rename_directory_no_replace(source: &Path, target: &Path) -> Result<(), DerivativeIoError> {
    use rustix::fs::{renameat_with, RenameFlags, CWD};

    renameat_with(CWD, source, CWD, target, RenameFlags::NOREPLACE).map_err(|_| DerivativeIoError)
}

#[cfg(not(any(target_vendor = "apple", target_os = "android", target_os = "linux")))]
fn rename_directory_no_replace(source: &Path, target: &Path) -> Result<(), DerivativeIoError> {
    // On Windows, `std::fs::rename` maps to a move without replacement flags:
    // an existing destination makes the syscall fail. The preflight check is
    // only an early diagnostic; correctness still comes from that syscall.
    if target.exists() {
        return Err(DerivativeIoError);
    }
    fs::rename(source, target).map_err(|_| DerivativeIoError)
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> std::io::Result<()> {
    fs::File::open(path)?.sync_all()
}

#[cfg(not(unix))]
fn sync_directory(path: &Path) -> std::io::Result<()> {
    fs::metadata(path).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files() -> [DerivativeFileViewV1<'static>; 3] {
        [
            DerivativeFileViewV1::new_for_store_test(
                DerivativeFileNameV1::Evidence,
                b"{\"schema_version\":1}\n",
            ),
            DerivativeFileViewV1::new_for_store_test(
                DerivativeFileNameV1::Checksums,
                b"checksums\n",
            ),
            DerivativeFileViewV1::new_for_store_test(
                DerivativeFileNameV1::Manifest,
                b"{\"manifest_version\":1}\n",
            ),
        ]
    }

    #[test]
    fn atomically_creates_and_reopens_exact_files() {
        let parent = tempfile::tempdir().unwrap();
        let target = parent.path().join("reviewable-derivative");
        let mut store = DerivativeDirectoryStoreV1::new(&target).unwrap();
        store.write_atomically(&files()).unwrap();

        let reopened = DerivativeDirectoryStoreV1::open_existing(&target).unwrap();
        let read = reopened.read_exact(1024).unwrap();
        assert_eq!(read.views()[0].contents(), files()[0].contents());
        assert_eq!(
            format!("{reopened:?}"),
            "DerivativeDirectoryStoreV1 { target: \"[private]\" }"
        );
    }

    #[test]
    fn existing_target_and_over_limit_reads_fail_closed() {
        let parent = tempfile::tempdir().unwrap();
        let target = parent.path().join("reviewable-derivative");
        let mut store = DerivativeDirectoryStoreV1::new(&target).unwrap();
        store.write_atomically(&files()).unwrap();
        assert!(DerivativeDirectoryStoreV1::new(&target).is_err());
        assert!(store.read_exact(8).is_err());
    }

    #[test]
    fn output_must_be_separate_from_sensitive_roots() {
        let parent = tempfile::tempdir().unwrap();
        let private_root = parent.path().join("private-captures");
        fs::create_dir(&private_root).unwrap();
        assert!(
            DerivativeDirectoryStoreV1::new_separate(parent.path().join("unchecked"), &[],)
                .is_err()
        );
        assert!(DerivativeDirectoryStoreV1::new_separate(
            private_root.join("derivative"),
            std::slice::from_ref(&private_root),
        )
        .is_err());
        let ancestor_target = parent.path().join("future-reviewable-root");
        assert!(DerivativeDirectoryStoreV1::new_separate(
            &ancestor_target,
            &[ancestor_target.join("future-private-child")],
        )
        .is_err());
        assert!(DerivativeDirectoryStoreV1::new_separate(
            parent.path().join("reviewable"),
            std::slice::from_ref(&private_root),
        )
        .is_ok());
        assert!(DerivativeDirectoryStoreV1::new_separate(
            parent.path().join("hostile\nname"),
            std::slice::from_ref(&private_root),
        )
        .is_err());
        assert!(DerivativeDirectoryStoreV1::new_separate(
            parent.path().join("x".repeat(MAX_PATH_COMPONENT_UNITS + 1)),
            std::slice::from_ref(&private_root),
        )
        .is_err());
        let filesystem_root = parent.path().ancestors().last().unwrap().to_path_buf();
        assert!(DerivativeDirectoryStoreV1::new_separate(
            parent.path().join("under-filesystem-root"),
            &[filesystem_root],
        )
        .is_err());
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_aliasing_and_device_components_are_rejected() {
        let parent = tempfile::tempdir().unwrap();
        let forbidden = parent.path().join("private");
        fs::create_dir(&forbidden).unwrap();
        for name in ["trailing.", "trailing ", "NUL", "con.txt", "stream:name"] {
            assert!(DerivativeDirectoryStoreV1::new_separate(
                parent.path().join(name),
                std::slice::from_ref(&forbidden),
            )
            .is_err());
        }
    }

    #[cfg(unix)]
    #[test]
    fn nonexistent_forbidden_child_is_resolved_through_a_symlinked_ancestor() {
        use std::os::unix::fs::symlink;

        let parent = tempfile::tempdir().unwrap();
        let real = parent.path().join("real");
        fs::create_dir(&real).unwrap();
        let alias = parent.path().join("alias");
        symlink(&real, &alias).unwrap();
        assert!(DerivativeDirectoryStoreV1::new_separate(
            real.join("derivative"),
            &[alias.join("derivative")],
        )
        .is_err());
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn nonexistent_forbidden_child_is_resolved_through_a_junction_ancestor() {
        let parent = tempfile::tempdir().unwrap();
        let real = parent.path().join("real");
        fs::create_dir(&real).unwrap();
        let alias = parent.path().join("alias");
        junction::create(&real, &alias).unwrap();
        assert!(DerivativeDirectoryStoreV1::new_separate(
            real.join("derivative"),
            &[alias.join("derivative")],
        )
        .is_err());
    }

    #[test]
    fn staging_directory_creation_never_adopts_an_existing_entry() {
        let parent = tempfile::tempdir().unwrap();
        let staging = parent.path().join("fixed.partial");
        create_private_directory(&staging).unwrap();
        assert_eq!(
            create_private_directory(&staging).unwrap_err().kind(),
            std::io::ErrorKind::AlreadyExists
        );
    }

    #[test]
    fn injected_rename_failure_leaves_no_published_target() {
        let parent = tempfile::tempdir().unwrap();
        let target = parent.path().join("reviewable-derivative");
        let mut store = DerivativeDirectoryStoreV1::new(&target).unwrap();

        assert_eq!(
            store.write_atomically_with_fault(&files(), PublicationFault::Rename),
            Err(DerivativeIoError)
        );
        assert!(!target.exists());
        assert!(fs::read_dir(parent.path()).unwrap().next().is_none());
    }

    #[test]
    fn staging_byte_mismatch_is_rejected_before_publication() {
        let parent = tempfile::tempdir().unwrap();
        let target = parent.path().join("reviewable-derivative");
        let mut store = DerivativeDirectoryStoreV1::new(&target).unwrap();

        assert_eq!(
            store.write_atomically_with_fault(&files(), PublicationFault::CorruptStaging),
            Err(DerivativeIoError)
        );
        assert!(!target.exists());
        assert!(fs::read_dir(parent.path()).unwrap().next().is_none());
    }

    #[test]
    fn injected_postcommit_failure_reports_committed_and_keeps_reviewable_target() {
        let parent = tempfile::tempdir().unwrap();
        let target = parent.path().join("reviewable-derivative");
        let mut store = DerivativeDirectoryStoreV1::new(&target).unwrap();

        assert_eq!(
            store
                .write_atomically_with_fault(&files(), PublicationFault::PostCommit)
                .unwrap(),
            DerivativeCommitStateV1::CommittedDurabilityUncertain
        );
        assert!(target.is_dir());
        let reopened = DerivativeDirectoryStoreV1::open_existing(&target).unwrap();
        let read = reopened.read_exact(1024).unwrap();
        assert_eq!(read.views()[0].contents(), files()[0].contents());
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_no_replace_rename_preserves_both_existing_directories() {
        let parent = tempfile::tempdir().unwrap();
        let source = parent.path().join("source");
        let target = parent.path().join("target");
        create_private_directory(&source).unwrap();
        create_private_directory(&target).unwrap();
        fs::write(source.join("source.txt"), b"source").unwrap();
        fs::write(target.join("target.txt"), b"target").unwrap();

        assert_eq!(
            rename_directory_no_replace(&source, &target),
            Err(DerivativeIoError)
        );
        assert_eq!(fs::read(source.join("source.txt")).unwrap(), b"source");
        assert_eq!(fs::read(target.join("target.txt")).unwrap(), b"target");
    }

    #[test]
    fn extra_entries_are_reported_without_reading_them() {
        let parent = tempfile::tempdir().unwrap();
        let target = parent.path().join("reviewable-derivative");
        let mut store = DerivativeDirectoryStoreV1::new(&target).unwrap();
        store.write_atomically(&files()).unwrap();
        let extra = target.join("unexpected.txt");
        let mut file = secure_new_file(&extra).unwrap();
        file.write_all(b"private and arbitrarily large").unwrap();
        file.sync_all().unwrap();

        let read = store.read_exact(1024).unwrap();
        assert_ne!(read.unexpected_entries(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn symlink_entry_is_rejected() {
        use std::os::unix::fs::symlink;

        let parent = tempfile::tempdir().unwrap();
        let target = parent.path().join("reviewable-derivative");
        let mut store = DerivativeDirectoryStoreV1::new(&target).unwrap();
        store.write_atomically(&files()).unwrap();
        let outside = parent.path().join("outside");
        fs::write(&outside, b"private").unwrap();
        symlink(outside, target.join("unexpected.txt")).unwrap();
        assert!(store.read_exact(1024).is_err());
    }
}
