use std::fs::File;
use std::io;
use std::mem::size_of;
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::path::Path;
use std::ptr::{null_mut, NonNull};
use windows_sys::Win32::{
    Foundation::{LocalFree, GENERIC_WRITE, INVALID_HANDLE_VALUE},
    Security::{
        Authorization::{GetSecurityInfo, SetSecurityInfo, SE_FILE_OBJECT},
        GetAce, GetLengthSid, GetSecurityDescriptorControl, IsValidAcl, ACE_HEADER, ACL,
        ATTRIBUTE_SECURITY_INFORMATION, DACL_SECURITY_INFORMATION, GROUP_SECURITY_INFORMATION,
        LABEL_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION,
        PROTECTED_DACL_SECURITY_INFORMATION, SCOPE_SECURITY_INFORMATION, SECURITY_ATTRIBUTES,
        SE_DACL_PROTECTED, UNPROTECTED_DACL_SECURITY_INFORMATION,
    },
    Storage::FileSystem::{
        CreateFileW, FileBasicInfo, FileDispositionInfoEx, GetFileInformationByHandleEx,
        SetFileInformationByHandle, CREATE_NEW, DELETE, FILE_ATTRIBUTE_HIDDEN,
        FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_NOT_CONTENT_INDEXED, FILE_ATTRIBUTE_SYSTEM,
        FILE_BASIC_INFO, FILE_DISPOSITION_FLAG_DELETE,
        FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE, FILE_DISPOSITION_FLAG_POSIX_SEMANTICS,
        FILE_DISPOSITION_INFO_EX, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ,
        FILE_SHARE_WRITE, READ_CONTROL, WRITE_DAC, WRITE_OWNER,
    },
    System::SystemServices::{
        ACCESS_FILTER_SECURITY_INFORMATION, PROCESS_TRUST_LABEL_SECURITY_INFORMATION,
        SYSTEM_MANDATORY_LABEL_ACE_TYPE,
    },
};

const PERSIST_SECURITY_INFORMATION: u32 = OWNER_SECURITY_INFORMATION
    | GROUP_SECURITY_INFORMATION
    | DACL_SECURITY_INFORMATION
    | LABEL_SECURITY_INFORMATION
    | ATTRIBUTE_SECURITY_INFORMATION
    | SCOPE_SECURITY_INFORMATION
    | PROCESS_TRUST_LABEL_SECURITY_INFORMATION as u32
    | ACCESS_FILTER_SECURITY_INFORMATION as u32;
const PERSIST_BASIC_ATTRIBUTES: u32 =
    FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_SYSTEM | FILE_ATTRIBUTE_NOT_CONTENT_INDEXED;

fn basic_information(file: &File) -> io::Result<FILE_BASIC_INFO> {
    let mut information = FILE_BASIC_INFO::default();
    if unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileBasicInfo,
            std::ptr::from_mut(&mut information).cast(),
            size_of::<FILE_BASIC_INFO>() as u32,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(information)
}

fn prepare_basic_attributes(source: &File, temp: &File) -> io::Result<()> {
    let expected = basic_information(source)?.FileAttributes & PERSIST_BASIC_ATTRIBUTES;
    let current = basic_information(temp)?.FileAttributes;
    if current & PERSIST_BASIC_ATTRIBUTES != expected {
        let attributes = (current & !(PERSIST_BASIC_ATTRIBUTES | FILE_ATTRIBUTE_NORMAL)) | expected;
        let information = FILE_BASIC_INFO {
            FileAttributes: if attributes == 0 {
                FILE_ATTRIBUTE_NORMAL
            } else {
                attributes
            },
            ..Default::default()
        };
        if unsafe {
            SetFileInformationByHandle(
                temp.as_raw_handle(),
                FileBasicInfo,
                std::ptr::from_ref(&information).cast(),
                size_of::<FILE_BASIC_INFO>() as u32,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
    }
    if basic_information(temp)?.FileAttributes & PERSIST_BASIC_ATTRIBUTES != expected {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "persist file attributes could not be preserved",
        ));
    }
    Ok(())
}

pub(crate) fn create_persist_temporary(path: &Path) -> io::Result<File> {
    use interprocess::os::windows::security_descriptor::AsSecurityDescriptorExt as _;
    let descriptor = super::user_security_descriptor("GA", "")?;
    let mut attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: null_mut(),
        bInheritHandle: 0,
    };
    descriptor.write_to_security_attributes(&mut attributes);
    let wide = super::extended_length_path(path)?;
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            GENERIC_WRITE | READ_CONTROL | WRITE_DAC | WRITE_OWNER | FILE_READ_ATTRIBUTES | DELETE,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            &attributes,
            CREATE_NEW,
            FILE_ATTRIBUTE_NORMAL,
            null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    let file = unsafe { File::from_raw_handle(handle) };
    if let Err(error) = check_persist_source(&file) {
        if let Err(cleanup) = discard_persist_temporary(&file, path) {
            return Err(io::Error::other(format!(
                "{error}; cannot remove owned temporary {}: {cleanup}",
                path.display()
            )));
        }
        return Err(error);
    }
    Ok(file)
}

pub(crate) fn check_persist_source(source: &File) -> io::Result<()> {
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_ARCHIVE,
        FILE_ATTRIBUTE_ENCRYPTED, FILE_ATTRIBUTE_READONLY,
    };
    if !source.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "persist source is not a regular file",
        ));
    }
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    if unsafe { GetFileInformationByHandle(source.as_raw_handle(), &mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    if info.dwFileAttributes & FILE_ATTRIBUTE_ENCRYPTED != 0 {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "encrypted persist files cannot be safely copied",
        ));
    }
    if info.dwFileAttributes
        & !(FILE_ATTRIBUTE_ARCHIVE
            | FILE_ATTRIBUTE_NORMAL
            | FILE_ATTRIBUTE_READONLY
            | PERSIST_BASIC_ATTRIBUTES)
        != 0
    {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "persist file has unsupported additional attributes",
        ));
    }
    if info.nNumberOfLinks > 1 {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "persist file has multiple hard links",
        ));
    }
    Security::read(source)?;
    check_streams(source)
}

fn check_streams(source: &File) -> io::Result<()> {
    use windows_sys::Win32::{
        Foundation::{ERROR_INSUFFICIENT_BUFFER, ERROR_MORE_DATA},
        Storage::FileSystem::{FileStreamInfo, GetFileInformationByHandleEx},
    };
    let mut capacity = 4096;
    loop {
        let mut storage = vec![0_u64; capacity / size_of::<u64>()];
        if unsafe {
            GetFileInformationByHandleEx(
                source.as_raw_handle(),
                FileStreamInfo,
                storage.as_mut_ptr().cast(),
                capacity as u32,
            )
        } != 0
        {
            let bytes =
                unsafe { std::slice::from_raw_parts(storage.as_ptr().cast::<u8>(), capacity) };
            return validate_streams(bytes);
        }
        let error = io::Error::last_os_error();
        if !matches!(
            error.raw_os_error().map(|code| code as u32),
            Some(ERROR_MORE_DATA | ERROR_INSUFFICIENT_BUFFER)
        ) {
            return Err(error);
        }
        if capacity == 1024 * 1024 {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "persist stream information exceeds safety limit",
            ));
        }
        capacity *= 2;
    }
}

fn validate_streams(bytes: &[u8]) -> io::Result<()> {
    let invalid = || {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid persist stream information",
        )
    };
    let mut offset = 0usize;
    let mut unnamed = false;
    loop {
        let header = bytes
            .get(offset..offset.checked_add(24).ok_or_else(invalid)?)
            .ok_or_else(invalid)?;
        let next = u32::from_le_bytes(header[..4].try_into().map_err(|_| invalid())?) as usize;
        let length = u32::from_le_bytes(header[4..8].try_into().map_err(|_| invalid())?) as usize;
        if length == 0 || !length.is_multiple_of(2) {
            return Err(invalid());
        }
        let end = offset
            .checked_add(24)
            .and_then(|start| start.checked_add(length))
            .ok_or_else(invalid)?;
        let name = bytes.get(offset + 24..end).ok_or_else(invalid)?;
        if name != b":\0:\0$\0D\0A\0T\0A\0" {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "named streams on persist files cannot be safely copied",
            ));
        }
        if unnamed {
            return Err(invalid());
        }
        unnamed = true;
        if next == 0 {
            return Ok(());
        }
        if !next.is_multiple_of(8) || next < 24 + length {
            return Err(invalid());
        }
        offset = offset.checked_add(next).ok_or_else(invalid)?;
    }
}

pub(crate) fn publish_persist_recovery(pending: &Path, backup: &Path) -> io::Result<()> {
    let pending = super::extended_length_path(pending)?;
    let backup = super::extended_length_path(backup)?;
    if unsafe {
        windows_sys::Win32::Storage::FileSystem::MoveFileExW(pending.as_ptr(), backup.as_ptr(), 0)
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub(crate) fn discard_persist_temporary(temp: &File, _path: &Path) -> io::Result<()> {
    let disposition = FILE_DISPOSITION_INFO_EX {
        Flags: FILE_DISPOSITION_FLAG_DELETE
            | FILE_DISPOSITION_FLAG_POSIX_SEMANTICS
            | FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE,
    };
    if unsafe {
        SetFileInformationByHandle(
            temp.as_raw_handle(),
            FileDispositionInfoEx,
            std::ptr::from_ref(&disposition).cast(),
            size_of::<FILE_DISPOSITION_INFO_EX>() as u32,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn reject_unsupported_authorization(acl: *mut ACL) -> io::Result<()> {
    if acl.is_null() {
        return Ok(());
    }
    if unsafe { IsValidAcl(acl) } == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid persist authorization ACL",
        ));
    }
    for index in 0..unsafe { (*acl).AceCount } {
        let mut ace = null_mut();
        if unsafe { GetAce(acl, u32::from(index), &mut ace) } == 0 {
            return Err(io::Error::last_os_error());
        }
        if u32::from(unsafe { (*ace.cast::<ACE_HEADER>()).AceType })
            != SYSTEM_MANDATORY_LABEL_ACE_TYPE
        {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "persist file has unsupported non-audit authorization metadata",
            ));
        }
    }
    Ok(())
}

struct Security {
    descriptor: NonNull<std::ffi::c_void>,
    owner: *mut std::ffi::c_void,
    group: *mut std::ffi::c_void,
    dacl: *mut ACL,
    label: *mut ACL,
    control: u16,
}

impl Drop for Security {
    fn drop(&mut self) {
        unsafe { LocalFree(self.descriptor.as_ptr()) };
    }
}

impl Security {
    fn read(file: &File) -> io::Result<Self> {
        let mut owner = null_mut();
        let mut group = null_mut();
        let mut dacl = null_mut();
        let mut label = null_mut();
        let mut descriptor = null_mut();
        let status = unsafe {
            GetSecurityInfo(
                file.as_raw_handle(),
                SE_FILE_OBJECT,
                PERSIST_SECURITY_INFORMATION,
                &mut owner,
                &mut group,
                &mut dacl,
                &mut label,
                &mut descriptor,
            )
        };
        if status != 0 {
            return Err(io::Error::from_raw_os_error(status as i32));
        }
        let descriptor = NonNull::new(descriptor)
            .ok_or_else(|| io::Error::other("missing file security descriptor"))?;
        let mut security = Self {
            descriptor,
            owner,
            group,
            dacl,
            label,
            control: 0,
        };
        let mut revision = 0;
        if unsafe {
            GetSecurityDescriptorControl(
                security.descriptor.as_ptr(),
                &mut security.control,
                &mut revision,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        reject_unsupported_authorization(security.label)?;
        Ok(security)
    }

    fn semantic(&self) -> io::Result<SecurityValue> {
        fn sid(sid: *mut std::ffi::c_void) -> io::Result<Vec<u8>> {
            if sid.is_null() {
                return Err(io::Error::other("missing file owner or group SID"));
            }
            let length = unsafe { GetLengthSid(sid) } as usize;
            Ok(unsafe { std::slice::from_raw_parts(sid.cast(), length) }.to_vec())
        }
        fn acl(acl: *mut ACL) -> io::Result<Option<Vec<Vec<u8>>>> {
            if acl.is_null() {
                return Ok(None);
            }
            let mut entries = Vec::new();
            for index in 0..unsafe { (*acl).AceCount } {
                let mut ace = null_mut();
                if unsafe { GetAce(acl, u32::from(index), &mut ace) } == 0 {
                    return Err(io::Error::last_os_error());
                }
                let header = unsafe { &*ace.cast::<ACE_HEADER>() };
                entries.push(
                    unsafe { std::slice::from_raw_parts(ace.cast(), usize::from(header.AceSize)) }
                        .to_vec(),
                );
            }
            Ok(Some(entries))
        }
        Ok(SecurityValue {
            owner: sid(self.owner)?,
            group: sid(self.group)?,
            dacl: acl(self.dacl)?,
            label: acl(self.label)?,
            control: self.control & !windows_sys::Win32::Security::SE_SELF_RELATIVE,
        })
    }
}

#[derive(Debug, PartialEq, Eq)]
struct SecurityValue {
    owner: Vec<u8>,
    group: Vec<u8>,
    dacl: Option<Vec<Vec<u8>>>,
    label: Option<Vec<Vec<u8>>>,
    control: u16,
}

#[cfg(test)]
fn metadata_comparison_diagnostic(stage: &str, expected: &SecurityValue, actual: &SecurityValue) {
    eprintln!(
        "persist.metadata stage={stage} expected_control={:#06x} actual_control={:#06x} \
         control_xor={:#06x} owner_eq={} group_eq={} dacl_eq={} label_eq={} \
         expected_dacl_aces={:?} actual_dacl_aces={:?} \
         expected_label_aces={:?} actual_label_aces={:?}",
        expected.control,
        actual.control,
        expected.control ^ actual.control,
        expected.owner == actual.owner,
        expected.group == actual.group,
        expected.dacl == actual.dacl,
        expected.label == actual.label,
        expected.dacl.as_ref().map(Vec::len),
        actual.dacl.as_ref().map(Vec::len),
        expected.label.as_ref().map(Vec::len),
        actual.label.as_ref().map(Vec::len),
    );
}

pub(crate) fn prepare_persist_metadata(source: &File, temp: &File) -> io::Result<()> {
    macro_rules! stage {
        ($stage:literal, $result:expr) => {{
            let result = $result;
            #[cfg(test)]
            let result = result.inspect_err(|error| {
                eprintln!(
                    "persist.metadata stage={} error={error} debug={error:?} kind={:?} raw_os_error={:?} security_read_information={PERSIST_SECURITY_INFORMATION:#010x}",
                    $stage,
                    error.kind(),
                    error.raw_os_error(),
                );
            });
            result
        }};
    }

    stage!("source.check", check_persist_source(source))?;
    stage!("temp.check", check_persist_source(temp))?;
    if stage!("source.metadata", source.metadata())?
        .permissions()
        .readonly()
    {
        return stage!(
            "source.readonly",
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "persist source is readonly",
            ))
        );
    }
    let original = stage!("source.security.read", Security::read(source))?;
    let expected = stage!("source.security.semantic", original.semantic())?;
    let current = stage!(
        "temp.security.semantic",
        stage!("temp.security.read", Security::read(temp))?.semantic()
    )?;
    let mut information = 0;
    if expected.owner != current.owner {
        information |= OWNER_SECURITY_INFORMATION;
    }
    if expected.group != current.group {
        information |= GROUP_SECURITY_INFORMATION;
    }
    if expected.label != current.label {
        information |= LABEL_SECURITY_INFORMATION;
    }
    use windows_sys::Win32::Security::{
        SE_DACL_AUTO_INHERITED, SE_DACL_AUTO_INHERIT_REQ, SE_DACL_DEFAULTED, SE_DACL_PRESENT,
    };
    let dacl_control = SE_DACL_PRESENT
        | SE_DACL_DEFAULTED
        | SE_DACL_PROTECTED
        | SE_DACL_AUTO_INHERITED
        | SE_DACL_AUTO_INHERIT_REQ;
    if expected.dacl != current.dacl
        || expected.control & dacl_control != current.control & dacl_control
    {
        information |= DACL_SECURITY_INFORMATION;
        information |= if original.control & SE_DACL_PROTECTED != 0 {
            PROTECTED_DACL_SECURITY_INFORMATION
        } else {
            UNPROTECTED_DACL_SECURITY_INFORMATION
        };
    }
    #[cfg(test)]
    metadata_comparison_diagnostic("before_set", &expected, &current);
    if information != 0 {
        let status = unsafe {
            SetSecurityInfo(
                temp.as_raw_handle(),
                SE_FILE_OBJECT,
                information,
                original.owner,
                original.group,
                original.dacl,
                original.label,
            )
        };
        #[cfg(test)]
        eprintln!("persist.metadata stage=security.set information={information:#010x} status={status:#010x}");
        if status != 0 {
            return stage!(
                "security.set",
                Err(io::Error::from_raw_os_error(status as i32))
            );
        }
    }
    #[cfg(test)]
    if information == 0 {
        eprintln!("persist.metadata stage=security.set information=0x00000000 skipped=true");
    }
    let actual = stage!(
        "readback.security.semantic",
        stage!("readback.security.read", Security::read(temp))?.semantic()
    )?;
    if actual != expected {
        #[cfg(test)]
        metadata_comparison_diagnostic("readback.strict_compare", &expected, &actual);
        return stage!(
            "readback.strict_compare",
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "file permissions cannot be preserved exactly during replacement",
            ))
        );
    }
    stage!("attributes.prepare", prepare_basic_attributes(source, temp))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    struct Directory(std::path::PathBuf);
    impl Directory {
        fn new(case: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "herdr-persist-{case}-{}-{}",
                std::process::id(),
                crate::config::test_dirs::unique_id()
            ));
            use interprocess::os::windows::security_descriptor::AsSecurityDescriptorExt as _;
            use windows_sys::Win32::Security::{
                GetSecurityDescriptorDacl, CONTAINER_INHERIT_ACE, OBJECT_INHERIT_ACE,
            };
            let descriptor = super::super::user_security_descriptor("GA", "").unwrap();
            let mut attributes = SECURITY_ATTRIBUTES {
                nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: null_mut(),
                bInheritHandle: 0,
            };
            descriptor.write_to_security_attributes(&mut attributes);
            let mut present = 0;
            let mut defaulted = 0;
            let mut dacl = null_mut();
            assert_ne!(
                unsafe {
                    GetSecurityDescriptorDacl(
                        attributes.lpSecurityDescriptor,
                        &mut present,
                        &mut dacl,
                        &mut defaulted,
                    )
                },
                0
            );
            assert!(!dacl.is_null());
            for index in 0..unsafe { (*dacl).AceCount } {
                let mut ace = null_mut();
                assert_ne!(unsafe { GetAce(dacl, u32::from(index), &mut ace) }, 0);
                unsafe {
                    (*ace.cast::<ACE_HEADER>()).AceFlags |=
                        (CONTAINER_INHERIT_ACE | OBJECT_INHERIT_ACE) as u8;
                }
            }
            let wide = super::super::extended_length_path(&path).unwrap();
            assert_ne!(
                unsafe {
                    windows_sys::Win32::Storage::FileSystem::CreateDirectoryW(
                        wide.as_ptr(),
                        &attributes,
                    )
                },
                0
            );
            use std::os::windows::fs::OpenOptionsExt;
            let directory = std::fs::OpenOptions::new()
                .access_mode(WRITE_DAC | READ_CONTROL)
                .custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS)
                .open(&path)
                .unwrap();
            assert_eq!(
                unsafe {
                    SetSecurityInfo(
                        directory.as_raw_handle(),
                        SE_FILE_OBJECT,
                        DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                        null_mut(),
                        null_mut(),
                        dacl,
                        null_mut(),
                    )
                },
                0
            );
            assert!(String::from_utf16_lossy(&security(&path)).contains("D:PAI"));
            Self(path)
        }
    }
    impl Drop for Directory {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn persist_permissions_native_private_and_inherited() {
        let dir = Directory::new("ordinary");
        for private in [false, true] {
            let source_path = dir.0.join(format!("source-{private}"));
            let mut source = super::super::create_config_temporary(&source_path, private).unwrap();
            source.write_all(b"original").unwrap();
            drop(source);
            let source = File::open(&source_path).unwrap();
            let before = Security::read(&source).unwrap().semantic().unwrap();
            let temp_path = dir.0.join(format!("temp-{private}"));
            let mut temp = create_persist_temporary(&temp_path).unwrap();
            assert_eq!(
                create_persist_temporary(&temp_path).unwrap_err().kind(),
                io::ErrorKind::AlreadyExists
            );
            prepare_persist_metadata(&source, &temp).unwrap_or_else(|error| {
                panic!(
                    "private={private}: {error}; expected={before:?}; actual={:?}",
                    Security::read(&temp).unwrap().semantic().unwrap()
                )
            });
            assert_eq!(Security::read(&temp).unwrap().semantic().unwrap(), before);
            temp.write_all(b"replacement").unwrap();
            temp.sync_all().unwrap();
            std::fs::rename(&temp_path, &source_path).unwrap();
            assert_eq!(std::fs::read(&source_path).unwrap(), b"replacement");
            assert_eq!(
                Security::read(&File::open(&source_path).unwrap())
                    .unwrap()
                    .semantic()
                    .unwrap(),
                before
            );
        }
    }

    #[test]
    fn persist_permissions_native_cleanup_by_owned_handle() {
        let dir = Directory::new("cleanup");
        let path = dir.0.join("temp");
        let temp = create_persist_temporary(&path).unwrap();
        let moved = dir.0.join("owned-moved");
        std::fs::rename(&path, &moved).unwrap();
        std::fs::write(&path, b"foreign").unwrap();
        let mut permissions = temp.metadata().unwrap().permissions();
        permissions.set_readonly(true);
        temp.set_permissions(permissions).unwrap();
        discard_persist_temporary(&temp, &path).unwrap();
        drop(temp);
        assert!(!moved.exists());
        assert_eq!(std::fs::read(&path).unwrap(), b"foreign");
    }

    fn powershell(script: &str, path: &Path) {
        let output = std::process::Command::new("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-Command", script])
            .env("HERDR_TEST_PERSIST_SOURCE", path)
            .output()
            .unwrap();
        assert!(
            output.status.success() && output.stderr.is_empty(),
            "{output:?}"
        );
    }

    fn security(path: &Path) -> Vec<u16> {
        let flags = OWNER_SECURITY_INFORMATION
            | GROUP_SECURITY_INFORMATION
            | DACL_SECURITY_INFORMATION
            | LABEL_SECURITY_INFORMATION;
        let mut descriptor = super::super::config_security_descriptor(path, flags).unwrap();
        super::super::config_security_sddl(&mut descriptor, flags).unwrap()
    }

    fn descriptor_case(case: &str, supported: bool) {
        use windows_sys::Win32::Security::{
            SetFileSecurityW, SetSecurityDescriptorControl, SE_DACL_AUTO_INHERITED,
            SE_DACL_AUTO_INHERIT_REQ,
        };
        let dir = Directory::new(case);
        let source_path = dir.0.join("source");
        std::fs::write(&source_path, b"original").unwrap();
        if matches!(case, "legacy" | "legacy-save") {
            let mut descriptor =
                super::super::config_security_descriptor(&source_path, DACL_SECURITY_INFORMATION)
                    .unwrap();
            assert_ne!(
                unsafe {
                    SetSecurityDescriptorControl(
                        descriptor.as_mut_ptr().cast(),
                        SE_DACL_AUTO_INHERITED | SE_DACL_AUTO_INHERIT_REQ | SE_DACL_PROTECTED,
                        0,
                    )
                },
                0
            );
            let path = super::super::extended_length_path(&source_path).unwrap();
            assert_ne!(
                unsafe {
                    SetFileSecurityW(
                        path.as_ptr(),
                        DACL_SECURITY_INFORMATION,
                        descriptor.as_mut_ptr().cast(),
                    )
                },
                0
            );
        }
        if matches!(case, "protected" | "unprotected" | "moved" | "restricted") {
            powershell(
                &format!(
                    r#"
$ErrorActionPreference = 'Stop'
$acl = [System.IO.File]::GetAccessControl($env:HERDR_TEST_PERSIST_SOURCE)
$acl.SetAccessRuleProtection(${}, $false)
$sid = [System.Security.Principal.WindowsIdentity]::GetCurrent().User
$rule = [System.Security.AccessControl.FileSystemAccessRule]::new($sid, [System.Security.AccessControl.FileSystemRights]::{}, [System.Security.AccessControl.AccessControlType]::Allow)
$acl.AddAccessRule($rule)
[System.IO.File]::SetAccessControl($env:HERDR_TEST_PERSIST_SOURCE, $acl)
"#,
                    matches!(case, "protected" | "restricted"),
                    if case == "restricted" {
                        "ReadAndExecute"
                    } else {
                        "FullControl"
                    }
                ),
                &source_path,
            );
        }
        if case == "metadata" {
            powershell(
                r#"
$ErrorActionPreference = 'Stop'
$acl = [System.IO.File]::GetAccessControl($env:HERDR_TEST_PERSIST_SOURCE)
$acl.SetOwner([System.Security.Principal.WindowsIdentity]::GetCurrent().User)
$acl.SetGroup([System.Security.Principal.SecurityIdentifier]::new('S-1-5-32-545'))
[System.IO.File]::SetAccessControl($env:HERDR_TEST_PERSIST_SOURCE, $acl)
$null = & icacls.exe $env:HERDR_TEST_PERSIST_SOURCE /setintegritylevel L
if ($LASTEXITCODE -ne 0) { throw 'could not install low integrity label' }
"#,
                &source_path,
            );
        }
        let source_path = if case == "moved" {
            let before = security(&source_path);
            let parent = dir.0.join("broader-parent");
            std::fs::create_dir(&parent).unwrap();
            powershell(
                r#"
$ErrorActionPreference = 'Stop'
$acl = [System.IO.Directory]::GetAccessControl($env:HERDR_TEST_PERSIST_SOURCE)
$sid = [System.Security.Principal.SecurityIdentifier]::new('S-1-5-32-546')
$rule = [System.Security.AccessControl.FileSystemAccessRule]::new($sid, [System.Security.AccessControl.FileSystemRights]::Read, [System.Security.AccessControl.InheritanceFlags]::ObjectInherit, [System.Security.AccessControl.PropagationFlags]::None, [System.Security.AccessControl.AccessControlType]::Allow)
$acl.AddAccessRule($rule)
[System.IO.Directory]::SetAccessControl($env:HERDR_TEST_PERSIST_SOURCE, $acl)
"#,
                &parent,
            );
            let moved = parent.join("source");
            std::fs::rename(&source_path, &moved).unwrap();
            assert_eq!(security(&moved), before);
            let inherited = parent.join("inherited");
            std::fs::write(&inherited, b"").unwrap();
            assert!(String::from_utf16_lossy(&security(&inherited)).contains(";;;BG)"));
            moved
        } else {
            source_path
        };
        let before = security(&source_path);
        let text = String::from_utf16_lossy(&before);
        match case {
            "legacy" | "legacy-save" => assert!(text.contains("D:("), "{text}"),
            "protected" | "restricted" => assert!(text.contains("D:PAI"), "{text}"),
            "unprotected" | "moved" => assert!(text.contains("D:AI"), "{text}"),
            "metadata" => assert!(text.contains("G:BU") && text.contains(";;;LW)"), "{text}"),
            _ => unreachable!(),
        }
        let source = File::open(&source_path).unwrap();
        if case == "legacy-save" {
            let expected = Security::read(&source).unwrap().semantic().unwrap();
            crate::persist::assert_legacy_save_rejected(&source_path);
            assert_eq!(security(&source_path), before);
            assert_eq!(
                Security::read(&File::open(&source_path).unwrap())
                    .unwrap()
                    .semantic()
                    .unwrap(),
                expected
            );
            return;
        }
        let temp_path = source_path.with_file_name("pending");
        let mut temp = create_persist_temporary(&temp_path).unwrap();
        let result = prepare_persist_metadata(&source, &temp);
        if supported {
            result.unwrap_or_else(|error| {
                panic!(
                    "{case}: {error}; original={:?}; current={:?}",
                    Security::read(&source).unwrap().semantic().unwrap(),
                    Security::read(&temp).unwrap().semantic().unwrap()
                )
            });
            assert_eq!(security(&temp_path), before);
            temp.write_all(b"new").unwrap();
            temp.sync_all().unwrap();
        } else {
            assert_eq!(result.unwrap_err().kind(), io::ErrorKind::Unsupported);
            assert_eq!(temp.metadata().unwrap().len(), 0);
        }
        assert_eq!(std::fs::read(&source_path).unwrap(), b"original");
        assert_eq!(security(&source_path), before);
        discard_persist_temporary(&temp, &temp_path).unwrap();
        drop(temp);
        assert!(!temp_path.exists());
    }

    #[test]
    fn persist_native_protected() {
        descriptor_case("protected", true);
    }
    #[test]
    fn persist_native_unprotected() {
        descriptor_case("unprotected", true);
    }
    #[test]
    fn persist_native_legacy_rejected() {
        descriptor_case("legacy", false);
    }
    #[test]
    fn persist_native_legacy_save_rejected_before_write() {
        descriptor_case("legacy-save", false);
    }
    #[test]
    fn persist_native_moved_rejected() {
        descriptor_case("moved", false);
    }
    #[test]
    fn persist_native_owner_group_low_label() {
        descriptor_case("metadata", true);
    }
    #[test]
    fn persist_native_restrictive_cleanup() {
        descriptor_case("restricted", true);
    }

    #[test]
    fn persist_native_private_low_label_does_not_rewrite_dacl() {
        let dir = Directory::new("private-label");
        let path = dir.0.join("source");
        let mut source = create_persist_temporary(&path).unwrap();
        source.write_all(b"original").unwrap();
        powershell(
            r#"
$ErrorActionPreference = 'Stop'
$null = & icacls.exe $env:HERDR_TEST_PERSIST_SOURCE /setintegritylevel L
if ($LASTEXITCODE -ne 0) { throw 'could not install low integrity label' }
"#,
            &path,
        );
        use windows_sys::Win32::Security::{
            SetFileSecurityW, SetSecurityDescriptorControl, SE_DACL_AUTO_INHERITED,
            SE_DACL_AUTO_INHERIT_REQ,
        };
        let mut descriptor =
            super::super::config_security_descriptor(&path, DACL_SECURITY_INFORMATION).unwrap();
        assert_ne!(
            unsafe {
                SetSecurityDescriptorControl(
                    descriptor.as_mut_ptr().cast(),
                    SE_DACL_AUTO_INHERITED | SE_DACL_AUTO_INHERIT_REQ,
                    0,
                )
            },
            0
        );
        let wide = super::super::extended_length_path(&path).unwrap();
        assert_ne!(
            unsafe {
                SetFileSecurityW(
                    wide.as_ptr(),
                    DACL_SECURITY_INFORMATION,
                    descriptor.as_mut_ptr().cast(),
                )
            },
            0
        );
        let before = security(&path);
        let text = String::from_utf16_lossy(&before);
        assert!(text.contains("D:P(") && text.contains(";;;LW)"), "{text}");
        let pending = dir.0.join("pending");
        let mut temp = create_persist_temporary(&pending).unwrap();
        prepare_persist_metadata(&source, &temp).unwrap();
        assert_eq!(security(&pending), before);
        temp.write_all(b"new").unwrap();
        temp.sync_all().unwrap();
        std::fs::rename(&pending, &path).unwrap();
        assert_eq!(security(&path), before);
        assert_eq!(std::fs::read(&path).unwrap(), b"new");
    }

    #[test]
    fn persist_native_named_streams_and_links_rejected() {
        let dir = Directory::new("streams");
        let path = dir.0.join("source");
        std::fs::write(&path, b"original").unwrap();
        for bytes in [b"".as_slice(), b"secret"] {
            let stream = dir.0.join("source:hidden");
            std::fs::write(&stream, bytes).unwrap();
            let file = File::open(&path).unwrap();
            let error = check_persist_source(&file).unwrap_err();
            assert!(error.to_string().contains("named streams"), "{error}");
            assert_eq!(std::fs::read(&stream).unwrap(), bytes);
            std::fs::remove_file(&stream).unwrap();
        }
        let second = dir.0.join("link");
        std::fs::hard_link(&path, &second).unwrap();
        let error = check_persist_source(&File::open(&path).unwrap()).unwrap_err();
        assert!(error.to_string().contains("hard links"));
        assert_eq!(std::fs::read(&second).unwrap(), b"original");
    }

    #[test]
    fn persist_native_readonly_allows_recovery_not_update() {
        let dir = Directory::new("readonly");
        let path = dir.0.join("source");
        std::fs::write(&path, b"original").unwrap();
        let source = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        let original_permissions = source.metadata().unwrap().permissions();
        let mut readonly = original_permissions.clone();
        readonly.set_readonly(true);
        source.set_permissions(readonly).unwrap();
        let pending = dir.0.join("pending");
        let temp = create_persist_temporary(&pending).unwrap();
        check_persist_source(&source).unwrap();
        let result = prepare_persist_metadata(&source, &temp);
        assert!(source.metadata().unwrap().permissions().readonly());
        source.set_permissions(original_permissions).unwrap();
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(std::fs::read(&path).unwrap(), b"original");
        discard_persist_temporary(&temp, &pending).unwrap();
    }

    #[test]
    fn persist_native_publication_never_overwrites_and_syncs_directory() {
        let dir = Directory::new("publication");
        let pending = dir.0.join("pending");
        let backup = dir.0.join("backup");
        let mut temp = create_persist_temporary(&pending).unwrap();
        temp.write_all(b"complete").unwrap();
        temp.sync_all().unwrap();
        std::fs::write(&backup, b"foreign").unwrap();
        assert!(publish_persist_recovery(&pending, &backup).is_err());
        assert_eq!(std::fs::read(&backup).unwrap(), b"foreign");
        assert_eq!(std::fs::read(&pending).unwrap(), b"complete");
        std::fs::remove_file(&backup).unwrap();
        publish_persist_recovery(&pending, &backup).unwrap();
        super::super::super::sync_directory(&dir.0).unwrap();
        assert!(!pending.exists());
        assert_eq!(std::fs::read(&backup).unwrap(), b"complete");
        assert!(super::super::super::sync_directory(&dir.0.join("missing")).is_err());
    }

    #[test]
    fn persist_native_old_config_handle_cannot_set_dacl() {
        let dir = Directory::new("access");
        let path = dir.0.join("old");
        let old = super::super::create_config_temporary(&path, true).unwrap();
        let descriptor = Security::read(&old).unwrap();
        let error = unsafe {
            SetSecurityInfo(
                old.as_raw_handle(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                null_mut(),
                null_mut(),
                descriptor.dacl,
                null_mut(),
            )
        };
        assert_eq!(error, windows_sys::Win32::Foundation::ERROR_ACCESS_DENIED);
        let path = dir.0.join("new");
        let new = create_persist_temporary(&path).unwrap();
        assert_eq!(
            unsafe {
                SetSecurityInfo(
                    new.as_raw_handle(),
                    SE_FILE_OBJECT,
                    DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                    null_mut(),
                    null_mut(),
                    descriptor.dacl,
                    null_mut(),
                )
            },
            0
        );
        discard_persist_temporary(&new, &path).unwrap();
    }

    #[test]
    fn persist_native_attribute_and_inherited_compression_rejection() {
        use std::os::windows::fs::MetadataExt;
        use windows_sys::Win32::Storage::FileSystem::{
            SetFileAttributesW, FILE_ATTRIBUTE_COMPRESSED, FILE_ATTRIBUTE_OFFLINE,
        };
        let dir = Directory::new("attributes");
        let source = dir.0.join("source");
        std::fs::write(&source, b"original").unwrap();
        let wide = super::super::extended_length_path(&source).unwrap();
        assert_ne!(
            unsafe { SetFileAttributesW(wide.as_ptr(), FILE_ATTRIBUTE_OFFLINE) },
            0
        );
        let error = check_persist_source(&File::open(&source).unwrap()).unwrap_err();
        assert!(error
            .to_string()
            .contains("unsupported additional attributes"));
        let compressed = dir.0.join("compressed");
        std::fs::create_dir(&compressed).unwrap();
        let output = std::process::Command::new("compact.exe")
            .args(["/C", "/I", "/Q", "/A"])
            .arg(&compressed)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        assert_ne!(
            std::fs::metadata(&compressed).unwrap().file_attributes() & FILE_ATTRIBUTE_COMPRESSED,
            0
        );
        let temp = compressed.join("pending");
        assert_eq!(
            create_persist_temporary(&temp).unwrap_err().kind(),
            io::ErrorKind::Unsupported
        );
        assert!(!temp.exists());
        assert_eq!(std::fs::read(&source).unwrap(), b"original");
    }

    #[test]
    fn persist_native_stream_query_grows_and_fails_closed() {
        let dir = Directory::new("stream-query");
        let source = dir.0.join("source");
        std::fs::write(&source, b"original").unwrap();
        let unsupported = File::open("NUL").unwrap();
        assert!(check_streams(&unsupported).is_err());
        for index in 0..80 {
            std::fs::write(
                dir.0.join(format!("source:{index:03}-{}", "x".repeat(80))),
                b"",
            )
            .unwrap();
        }
        let error = check_persist_source(&File::open(&source).unwrap()).unwrap_err();
        assert!(error.to_string().contains("named streams"), "{error}");
    }

    #[test]
    fn persist_stream_information_rejects_malformed_offsets_and_lengths() {
        let mut bytes = vec![0; 40];
        bytes[4..8].copy_from_slice(&14u32.to_le_bytes());
        bytes[24..38].copy_from_slice(b":\0:\0$\0D\0A\0T\0A\0");
        validate_streams(&bytes).unwrap();
        for next in [1u32, 24, 39, 48, u32::MAX] {
            let mut malformed = bytes.clone();
            malformed[..4].copy_from_slice(&next.to_le_bytes());
            assert!(validate_streams(&malformed).is_err());
        }
        for length in [0u32, 1, 15, 18, u32::MAX] {
            let mut malformed = bytes.clone();
            malformed[4..8].copy_from_slice(&length.to_le_bytes());
            assert!(validate_streams(&malformed).is_err());
        }
        for length in 0..38 {
            assert!(validate_streams(&bytes[..length]).is_err());
        }
    }

    #[test]
    fn persist_native_sharing_conflict_preserves_source_and_cleans_temp() {
        use std::os::windows::fs::OpenOptionsExt;
        let dir = Directory::new("sharing");
        let source_path = dir.0.join("source");
        std::fs::write(&source_path, b"original").unwrap();
        let source = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .open(&source_path)
            .unwrap();
        let pending = dir.0.join("pending");
        let mut temp = create_persist_temporary(&pending).unwrap();
        prepare_persist_metadata(&source, &temp).unwrap_or_else(|error| {
            panic!(
                "{error}; original={:?}; current={:?}",
                Security::read(&source).unwrap().semantic().unwrap(),
                Security::read(&temp).unwrap().semantic().unwrap()
            )
        });
        temp.write_all(b"replacement").unwrap();
        temp.sync_all().unwrap();
        assert!(std::fs::rename(&pending, &source_path).is_err());
        assert_eq!(std::fs::read(&source_path).unwrap(), b"original");
        discard_persist_temporary(&temp, &pending).unwrap();
        drop(temp);
        assert!(!pending.exists());
    }

    fn with_sacl(text: &str, inspect: impl FnOnce(*mut ACL)) {
        use interprocess::os::windows::security_descriptor::{
            AsSecurityDescriptorExt as _, SecurityDescriptor,
        };
        use windows_sys::Win32::Security::GetSecurityDescriptorSacl;
        let sddl = widestring::U16CString::from_str(text).unwrap();
        let descriptor = SecurityDescriptor::deserialize(&sddl)
            .unwrap_or_else(|error| panic!("{text}: {error}"));
        let mut attributes = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: null_mut(),
            bInheritHandle: 0,
        };
        descriptor.write_to_security_attributes(&mut attributes);
        let mut present = 0;
        let mut defaulted = 0;
        let mut acl = null_mut();
        assert_ne!(
            unsafe {
                GetSecurityDescriptorSacl(
                    attributes.lpSecurityDescriptor,
                    &mut present,
                    &mut acl,
                    &mut defaulted,
                )
            },
            0
        );
        assert_ne!(present, 0);
        assert!(!acl.is_null());
        inspect(acl);
    }

    fn unsupported_authorization_native(case: &str, information: u32, sddl: &str) {
        use windows_sys::Win32::Security::GetSecurityDescriptorSacl;
        let dir = Directory::new(case);
        let path = dir.0.join("source");
        let mut source = create_persist_temporary(&path).unwrap();
        source.write_all(b"original").unwrap();
        let mut status = 0;
        with_sacl(sddl, |acl| {
            status = unsafe {
                SetSecurityInfo(
                    source.as_raw_handle(),
                    SE_FILE_OBJECT,
                    information,
                    null_mut(),
                    null_mut(),
                    null_mut(),
                    acl,
                )
            };
        });
        if status != 0 {
            eprintln!(
                "PENDING: native {case} fixture unavailable without privileges: {}",
                io::Error::from_raw_os_error(status as i32)
            );
            return;
        }
        let mut before = super::super::config_security_descriptor(&path, information).unwrap();
        let mut present = 0;
        let mut defaulted = 0;
        let mut acl = null_mut();
        assert_ne!(
            unsafe {
                GetSecurityDescriptorSacl(
                    before.as_mut_ptr().cast(),
                    &mut present,
                    &mut acl,
                    &mut defaulted,
                )
            },
            0
        );
        assert!(
            !acl.is_null() && unsafe { (*acl).AceCount } != 0,
            "fixture must be stored, not silently dropped"
        );
        let error = check_persist_source(&source).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Unsupported);
        assert!(error.to_string().contains("authorization metadata"));
        let pending = dir.0.join("pending");
        let temp = create_persist_temporary(&pending).unwrap();
        assert_eq!(
            prepare_persist_metadata(&source, &temp).unwrap_err().kind(),
            io::ErrorKind::Unsupported
        );
        assert_eq!(temp.metadata().unwrap().len(), 0);
        assert_eq!(std::fs::read(&path).unwrap(), b"original");
        assert_eq!(
            super::super::config_security_descriptor(&path, information).unwrap(),
            before
        );
        discard_persist_temporary(&temp, &pending).unwrap();
        drop(temp);
        assert!(!pending.exists());
        eprintln!("PASS: native {case} installed, queried and rejected before copying");
    }

    #[test]
    fn persist_review_resource_attribute_native_rejected() {
        unsupported_authorization_native(
            "resource-attribute",
            ATTRIBUTE_SECURITY_INFORMATION,
            r#"S:(RA;;;;;WD;("Department",TS,0,"Finance"))"#,
        );
    }

    #[test]
    fn persist_review_cap_scope_native_rejected() {
        unsupported_authorization_native(
            "CAP-scope",
            SCOPE_SECURITY_INFORMATION,
            "S:(SP;;;;;S-1-17-1)",
        );
    }

    #[test]
    fn persist_review_authorization_descriptors_rejected() {
        use windows_sys::Win32::System::SystemServices::{
            SYSTEM_ACCESS_FILTER_ACE_TYPE, SYSTEM_PROCESS_TRUST_LABEL_ACE_TYPE,
            SYSTEM_RESOURCE_ATTRIBUTE_ACE_TYPE, SYSTEM_SCOPED_POLICY_ID_ACE_TYPE,
        };
        for (sddl, expected) in [
            (
                r#"S:(RA;;;;;WD;("Department",TS,0,"Finance"))"#,
                SYSTEM_RESOURCE_ATTRIBUTE_ACE_TYPE,
            ),
            ("S:(SP;;;;;S-1-17-1)", SYSTEM_SCOPED_POLICY_ID_ACE_TYPE),
            (
                "S:(TL;;FR;;;S-1-19-512-4096)",
                SYSTEM_PROCESS_TRUST_LABEL_ACE_TYPE,
            ),
            (
                r#"S:(FL;;FR;;;WD;(@User.Department == "Finance"))"#,
                SYSTEM_ACCESS_FILTER_ACE_TYPE,
            ),
        ] {
            with_sacl(sddl, |acl| {
                let mut ace = null_mut();
                assert_ne!(unsafe { GetAce(acl, 0, &mut ace) }, 0);
                assert_eq!(
                    u32::from(unsafe { (*ace.cast::<ACE_HEADER>()).AceType }),
                    expected
                );
                assert_eq!(
                    reject_unsupported_authorization(acl).unwrap_err().kind(),
                    io::ErrorKind::Unsupported
                );
            });
        }
        with_sacl("S:(ML;;NW;;;LW)", |acl| {
            reject_unsupported_authorization(acl).unwrap()
        });
        reject_unsupported_authorization(null_mut()).unwrap();
    }

    #[test]
    fn persist_review_authorization_queries_are_read_control_only() {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Security::QuerySecurityAccessMask;
        for information in [
            ATTRIBUTE_SECURITY_INFORMATION,
            SCOPE_SECURITY_INFORMATION,
            PROCESS_TRUST_LABEL_SECURITY_INFORMATION as u32,
            ACCESS_FILTER_SECURITY_INFORMATION as u32,
            PERSIST_SECURITY_INFORMATION,
        ] {
            let mut access = 0;
            unsafe {
                QuerySecurityAccessMask(information, &mut access);
            }
            assert_eq!(access, READ_CONTROL);
        }
        let dir = Directory::new("authorization-query");
        let path = dir.0.join("source");
        let source = create_persist_temporary(&path).unwrap();
        Security::read(&source).unwrap();
        let reader = std::fs::OpenOptions::new()
            .access_mode(READ_CONTROL)
            .open(&path)
            .unwrap();
        Security::read(&reader).unwrap();
        let denied = std::fs::OpenOptions::new()
            .access_mode(FILE_READ_ATTRIBUTES)
            .open(&path)
            .unwrap();
        assert_eq!(
            Security::read(&denied).err().unwrap().kind(),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(
            check_persist_source(&denied).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
    }

    #[test]
    fn persist_review_basic_attributes_roundtrip() {
        let dir = Directory::new("basic-attributes");
        let wide = super::super::extended_length_path(&dir.0).unwrap();
        assert_ne!(
            unsafe {
                windows_sys::Win32::Storage::FileSystem::SetFileAttributesW(
                    wide.as_ptr(),
                    FILE_ATTRIBUTE_NORMAL,
                )
            },
            0,
            "{}",
            io::Error::last_os_error()
        );
        for private in [false, true] {
            for combination in 0..8u32 {
                let flags = [
                    FILE_ATTRIBUTE_HIDDEN,
                    FILE_ATTRIBUTE_SYSTEM,
                    FILE_ATTRIBUTE_NOT_CONTENT_INDEXED,
                ]
                .iter()
                .enumerate()
                .filter(|(index, _)| combination & (1 << index) != 0)
                .fold(0, |flags, (_, flag)| flags | flag);
                let path = dir.0.join(format!("source-{private}-{combination}"));
                let mut file = super::super::create_config_temporary(&path, private).unwrap();
                file.write_all(b"original").unwrap();
                drop(file);
                let source = std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(&path)
                    .unwrap();
                let information = FILE_BASIC_INFO {
                    CreationTime: 125_000_000_000_000_000,
                    LastAccessTime: 125_000_000_000_000_000,
                    LastWriteTime: 125_000_000_000_000_000,
                    FileAttributes: if flags == 0 {
                        FILE_ATTRIBUTE_NORMAL
                    } else {
                        flags
                    },
                    ..Default::default()
                };
                assert_ne!(
                    unsafe {
                        SetFileInformationByHandle(
                            source.as_raw_handle(),
                            FileBasicInfo,
                            std::ptr::from_ref(&information).cast(),
                            size_of::<FILE_BASIC_INFO>() as u32,
                        )
                    },
                    0
                );
                let before_security = security(&path);
                let pending = path.with_extension("pending");
                let mut temp = create_persist_temporary(&pending).unwrap();
                let new_security = security(&pending);
                assert!(String::from_utf16_lossy(&new_security).contains("D:P("));
                assert_eq!(
                    basic_information(&temp).unwrap().FileAttributes & PERSIST_BASIC_ATTRIBUTES,
                    0
                );
                let fresh = basic_information(&temp).unwrap();
                prepare_persist_metadata(&source, &temp).unwrap();
                let prepared = basic_information(&temp).unwrap();
                assert_eq!(prepared.FileAttributes & PERSIST_BASIC_ATTRIBUTES, flags);
                assert_eq!(prepared.CreationTime, fresh.CreationTime);
                assert_eq!(prepared.LastWriteTime, fresh.LastWriteTime);
                assert_ne!(prepared.CreationTime, information.CreationTime);
                assert_eq!(security(&pending), before_security);
                temp.write_all(b"replacement").unwrap();
                temp.sync_all().unwrap();
                std::fs::rename(&pending, &path).unwrap();
                assert_eq!(
                    basic_information(&File::open(&path).unwrap())
                        .unwrap()
                        .FileAttributes
                        & PERSIST_BASIC_ATTRIBUTES,
                    flags
                );
                assert_eq!(security(&path), before_security);
                assert_eq!(std::fs::read(&path).unwrap(), b"replacement");
            }
        }
    }

    #[test]
    fn persist_native_efs_rejected_before_copy() {
        use std::os::windows::fs::MetadataExt;
        use windows_sys::Win32::Storage::FileSystem::{EncryptFileW, FILE_ATTRIBUTE_ENCRYPTED};
        let dir = Directory::new("efs");
        let path = dir.0.join("source");
        std::fs::write(&path, b"encrypted original").unwrap();
        let wide = super::super::extended_length_path(&path).unwrap();
        if unsafe { EncryptFileW(wide.as_ptr()) } == 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error()
                == Some(windows_sys::Win32::Foundation::ERROR_NOT_SUPPORTED as i32)
                && std::env::var_os("CI").is_none()
            {
                eprintln!("PENDING: EFS source and inherited EFS fixtures unavailable: {error}");
                return;
            }
            panic!("EFS fixture unavailable: {error}");
        }
        assert_ne!(
            std::fs::metadata(&path).unwrap().file_attributes() & FILE_ATTRIBUTE_ENCRYPTED,
            0
        );
        let before = security(&path);
        let error = check_persist_source(&File::open(&path).unwrap()).unwrap_err();
        assert!(error.to_string().contains("encrypted persist"));
        assert_eq!(security(&path), before);
        assert_eq!(std::fs::read(&path).unwrap(), b"encrypted original");
        let wide = super::super::extended_length_path(&dir.0).unwrap();
        assert_ne!(unsafe { EncryptFileW(wide.as_ptr()) }, 0);
        let pending = dir.0.join("pending");
        assert_eq!(
            create_persist_temporary(&pending).unwrap_err().kind(),
            io::ErrorKind::Unsupported
        );
        assert!(!pending.exists());
    }
}
