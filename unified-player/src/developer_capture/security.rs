use std::{
    fmt, fs, io,
    path::{Component, Path, PathBuf},
};

use age::secrecy::SecretString;
use zeroize::{Zeroize as _, Zeroizing};

const MAX_PASSPHRASE_BYTES: usize = 1_024;
const ERASED_UTF8: &str = "\0\0\0\0";

pub(crate) struct CapturePassphrase(SecretString);

/// A TUI-only masked input buffer. It intentionally has no text accessor,
/// cloning, serialization, or clipboard integration.
pub(crate) struct CapturePassphraseInput(Zeroizing<String>);

impl CapturePassphraseInput {
    pub(crate) fn new() -> Self {
        // Reserve the complete bounded input once so growing the secret cannot
        // abandon plaintext in allocator-managed reallocation buffers.
        Self(Zeroizing::new(String::with_capacity(MAX_PASSPHRASE_BYTES)))
    }

    pub(crate) fn push(&mut self, character: char) -> bool {
        if character.is_control()
            || self.0.len().saturating_add(character.len_utf8()) > MAX_PASSPHRASE_BYTES
        {
            return false;
        }
        self.0.push(character);
        true
    }

    pub(crate) fn pop(&mut self) -> bool {
        if self.0.is_empty() {
            return false;
        }
        let mut new_len = self.0.len() - 1;
        while new_len > 0 && self.0.as_bytes()[new_len] & 0xc0 == 0x80 {
            new_len -= 1;
        }
        let removed_len = self.0.len() - new_len;
        self.0.replace_range(new_len.., &ERASED_UTF8[..removed_len]);
        self.0.truncate(new_len);
        true
    }

    pub(crate) fn character_count(&self) -> usize {
        self.0.chars().count()
    }

    pub(crate) fn into_passphrase(mut self) -> Result<CapturePassphrase, SecurityError> {
        CapturePassphrase::new(std::mem::take(&mut *self.0))
    }
}

impl Default for CapturePassphraseInput {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for CapturePassphraseInput {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("CapturePassphraseInput([private])")
    }
}

impl CapturePassphrase {
    pub(crate) fn new(mut value: String) -> Result<Self, SecurityError> {
        if value.is_empty() || value.len() > MAX_PASSPHRASE_BYTES {
            value.zeroize();
            return Err(SecurityError::InvalidPassphrase);
        }
        Ok(Self(SecretString::from(value)))
    }

    pub(super) fn age_secret(&self) -> SecretString {
        self.0.clone()
    }
}

impl fmt::Debug for CapturePassphrase {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("CapturePassphrase([private])")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SecurityError {
    InvalidPassphrase,
    Io,
    UnsafePath,
    InsecurePermissions,
}

impl fmt::Display for SecurityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidPassphrase => "private capture passphrase is invalid",
            Self::Io => "private capture storage operation failed",
            Self::UnsafePath => "private capture storage path is unsafe",
            Self::InsecurePermissions => "private capture storage permissions are not private",
        })
    }
}

impl std::error::Error for SecurityError {}

impl From<io::Error> for SecurityError {
    fn from(_: io::Error) -> Self {
        Self::Io
    }
}

pub(super) fn prepare_private_root(path: &Path) -> Result<(), SecurityError> {
    let path = checked_absolute_path(path)?;
    platform_prepare_private_root(&path)
}

/// Rechecks the complete root chain immediately around a path-based mutation.
///
/// Callers must not treat this as a persistent capability: once this function
/// returns, its ancestor handles are released. The store deliberately invokes
/// it immediately before and after operations that cannot yet be expressed
/// relative to a retained directory handle.
pub(super) fn verify_private_root(path: &Path) -> Result<(), SecurityError> {
    let path = checked_absolute_path(path)?;
    platform_verify_private_root(&path)
}

pub(super) fn secure_new_file(path: &Path) -> Result<fs::File, SecurityError> {
    let path = checked_absolute_path(path)?;
    platform_secure_new_file(&path)
}

pub(super) fn open_regular_private_file(path: &Path) -> Result<fs::File, SecurityError> {
    let path = checked_absolute_path(path)?;
    platform_open_regular_private_file(&path)
}

pub(super) fn verify_regular_private_file(path: &Path) -> Result<fs::Metadata, SecurityError> {
    open_regular_private_file(path)?
        .metadata()
        .map_err(Into::into)
}

pub(super) fn reject_link_or_reparse(path: &Path) -> Result<(), SecurityError> {
    let path = checked_absolute_path(path)?;
    platform_reject_link_or_reparse(&path)
}

/// Rejects both containment directions after resolving every existing prefix.
///
/// The candidate may not exist yet. Resolving its nearest existing ancestor is
/// what makes a symlink, junction, reparse-point, or differently-spelled alias
/// comparable before the private root is created.
pub(crate) fn ensure_private_root_separate(
    candidate: &Path,
    ordinary_roots: &[PathBuf],
) -> Result<(), SecurityError> {
    if ordinary_roots.is_empty() {
        return Err(SecurityError::UnsafePath);
    }
    let candidate = resolve_existing_prefix(&checked_absolute_path(candidate)?)?;
    for root in ordinary_roots {
        let root = resolve_existing_prefix(&checked_absolute_path(root)?)?;
        if paths_overlap(&candidate, &root) {
            return Err(SecurityError::UnsafePath);
        }
    }
    Ok(())
}

fn checked_absolute_path(path: &Path) -> Result<PathBuf, SecurityError> {
    if path.as_os_str().is_empty() {
        return Err(SecurityError::UnsafePath);
    }
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut has_normal_component = false;
    for component in absolute.components() {
        match component {
            Component::ParentDir => return Err(SecurityError::UnsafePath),
            Component::Normal(_) => has_normal_component = true,
            Component::Prefix(_) | Component::RootDir | Component::CurDir => {}
        }
    }
    if !absolute.is_absolute() || !has_normal_component {
        return Err(SecurityError::UnsafePath);
    }
    Ok(absolute)
}

fn resolve_existing_prefix(path: &Path) -> Result<PathBuf, SecurityError> {
    let mut existing = path;
    let mut suffix = Vec::new();
    while !existing.exists() {
        suffix.push(
            existing
                .file_name()
                .ok_or(SecurityError::UnsafePath)?
                .to_os_string(),
        );
        existing = existing.parent().ok_or(SecurityError::UnsafePath)?;
    }
    let mut resolved = fs::canonicalize(existing).map_err(|_| SecurityError::Io)?;
    for component in suffix.into_iter().rev() {
        resolved.push(component);
    }
    Ok(resolved)
}

#[cfg(any(target_os = "windows", target_vendor = "apple"))]
fn paths_overlap(left: &Path, right: &Path) -> bool {
    fn normalized(path: &Path) -> Vec<String> {
        path.components()
            .map(|component| component.as_os_str().to_string_lossy().to_lowercase())
            .collect()
    }

    let left = normalized(left);
    let right = normalized(right);
    left.starts_with(&right) || right.starts_with(&left)
}

#[cfg(not(any(target_os = "windows", target_vendor = "apple")))]
fn paths_overlap(left: &Path, right: &Path) -> bool {
    left.starts_with(right) || right.starts_with(left)
}

fn checked_parent_and_name(path: &Path) -> Result<(&Path, &std::ffi::OsStr), SecurityError> {
    let parent = path.parent().ok_or(SecurityError::UnsafePath)?;
    let name = path.file_name().ok_or(SecurityError::UnsafePath)?;
    if name.is_empty() {
        return Err(SecurityError::UnsafePath);
    }
    Ok((parent, name))
}

#[cfg(unix)]
fn platform_prepare_private_root(path: &Path) -> Result<(), SecurityError> {
    use rustix::fs::{fchmod, Mode};

    let directory = unix_open_directory_chain(path, true)?;
    fchmod(&directory, Mode::from_raw_mode(0o700)).map_err(|_| SecurityError::Io)?;
    let file = fs::File::from(directory);
    verify_unix_handle(&file, true)
}

#[cfg(unix)]
fn platform_verify_private_root(path: &Path) -> Result<(), SecurityError> {
    let directory = unix_open_directory_chain(path, false)?;
    let file = fs::File::from(directory);
    verify_unix_handle(&file, true)
}

#[cfg(unix)]
fn platform_secure_new_file(path: &Path) -> Result<fs::File, SecurityError> {
    use rustix::fs::{fchmod, openat, Mode, OFlags};

    let (parent, name) = checked_parent_and_name(path)?;
    let parent = unix_open_directory_chain(parent, false)?;
    let descriptor = openat(
        &parent,
        name,
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::from_raw_mode(0o600),
    )
    .map_err(map_unix_path_error)?;
    fchmod(&descriptor, Mode::from_raw_mode(0o600)).map_err(|_| SecurityError::Io)?;
    let file = fs::File::from(descriptor);
    verify_unix_handle(&file, false)?;
    Ok(file)
}

#[cfg(unix)]
fn platform_open_regular_private_file(path: &Path) -> Result<fs::File, SecurityError> {
    use rustix::fs::{openat, Mode, OFlags};

    let (parent, name) = checked_parent_and_name(path)?;
    let parent = unix_open_directory_chain(parent, false)?;
    let descriptor = openat(
        &parent,
        name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(map_unix_path_error)?;
    let file = fs::File::from(descriptor);
    verify_unix_handle(&file, false)?;
    Ok(file)
}

#[cfg(unix)]
fn platform_reject_link_or_reparse(path: &Path) -> Result<(), SecurityError> {
    use rustix::fs::{statat, AtFlags, FileType};

    let (parent, name) = checked_parent_and_name(path)?;
    let parent = unix_open_directory_chain(parent, false)?;
    let metadata = statat(&parent, name, AtFlags::SYMLINK_NOFOLLOW).map_err(map_unix_path_error)?;
    if FileType::from_raw_mode(metadata.st_mode) == FileType::Symlink {
        return Err(SecurityError::UnsafePath);
    }
    Ok(())
}

#[cfg(unix)]
fn unix_open_directory_chain(
    path: &Path,
    create_missing: bool,
) -> Result<rustix::fd::OwnedFd, SecurityError> {
    use rustix::{
        fs::{mkdirat, openat, Mode, OFlags, CWD},
        io::Errno,
    };

    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let mut current =
        openat(CWD, Path::new("/"), flags, Mode::empty()).map_err(|_| SecurityError::Io)?;
    let mut opened_component = false;
    for component in path.components() {
        let Component::Normal(name) = component else {
            continue;
        };
        opened_component = true;
        let next = match openat(&current, name, flags, Mode::empty()) {
            Ok(next) => next,
            Err(error) if create_missing && error == Errno::NOENT => {
                match mkdirat(&current, name, Mode::from_raw_mode(0o700)) {
                    Ok(()) => {}
                    Err(error) if error == Errno::EXIST => {}
                    Err(_) => return Err(SecurityError::Io),
                }
                openat(&current, name, flags, Mode::empty()).map_err(map_unix_path_error)?
            }
            Err(error) => return Err(map_unix_path_error(error)),
        };
        current = next;
    }
    if !opened_component {
        return Err(SecurityError::UnsafePath);
    }
    Ok(current)
}

#[cfg(unix)]
fn verify_unix_handle(file: &fs::File, directory: bool) -> Result<(), SecurityError> {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

    let metadata = file.metadata()?;
    if (directory && !metadata.is_dir()) || (!directory && !metadata.is_file()) {
        return Err(SecurityError::UnsafePath);
    }
    let expected = if directory { 0o700 } else { 0o600 };
    if metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.permissions().mode() & 0o777 != expected
    {
        return Err(SecurityError::InsecurePermissions);
    }
    Ok(())
}

#[cfg(unix)]
fn map_unix_path_error(error: rustix::io::Errno) -> SecurityError {
    use rustix::io::Errno;

    if matches!(error, Errno::LOOP | Errno::NOTDIR) {
        SecurityError::UnsafePath
    } else {
        SecurityError::Io
    }
}

#[cfg(target_os = "windows")]
fn platform_prepare_private_root(path: &Path) -> Result<(), SecurityError> {
    let mut chain = windows_open_directory_chain(path, true, true)?;
    let root = chain.last_mut().ok_or(SecurityError::UnsafePath)?;
    apply_windows_permissions(root, true)?;
    verify_windows_handle(root, true)
}

#[cfg(target_os = "windows")]
fn platform_verify_private_root(path: &Path) -> Result<(), SecurityError> {
    let chain = windows_open_directory_chain(path, false, false)?;
    let root = chain.last().ok_or(SecurityError::UnsafePath)?;
    verify_windows_handle(root, true)
}

#[cfg(target_os = "windows")]
fn platform_secure_new_file(path: &Path) -> Result<fs::File, SecurityError> {
    use std::os::windows::fs::OpenOptionsExt;

    const FILE_GENERIC_WRITE: u32 = 0x0012_0116;
    const READ_CONTROL: u32 = 0x0002_0000;
    const WRITE_DAC: u32 = 0x0004_0000;
    const FILE_SHARE_READ: u32 = 0x1;
    const FILE_SHARE_WRITE: u32 = 0x2;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;

    let (parent, _) = checked_parent_and_name(path)?;
    let _ancestor_guards = windows_open_directory_chain(parent, false, false)?;
    let mut options = fs::OpenOptions::new();
    options
        .write(true)
        .create_new(true)
        .access_mode(FILE_GENERIC_WRITE | READ_CONTROL | WRITE_DAC)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    let mut file = options.open(path)?;
    reject_windows_handle_reparse(&file, false)?;
    apply_windows_permissions(&mut file, false)?;
    if let Err(error) = verify_windows_handle(&file, false) {
        drop(file);
        let _ = fs::remove_file(path);
        return Err(error);
    }
    Ok(file)
}

#[cfg(target_os = "windows")]
fn platform_open_regular_private_file(path: &Path) -> Result<fs::File, SecurityError> {
    let (parent, _) = checked_parent_and_name(path)?;
    let _ancestor_guards = windows_open_directory_chain(parent, false, false)?;
    let file = windows_open_existing(path, false, false)?;
    reject_windows_handle_reparse(&file, false)?;
    verify_windows_handle(&file, false)?;
    Ok(file)
}

#[cfg(target_os = "windows")]
fn platform_reject_link_or_reparse(path: &Path) -> Result<(), SecurityError> {
    let (parent, _) = checked_parent_and_name(path)?;
    let _ancestor_guards = windows_open_directory_chain(parent, false, false)?;
    let file = windows_open_existing(path, false, true)?;
    reject_windows_handle_reparse(&file, file.metadata()?.is_dir())
}

#[cfg(target_os = "windows")]
fn windows_open_directory_chain(
    path: &Path,
    create_missing: bool,
    write_final_dacl: bool,
) -> Result<Vec<fs::File>, SecurityError> {
    let paths = windows_component_paths(path)?;
    let final_index = paths.len().saturating_sub(1);
    let mut handles = Vec::with_capacity(paths.len());
    for (index, component_path) in paths.iter().enumerate() {
        let write_dacl = write_final_dacl && index == final_index;
        let (mut handle, created) = match windows_open_existing(component_path, write_dacl, true) {
            Ok(handle) => (handle, false),
            Err(SecurityError::Io) if create_missing => {
                windows_create_private_directory(component_path)?;
                (windows_open_existing(component_path, true, true)?, true)
            }
            Err(error) => return Err(error),
        };
        reject_windows_handle_reparse(&handle, true)?;
        if created {
            apply_windows_permissions(&mut handle, true)?;
            verify_windows_handle(&handle, true)?;
        }
        handles.push(handle);
    }
    if handles.is_empty() {
        return Err(SecurityError::UnsafePath);
    }
    Ok(handles)
}

#[cfg(target_os = "windows")]
fn windows_component_paths(path: &Path) -> Result<Vec<PathBuf>, SecurityError> {
    let mut current = PathBuf::new();
    let mut paths = Vec::new();
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir => current.push(component.as_os_str()),
            Component::Normal(_) => {
                current.push(component.as_os_str());
                paths.push(current.clone());
            }
            Component::CurDir => {}
            Component::ParentDir => return Err(SecurityError::UnsafePath),
        }
    }
    if paths.is_empty() {
        return Err(SecurityError::UnsafePath);
    }
    Ok(paths)
}

#[cfg(target_os = "windows")]
fn windows_open_existing(
    path: &Path,
    write_dacl: bool,
    directory: bool,
) -> Result<fs::File, SecurityError> {
    use std::os::windows::fs::OpenOptionsExt;

    const FILE_GENERIC_READ: u32 = 0x0012_0089;
    const READ_CONTROL: u32 = 0x0002_0000;
    const WRITE_DAC: u32 = 0x0004_0000;
    const FILE_SHARE_READ: u32 = 0x1;
    const FILE_SHARE_WRITE: u32 = 0x2;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;

    let access = if write_dacl {
        READ_CONTROL | WRITE_DAC
    } else if directory {
        READ_CONTROL
    } else {
        FILE_GENERIC_READ | READ_CONTROL
    };
    let mut flags = FILE_FLAG_OPEN_REPARSE_POINT;
    if directory {
        flags |= FILE_FLAG_BACKUP_SEMANTICS;
    }
    let mut options = fs::OpenOptions::new();
    options
        .access_mode(access)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(flags);
    options.open(path).map_err(SecurityError::from)
}

#[cfg(target_os = "windows")]
fn windows_create_private_directory(path: &Path) -> Result<(), SecurityError> {
    match fs::create_dir(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(_) => Err(SecurityError::Io),
    }
}

#[cfg(target_os = "windows")]
fn windows_private_descriptor(
    directory: bool,
) -> Result<windows_permissions::LocalBox<windows_permissions::SecurityDescriptor>, SecurityError> {
    use windows_permissions::{
        utilities::current_process_sid,
        wrappers::{ConvertSidToStringSid, ConvertStringSecurityDescriptorToSecurityDescriptor},
    };

    let current_sid = current_process_sid().map_err(|_| SecurityError::InsecurePermissions)?;
    let sid = ConvertSidToStringSid(&current_sid)
        .map_err(|_| SecurityError::InsecurePermissions)?
        .to_string_lossy()
        .into_owned();
    let inheritance = if directory { "OICI" } else { "" };
    let sddl = format!("D:P(A;{inheritance};FA;;;{sid})");
    ConvertStringSecurityDescriptorToSecurityDescriptor(&sddl)
        .map_err(|_| SecurityError::InsecurePermissions)
}

#[cfg(target_os = "windows")]
fn apply_windows_permissions(file: &mut fs::File, directory: bool) -> Result<(), SecurityError> {
    use windows_permissions::{
        constants::{SeObjectType, SecurityInformation},
        wrappers::SetSecurityInfo,
    };

    let descriptor = windows_private_descriptor(directory)?;
    let dacl = descriptor
        .dacl()
        .ok_or(SecurityError::InsecurePermissions)?;
    SetSecurityInfo(
        file,
        SeObjectType::SE_FILE_OBJECT,
        SecurityInformation::Dacl | SecurityInformation::ProtectedDacl,
        None,
        None,
        Some(dacl),
        None,
    )
    .map_err(|_| SecurityError::InsecurePermissions)
}

#[cfg(target_os = "windows")]
fn verify_windows_handle(file: &fs::File, directory: bool) -> Result<(), SecurityError> {
    use windows_permissions::{
        constants::{AccessRights, AceFlags, AceType, SeObjectType, SecurityInformation},
        utilities::current_process_sid,
        wrappers::{
            ConvertSecurityDescriptorToStringSecurityDescriptor, EqualSid, GetSecurityInfo,
        },
    };

    reject_windows_handle_reparse(file, directory)?;
    let current_sid = current_process_sid().map_err(|_| SecurityError::InsecurePermissions)?;
    let descriptor = GetSecurityInfo(
        file,
        SeObjectType::SE_FILE_OBJECT,
        SecurityInformation::Owner | SecurityInformation::Dacl,
    )
    .map_err(|_| SecurityError::InsecurePermissions)?;
    let dacl_sddl =
        ConvertSecurityDescriptorToStringSecurityDescriptor(&descriptor, SecurityInformation::Dacl)
            .map_err(|_| SecurityError::InsecurePermissions)?;
    if !dacl_sddl.to_string_lossy().starts_with("D:P") {
        return Err(SecurityError::InsecurePermissions);
    }
    let owner = descriptor
        .owner()
        .ok_or(SecurityError::InsecurePermissions)?;
    let dacl = descriptor
        .dacl()
        .ok_or(SecurityError::InsecurePermissions)?;
    if !EqualSid(owner, &current_sid) || dacl.len() != 1 {
        return Err(SecurityError::InsecurePermissions);
    }
    let ace = dacl.get_ace(0).ok_or(SecurityError::InsecurePermissions)?;
    let ace_sid = ace.sid().ok_or(SecurityError::InsecurePermissions)?;
    let expected_flags = if directory {
        AceFlags::ContainerInherit | AceFlags::ObjectInherit
    } else {
        AceFlags::empty()
    };
    if ace.ace_type() != AceType::ACCESS_ALLOWED_ACE_TYPE
        || ace.flags() != expected_flags
        || ace.mask() != AccessRights::FileAllAccess
        || !EqualSid(ace_sid, &current_sid)
    {
        return Err(SecurityError::InsecurePermissions);
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn reject_windows_handle_reparse(
    file: &fs::File,
    expected_directory: bool,
) -> Result<(), SecurityError> {
    use std::os::windows::fs::MetadataExt;

    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    let metadata = file.metadata()?;
    if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
        || (expected_directory && !metadata.is_dir())
        || (!expected_directory && !metadata.is_file())
    {
        return Err(SecurityError::UnsafePath);
    }
    Ok(())
}

#[cfg(not(any(unix, target_os = "windows")))]
fn platform_prepare_private_root(_: &Path) -> Result<(), SecurityError> {
    Err(SecurityError::InsecurePermissions)
}

#[cfg(not(any(unix, target_os = "windows")))]
fn platform_verify_private_root(_: &Path) -> Result<(), SecurityError> {
    Err(SecurityError::InsecurePermissions)
}

#[cfg(not(any(unix, target_os = "windows")))]
fn platform_secure_new_file(_: &Path) -> Result<fs::File, SecurityError> {
    Err(SecurityError::InsecurePermissions)
}

#[cfg(not(any(unix, target_os = "windows")))]
fn platform_open_regular_private_file(_: &Path) -> Result<fs::File, SecurityError> {
    Err(SecurityError::InsecurePermissions)
}

#[cfg(not(any(unix, target_os = "windows")))]
fn platform_reject_link_or_reparse(_: &Path) -> Result<(), SecurityError> {
    Err(SecurityError::InsecurePermissions)
}

#[cfg(test)]
mod tests {
    use super::{
        ensure_private_root_separate, prepare_private_root, secure_new_file, verify_private_root,
        verify_regular_private_file, CapturePassphrase, CapturePassphraseInput, SecurityError,
        MAX_PASSPHRASE_BYTES,
    };
    use static_assertions::assert_not_impl_any;

    assert_not_impl_any!(CapturePassphrase: std::fmt::Display, serde::Serialize);
    assert_not_impl_any!(CapturePassphraseInput: Clone, std::fmt::Display, serde::Serialize);

    #[test]
    fn passphrase_is_redacted_and_bounded() {
        let passphrase = CapturePassphrase::new("seeded-private-passphrase".to_owned()).unwrap();
        assert_eq!(format!("{passphrase:?}"), "CapturePassphrase([private])");
        assert!(CapturePassphrase::new(String::new()).is_err());
        assert!(CapturePassphrase::new("x".repeat(1_025)).is_err());
    }

    #[test]
    fn masked_input_has_no_plaintext_observation_api() {
        let mut input = CapturePassphraseInput::new();
        assert!(input.push('s'));
        assert!(input.push('\u{15f}'));
        assert!(!input.push('\n'));
        assert_eq!(input.character_count(), 2);
        assert_eq!(format!("{input:?}"), "CapturePassphraseInput([private])");
        assert!(input.pop());
        input.into_passphrase().unwrap();
    }

    #[test]
    fn masked_input_never_reallocates_secret_bytes_and_erases_before_truncating() {
        let mut input = CapturePassphraseInput::new();
        let allocation = input.0.as_ptr();
        for _ in 0..MAX_PASSPHRASE_BYTES {
            assert!(input.push('x'));
        }
        assert_eq!(input.0.as_ptr(), allocation);
        assert!(!input.push('x'));
        while input.pop() {}
        assert!(input.0.is_empty());
        assert_eq!(input.0.as_ptr(), allocation);
    }

    #[test]
    fn private_root_and_file_permissions_are_enforced() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("vault");
        prepare_private_root(&root).unwrap();
        verify_private_root(&root).unwrap();
        let path = root.join("fixture.partial");
        let file = secure_new_file(&path).unwrap();
        file.sync_all().unwrap();
        verify_regular_private_file(&path).unwrap();
    }

    #[test]
    fn lexical_parent_components_are_rejected() {
        let parent = tempfile::tempdir().unwrap();
        let path = parent.path().join("missing").join("..").join("vault");
        assert_eq!(prepare_private_root(&path), Err(SecurityError::UnsafePath));
        assert!(!parent.path().join("vault").exists());
    }

    #[test]
    fn private_root_separation_is_symmetric_and_accepts_only_siblings() {
        let parent = tempfile::tempdir().unwrap();
        let ordinary = parent.path().join("ordinary");
        std::fs::create_dir(&ordinary).unwrap();
        let sibling = parent.path().join("private");

        ensure_private_root_separate(&sibling, std::slice::from_ref(&ordinary)).unwrap();
        assert_eq!(
            ensure_private_root_separate(
                &ordinary.join("nested-private"),
                std::slice::from_ref(&ordinary)
            ),
            Err(SecurityError::UnsafePath)
        );
        assert_eq!(
            ensure_private_root_separate(parent.path(), std::slice::from_ref(&ordinary)),
            Err(SecurityError::UnsafePath)
        );
    }

    #[cfg(unix)]
    #[test]
    fn private_root_separation_resolves_symlink_aliases() {
        use std::os::unix::fs::symlink;

        let parent = tempfile::tempdir().unwrap();
        let ordinary = parent.path().join("ordinary");
        std::fs::create_dir(&ordinary).unwrap();
        let alias = parent.path().join("ordinary-alias");
        symlink(&ordinary, &alias).unwrap();

        assert_eq!(
            ensure_private_root_separate(&ordinary.join("private"), std::slice::from_ref(&alias)),
            Err(SecurityError::UnsafePath)
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn private_root_separation_resolves_junction_aliases() {
        let parent = tempfile::tempdir().unwrap();
        let ordinary = parent.path().join("ordinary");
        std::fs::create_dir(&ordinary).unwrap();
        let alias = parent.path().join("ordinary-alias");
        junction::create(&ordinary, &alias).unwrap();

        assert_eq!(
            ensure_private_root_separate(&ordinary.join("private"), std::slice::from_ref(&alias)),
            Err(SecurityError::UnsafePath)
        );
    }

    #[cfg(unix)]
    #[test]
    fn final_and_ancestor_links_are_rejected() {
        use std::os::unix::fs::symlink;

        let parent = tempfile::tempdir().unwrap();
        let target = parent.path().join("target");
        std::fs::create_dir(&target).unwrap();
        let link = parent.path().join("link");
        symlink(&target, &link).unwrap();
        assert_eq!(prepare_private_root(&link), Err(SecurityError::UnsafePath));
        assert_eq!(
            prepare_private_root(&link.join("nested-vault")),
            Err(SecurityError::UnsafePath)
        );
        assert!(!target.join("nested-vault").exists());
    }

    #[cfg(unix)]
    #[test]
    fn unix_permission_regressions_are_rejected_and_repaired() {
        use std::os::unix::fs::PermissionsExt;

        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("vault");
        prepare_private_root(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            verify_private_root(&root),
            Err(SecurityError::InsecurePermissions)
        );
        prepare_private_root(&root).unwrap();
        verify_private_root(&root).unwrap();
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_junction_ancestors_are_rejected_before_creation() {
        let parent = tempfile::tempdir().unwrap();
        let target = parent.path().join("target");
        std::fs::create_dir(&target).unwrap();
        let junction = parent.path().join("junction");
        junction::create(&target, &junction).unwrap();
        assert_eq!(
            prepare_private_root(&junction.join("nested-vault")),
            Err(SecurityError::UnsafePath)
        );
        assert!(!target.join("nested-vault").exists());
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_hostile_inheritance_is_replaced_with_a_protected_exact_dacl() {
        let parent = tempfile::tempdir().unwrap();
        set_windows_dacl(parent.path(), "D:(A;OICI;GA;;;WD)", false);
        let root = parent.path().join("vault");
        prepare_private_root(&root).unwrap();
        verify_private_root(&root).unwrap();
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_unprotected_or_underprivileged_dacls_are_rejected() {
        use windows_permissions::{
            utilities::current_process_sid, wrappers::ConvertSidToStringSid,
        };

        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("vault");
        prepare_private_root(&root).unwrap();
        let sid = ConvertSidToStringSid(&current_process_sid().unwrap())
            .unwrap()
            .to_string_lossy()
            .into_owned();
        set_windows_dacl(&root, &format!("D:(A;OICI;FA;;;{sid})"), false);
        assert_eq!(
            verify_private_root(&root),
            Err(SecurityError::InsecurePermissions)
        );
        set_windows_dacl(&root, &format!("D:P(A;OICI;FR;;;{sid})"), true);
        assert_eq!(
            verify_private_root(&root),
            Err(SecurityError::InsecurePermissions)
        );
        prepare_private_root(&root).unwrap();
    }

    #[cfg(target_os = "windows")]
    fn set_windows_dacl(path: &std::path::Path, sddl: &str, protected: bool) {
        use windows_permissions::{
            constants::{SeObjectType, SecurityInformation},
            wrappers::{ConvertStringSecurityDescriptorToSecurityDescriptor, SetNamedSecurityInfo},
        };

        let descriptor = ConvertStringSecurityDescriptorToSecurityDescriptor(sddl).unwrap();
        let dacl = descriptor.dacl().unwrap();
        let protection = if protected {
            SecurityInformation::ProtectedDacl
        } else {
            SecurityInformation::UnprotectedDacl
        };
        SetNamedSecurityInfo(
            path.as_os_str(),
            SeObjectType::SE_FILE_OBJECT,
            SecurityInformation::Dacl | protection,
            None,
            None,
            Some(dacl),
            None,
        )
        .unwrap();
    }
}
