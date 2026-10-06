#[cfg(windows)]
pub(crate) use super::windows::persist_files::{
    check_persist_source, create_persist_temporary, discard_persist_temporary,
    prepare_persist_metadata, publish_persist_recovery,
};

use std::fs::File;
use std::{io, path::Path};

pub(crate) fn same_persist_file(first: &File, second: &File) -> io::Result<bool> {
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{
            GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
        };
        let identity = |file: &File| {
            let mut information = BY_HANDLE_FILE_INFORMATION::default();
            if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut information) } == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok((
                information.dwVolumeSerialNumber,
                information.nFileIndexHigh,
                information.nFileIndexLow,
            ))
        };
        Ok(identity(first)? == identity(second)?)
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let first = first.metadata()?;
        let second = second.metadata()?;
        Ok((first.dev(), first.ino()) == (second.dev(), second.ino()))
    }
}

#[cfg(not(windows))]
pub(crate) fn create_persist_temporary(path: &Path) -> io::Result<File> {
    super::create_config_temporary(path, true)
}

#[cfg(not(windows))]
pub(crate) fn check_persist_source(source: &File) -> io::Result<()> {
    let metadata = source.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "persist source is not a regular file",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.nlink() > 1 {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "persist source has multiple hard links",
            ));
        }
    }
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) fn prepare_persist_metadata(source: &File, temp: &File) -> io::Result<()> {
    check_persist_source(source)?;
    if source.metadata()?.permissions().readonly() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "persist source is readonly",
        ));
    }
    use std::os::unix::fs::MetadataExt;
    let original = source.metadata()?;
    let attributes = persist_xattrs(source)?;
    if original.mode() & 0o6000 != 0
        || [
            b"security.capability\0".as_slice(),
            b"security.ima\0",
            b"security.evm\0",
        ]
        .iter()
        .any(|name| attributes.contains_key(*name))
    {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "persist file has execution privileges or content-bound integrity metadata",
        ));
    }
    #[cfg(target_os = "macos")]
    let acl = persist_acl(source)?;
    super::prepare_config_metadata(source, temp)?;
    let copied = temp.metadata()?;
    if (original.uid(), original.gid(), original.mode())
        != (copied.uid(), copied.gid(), copied.mode())
        || attributes != persist_xattrs(temp)?
    {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "persist permissions or extended attributes could not be preserved",
        ));
    }
    #[cfg(target_os = "macos")]
    if acl != persist_acl(temp)? {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "persist ACL could not be preserved",
        ));
    }
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn persist_xattrs(file: &File) -> io::Result<std::collections::BTreeMap<Vec<u8>, Vec<u8>>> {
    use std::os::fd::AsRawFd;
    fn read(mut query: impl FnMut(*mut u8, usize) -> isize) -> io::Result<Vec<u8>> {
        let size = query(std::ptr::null_mut(), 0);
        if size < 0 {
            return Err(io::Error::last_os_error());
        }
        if size > 16 * 1024 * 1024 {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "persist extended attribute exceeds safety limit",
            ));
        }
        let mut bytes = vec![0; size as usize];
        let count = query(bytes.as_mut_ptr(), bytes.len());
        if count < 0 {
            return Err(io::Error::last_os_error());
        }
        if count as usize > bytes.len() {
            return Err(io::Error::other(
                "persist extended attributes changed during preparation",
            ));
        }
        bytes.truncate(count as usize);
        Ok(bytes)
    }
    let names = read(|buffer, size| {
        #[cfg(target_os = "linux")]
        {
            unsafe { libc::flistxattr(file.as_raw_fd(), buffer.cast(), size) }
        }
        #[cfg(target_os = "macos")]
        {
            unsafe { libc::flistxattr(file.as_raw_fd(), buffer.cast(), size, 0) }
        }
    });
    let names = match names {
        Err(error) if error.raw_os_error() == Some(libc::ENOTSUP) => return Ok(Default::default()),
        other => other?,
    };
    let mut attributes = std::collections::BTreeMap::new();
    for bytes in names.split_inclusive(|byte| *byte == 0) {
        let name = std::ffi::CStr::from_bytes_with_nul(bytes).map_err(io::Error::other)?;
        let value = read(|buffer, size| {
            #[cfg(target_os = "linux")]
            {
                unsafe { libc::fgetxattr(file.as_raw_fd(), name.as_ptr(), buffer.cast(), size) }
            }
            #[cfg(target_os = "macos")]
            {
                unsafe {
                    libc::fgetxattr(file.as_raw_fd(), name.as_ptr(), buffer.cast(), size, 0, 0)
                }
            }
        })?;
        attributes.insert(bytes.to_vec(), value);
    }
    Ok(attributes)
}

#[cfg(target_os = "macos")]
fn persist_acl(file: &File) -> io::Result<Vec<u8>> {
    use std::os::fd::AsRawFd;
    unsafe extern "C" {
        fn acl_get_fd_np(fd: libc::c_int, kind: libc::c_int) -> *mut libc::c_void;
        fn acl_to_text(acl: *mut libc::c_void, length: *mut libc::ssize_t) -> *mut libc::c_char;
        fn acl_free(pointer: *mut libc::c_void) -> libc::c_int;
    }
    let acl = unsafe { acl_get_fd_np(file.as_raw_fd(), 0x100) };
    if acl.is_null() {
        return Err(io::Error::last_os_error());
    }
    let mut length = 0;
    let text = unsafe { acl_to_text(acl, &mut length) };
    let result = if text.is_null() {
        Err(io::Error::last_os_error())
    } else if length < 0 {
        Err(io::Error::other("invalid persist ACL text length"))
    } else {
        Ok(unsafe { std::slice::from_raw_parts(text.cast(), length as usize) }.to_vec())
    };
    unsafe {
        acl_free(acl);
        if !text.is_null() {
            acl_free(text.cast());
        }
    }
    result
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
pub(crate) fn prepare_persist_metadata(_source: &File, _temp: &File) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "persist permission preservation is unsupported on this platform",
    ))
}

#[cfg(unix)]
pub(crate) fn discard_persist_temporary(temp: &File, path: &Path) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    let owned = temp.metadata()?;
    let current = std::fs::symlink_metadata(path)?;
    if (owned.dev(), owned.ino()) != (current.dev(), current.ino()) {
        return Err(io::Error::other(
            "persist temporary path no longer names the owned file",
        ));
    }
    std::fs::remove_file(path)
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn discard_persist_temporary(_temp: &File, _path: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "persist temporary cleanup is unsupported on this platform",
    ))
}

pub(crate) fn sync_directory(directory: &Path) -> io::Result<()> {
    #[cfg(windows)]
    {
        super::windows::flush_directory(directory)
    }
    #[cfg(not(windows))]
    {
        File::open(directory)?.sync_all()
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) fn publish_persist_recovery(pending: &Path, backup: &Path) -> io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let pending = std::ffi::CString::new(pending.as_os_str().as_bytes())?;
    let backup = std::ffi::CString::new(backup.as_os_str().as_bytes())?;
    #[cfg(target_os = "linux")]
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            pending.as_ptr(),
            libc::AT_FDCWD,
            backup.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    #[cfg(target_os = "macos")]
    let result = unsafe { libc::renamex_np(pending.as_ptr(), backup.as_ptr(), libc::RENAME_EXCL) };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
pub(crate) fn publish_persist_recovery(_pending: &Path, _backup: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "exclusive persist recovery publication is unsupported on this platform",
    ))
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};

    struct Directory(std::path::PathBuf);
    impl Directory {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "herdr-persist-{}-{}",
                std::process::id(),
                crate::config::test_dirs::unique_id()
            ));
            std::fs::DirBuilder::new()
                .mode(0o700)
                .create(&path)
                .unwrap();
            Self(path)
        }
    }
    impl Drop for Directory {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn persist_unix_metadata_and_private_creation() {
        use std::os::fd::AsRawFd;
        let dir = Directory::new();
        let source_path = dir.0.join("source");
        let mut source = create_persist_temporary(&source_path).unwrap();
        source.write_all(b"original").unwrap();
        source
            .set_permissions(std::fs::Permissions::from_mode(0o640))
            .unwrap();
        let name = c"user.herdr-test";
        #[cfg(target_os = "linux")]
        let result = unsafe {
            libc::fsetxattr(
                source.as_raw_fd(),
                name.as_ptr(),
                b"value".as_ptr().cast(),
                5,
                0,
            )
        };
        #[cfg(target_os = "macos")]
        let result = unsafe {
            libc::fsetxattr(
                source.as_raw_fd(),
                name.as_ptr(),
                b"value".as_ptr().cast(),
                5,
                0,
                0,
            )
        };
        assert_eq!(result, 0, "{}", io::Error::last_os_error());
        let pending = dir.0.join("pending");
        let mut temp = create_persist_temporary(&pending).unwrap();
        assert_eq!(temp.metadata().unwrap().mode() & 0o777, 0o600);
        assert_eq!(
            create_persist_temporary(&pending).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        prepare_persist_metadata(&source, &temp).unwrap();
        assert_eq!(
            source.metadata().unwrap().mode(),
            temp.metadata().unwrap().mode()
        );
        assert_eq!(
            persist_xattrs(&source).unwrap(),
            persist_xattrs(&temp).unwrap()
        );
        temp.write_all(b"new").unwrap();
        temp.sync_all().unwrap();
        std::fs::rename(&pending, &source_path).unwrap();
        sync_directory(&dir.0).unwrap();
        assert_eq!(std::fs::read(&source_path).unwrap(), b"new");
    }

    #[test]
    fn persist_unix_recovery_is_exclusive_and_cleanup_checks_identity() {
        let dir = Directory::new();
        let pending = dir.0.join("pending");
        let backup = dir.0.join("backup");
        let mut temp = create_persist_temporary(&pending).unwrap();
        temp.write_all(b"complete").unwrap();
        temp.sync_all().unwrap();
        std::fs::write(&backup, b"foreign").unwrap();
        assert!(publish_persist_recovery(&pending, &backup).is_err());
        assert_eq!(std::fs::read(&backup).unwrap(), b"foreign");
        std::fs::remove_file(&backup).unwrap();
        publish_persist_recovery(&pending, &backup).unwrap();
        sync_directory(&dir.0).unwrap();
        assert!(!pending.exists());
        assert_eq!(std::fs::read(&backup).unwrap(), b"complete");
        std::fs::write(&pending, b"foreign").unwrap();
        assert!(discard_persist_temporary(&temp, &pending).is_err());
        assert_eq!(std::fs::read(&pending).unwrap(), b"foreign");
    }

    #[test]
    fn persist_unix_readonly_and_hard_links_are_not_updated() {
        let dir = Directory::new();
        let source_path = dir.0.join("source");
        let source = create_persist_temporary(&source_path).unwrap();
        source
            .set_permissions(std::fs::Permissions::from_mode(0o400))
            .unwrap();
        check_persist_source(&source).unwrap();
        let pending = dir.0.join("pending");
        let temp = create_persist_temporary(&pending).unwrap();
        assert_eq!(
            prepare_persist_metadata(&source, &temp).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        source
            .set_permissions(std::fs::Permissions::from_mode(0o4600))
            .unwrap();
        assert_eq!(
            prepare_persist_metadata(&source, &temp).unwrap_err().kind(),
            io::ErrorKind::Unsupported
        );
        source
            .set_permissions(std::fs::Permissions::from_mode(0o600))
            .unwrap();
        std::fs::hard_link(&source_path, dir.0.join("link")).unwrap();
        assert_eq!(
            check_persist_source(&source).unwrap_err().kind(),
            io::ErrorKind::Unsupported
        );
        discard_persist_temporary(&temp, &pending).unwrap();
        assert!(!pending.exists());
    }
}

#[cfg(test)]
mod identity_tests {
    use super::*;

    #[test]
    fn persist_identity_distinguishes_equal_bytes_after_replacement() {
        let root = crate::config::test_dirs::TempDir::new("persist-identity");
        let path = root.path().join("original");
        let replacement = root.path().join("replacement");
        std::fs::write(&path, b"same bytes").unwrap();
        let original = File::open(&path).unwrap();
        let original_again = File::open(&path).unwrap();
        assert!(same_persist_file(&original, &original_again).unwrap());
        std::fs::write(&replacement, b"same bytes").unwrap();
        std::fs::rename(&replacement, &path).unwrap();
        let replaced = File::open(&path).unwrap();
        assert!(!same_persist_file(&original, &replaced).unwrap());
        assert!(same_persist_file(&original, &original_again).unwrap());
    }
}
