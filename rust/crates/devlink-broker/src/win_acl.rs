//! Native Windows security descriptor access for the DevLink ACL policy.
//!
//! Reading and writing ACLs in-process replaces one PowerShell or icacls child
//! process per check (S15P31D108-164). The policy itself lives in `credentials`.

use anyhow::{Context, Result, bail, ensure};
use std::{ffi::c_void, os::windows::ffi::OsStrExt, path::Path, ptr, sync::OnceLock};
use windows_sys::Win32::{
    Foundation::{CloseHandle, ERROR_SUCCESS, HANDLE, LocalFree},
    Security::Authorization::{GetNamedSecurityInfoW, SE_FILE_OBJECT, SetNamedSecurityInfoW},
    Security::{
        ACE_HEADER, ACL, ACL_REVISION, ACL_SIZE_INFORMATION, AclSizeInformation,
        AddAccessAllowedAceEx, CONTAINER_INHERIT_ACE, DACL_SECURITY_INFORMATION, GetAce,
        GetAclInformation, GetLengthSid, GetSecurityDescriptorControl, GetSecurityDescriptorDacl,
        GetSecurityDescriptorOwner, GetTokenInformation, InitializeAcl, IsValidSid,
        OBJECT_INHERIT_ACE, OWNER_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION,
        PSECURITY_DESCRIPTOR, PSID, SE_DACL_PROTECTED, TOKEN_QUERY, TOKEN_USER, TokenUser,
        UNPROTECTED_DACL_SECURITY_INFORMATION,
    },
    System::Threading::{GetCurrentProcess, OpenProcessToken},
};

const ACCESS_ALLOWED: u8 = 0;
const ACCESS_ALLOWED_OBJECT: u8 = 5;
const ACCESS_ALLOWED_CALLBACK: u8 = 9;
const ACCESS_ALLOWED_CALLBACK_OBJECT: u8 = 11;
/// Same mask icacls writes for `(F)`.
const FILE_ALL_ACCESS: u32 = 0x001F_01FF;

/// One allow ACE. Deny ACEs never widen access, so the policy does not read them.
#[derive(Debug, Clone)]
pub(crate) struct Allow {
    pub(crate) flags: u8,
    pub(crate) mask: u32,
    /// `None` when the trustee cannot be read (object ACEs); treated as untrusted.
    pub(crate) sid: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct Descriptor {
    pub(crate) owner: Option<String>,
    /// `None` is a null DACL, which grants everyone full access.
    pub(crate) allows: Option<Vec<Allow>>,
}

struct LocalMemory(*mut c_void);
impl Drop for LocalMemory {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { LocalFree(self.0) };
        }
    }
}

fn wide(path: &Path) -> Result<Vec<u16>> {
    let mut value: Vec<u16> = path.as_os_str().encode_wide().collect();
    ensure!(!value.contains(&0), "Path contains a NUL character");
    value.push(0);
    Ok(value)
}

/// Formats a SID like `ConvertSidToStringSidW` without another allocation round trip.
unsafe fn sid_string(sid: PSID) -> Option<String> {
    if sid.is_null() || unsafe { IsValidSid(sid) } == 0 {
        return None;
    }
    let length = unsafe { GetLengthSid(sid) } as usize;
    let bytes = unsafe { std::slice::from_raw_parts(sid as *const u8, length) };
    let count = *bytes.get(1)? as usize;
    if bytes.len() != 8 + 4 * count {
        return None;
    }
    let authority = bytes[2..8]
        .iter()
        .fold(0u64, |value, byte| (value << 8) | u64::from(*byte));
    let mut text = if authority < (1 << 32) {
        format!("S-{}-{authority}", bytes[0])
    } else {
        format!("S-{}-0x{authority:012X}", bytes[0])
    };
    for chunk in bytes[8..].as_chunks::<4>().0 {
        text.push_str(&format!("-{}", u32::from_le_bytes(*chunk)));
    }
    Some(text)
}

unsafe fn read_allows(dacl: *const ACL) -> Result<Vec<Allow>> {
    let mut info = ACL_SIZE_INFORMATION::default();
    let size = std::mem::size_of::<ACL_SIZE_INFORMATION>() as u32;
    let read = unsafe {
        GetAclInformation(
            dacl,
            (&mut info as *mut ACL_SIZE_INFORMATION).cast(),
            size,
            AclSizeInformation,
        )
    };
    ensure!(read != 0, "Cannot read Windows ACL");
    let mut allows = Vec::new();
    for index in 0..info.AceCount {
        let mut ace: *mut c_void = ptr::null_mut();
        ensure!(
            unsafe { GetAce(dacl, index, &mut ace) } != 0 && !ace.is_null(),
            "Cannot read Windows ACE"
        );
        let header = unsafe { &*(ace as *const ACE_HEADER) };
        let size = usize::from(header.AceSize);
        ensure!(size >= 8, "Malformed Windows ACE");
        let mask = unsafe { (ace as *const u8).add(4).cast::<u32>().read_unaligned() };
        let sid = match header.AceType {
            ACCESS_ALLOWED | ACCESS_ALLOWED_CALLBACK => {
                ensure!(size > 8, "Malformed Windows ACE");
                let sid = unsafe { (ace as *mut u8).add(8) } as PSID;
                let text = unsafe { sid_string(sid) }.context("Malformed Windows ACE SID")?;
                ensure!(
                    8 + unsafe { GetLengthSid(sid) } as usize <= size,
                    "Malformed Windows ACE SID"
                );
                Some(text)
            }
            // Object ACEs carry optional GUIDs before the SID and do not belong on files;
            // keep them as untrusted allows so the policy fails closed.
            ACCESS_ALLOWED_OBJECT | ACCESS_ALLOWED_CALLBACK_OBJECT => None,
            _ => continue,
        };
        allows.push(Allow {
            flags: header.AceFlags,
            mask,
            sid,
        });
    }
    Ok(allows)
}

/// Reads the owner and allow ACEs from a self-relative or absolute descriptor.
unsafe fn read_descriptor(descriptor: PSECURITY_DESCRIPTOR) -> Result<Descriptor> {
    let mut owner: PSID = ptr::null_mut();
    let mut defaulted = 0;
    ensure!(
        unsafe { GetSecurityDescriptorOwner(descriptor, &mut owner, &mut defaulted) } != 0,
        "Cannot read Windows owner"
    );
    let mut present = 0;
    let mut dacl: *mut ACL = ptr::null_mut();
    ensure!(
        unsafe { GetSecurityDescriptorDacl(descriptor, &mut present, &mut dacl, &mut defaulted) }
            != 0,
        "Cannot read Windows DACL"
    );
    let allows = if present == 0 || dacl.is_null() {
        None
    } else {
        Some(unsafe { read_allows(dacl) }?)
    };
    Ok(Descriptor {
        owner: unsafe { sid_string(owner) },
        allows,
    })
}

fn named_descriptor(path: &Path, information: u32) -> Result<LocalMemory> {
    let name = wide(path)?;
    let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
    let status = unsafe {
        GetNamedSecurityInfoW(
            name.as_ptr(),
            SE_FILE_OBJECT,
            information,
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            &mut descriptor,
        )
    };
    let descriptor = LocalMemory(descriptor);
    ensure!(
        status == ERROR_SUCCESS && !descriptor.0.is_null(),
        "Cannot read Windows security descriptor"
    );
    Ok(descriptor)
}

pub(crate) fn read(path: &Path) -> Result<Descriptor> {
    let descriptor =
        named_descriptor(path, OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION)?;
    unsafe { read_descriptor(descriptor.0) }
}

/// Parses SDDL with the same Win32 parser .NET uses, for policy regression tests.
#[cfg(test)]
pub(crate) fn parse_sddl(sddl: &str) -> Result<Descriptor> {
    use windows_sys::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    let text: Vec<u16> = sddl.encode_utf16().chain(Some(0)).collect();
    let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
    let converted = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            text.as_ptr(),
            SDDL_REVISION_1,
            &mut descriptor,
            ptr::null_mut(),
        )
    };
    let descriptor = LocalMemory(descriptor);
    ensure!(converted != 0, "Invalid SDDL");
    unsafe { read_descriptor(descriptor.0) }
}

pub(crate) struct UserSid {
    bytes: Vec<u8>,
    pub(crate) text: String,
}

struct Token(HANDLE);
impl Drop for Token {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

fn query_user() -> Result<UserSid> {
    let mut handle: HANDLE = ptr::null_mut();
    ensure!(
        unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut handle) } != 0,
        "Cannot open the current process token"
    );
    let token = Token(handle);
    let mut length = 0u32;
    unsafe { GetTokenInformation(token.0, TokenUser, ptr::null_mut(), 0, &mut length) };
    ensure!(length > 0, "Cannot size the current token user");
    // u64 storage keeps TOKEN_USER and its embedded pointer aligned.
    let mut buffer = vec![0u64; (length as usize).div_ceil(8)];
    ensure!(
        unsafe {
            GetTokenInformation(
                token.0,
                TokenUser,
                buffer.as_mut_ptr().cast(),
                length,
                &mut length,
            )
        } != 0,
        "Cannot read the current token user"
    );
    let sid = unsafe { (*(buffer.as_ptr() as *const TOKEN_USER)).User.Sid };
    let text = unsafe { sid_string(sid) }.context("Invalid current user SID")?;
    let size = unsafe { GetLengthSid(sid) } as usize;
    let bytes = unsafe { std::slice::from_raw_parts(sid as *const u8, size) }.to_vec();
    Ok(UserSid { bytes, text })
}

/// The process token user, the same identity `whoami /user` reported.
pub(crate) fn current_user() -> Result<&'static UserSid> {
    static USER: OnceLock<UserSid> = OnceLock::new();
    if let Some(user) = USER.get() {
        return Ok(user);
    }
    let user = query_user()?;
    Ok(USER.get_or_init(|| user))
}

fn set_dacl(path: &Path, information: u32, dacl: *const ACL) -> Result<()> {
    let name = wide(path)?;
    let status = unsafe {
        SetNamedSecurityInfoW(
            name.as_ptr(),
            SE_FILE_OBJECT,
            information,
            ptr::null_mut(),
            ptr::null_mut(),
            dacl,
            ptr::null(),
        )
    };
    if status != ERROR_SUCCESS {
        bail!("Cannot write Windows DACL");
    }
    Ok(())
}

/// Replaces the DACL with a protected one granting only the current user full
/// control, inheritable for directories. The owner is left unchanged.
pub(crate) fn set_private(path: &Path, directory: bool) -> Result<()> {
    let user = current_user()?;
    let mut sid = user.bytes.clone();
    // ACL header (8) + ACE header and mask (8) + SID, DWORD aligned.
    let size = (16 + sid.len()).next_multiple_of(4);
    let mut buffer = vec![0u32; size / 4];
    let acl = buffer.as_mut_ptr() as *mut ACL;
    ensure!(
        unsafe { InitializeAcl(acl, size as u32, ACL_REVISION) } != 0,
        "Cannot initialize Windows ACL"
    );
    let flags = if directory {
        OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE
    } else {
        0
    };
    ensure!(
        unsafe {
            AddAccessAllowedAceEx(
                acl,
                ACL_REVISION,
                flags,
                FILE_ALL_ACCESS,
                sid.as_mut_ptr().cast(),
            )
        } != 0,
        "Cannot build Windows ACL"
    );
    set_dacl(
        path,
        DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
        acl,
    )
}

/// Copies the DACL and its protection state, like .NET `SetAccessControl` with the
/// Access section of the source descriptor.
pub(crate) fn copy_dacl(source: &Path, target: &Path) -> Result<()> {
    let descriptor = named_descriptor(source, DACL_SECURITY_INFORMATION)?;
    let mut control = 0u16;
    let mut revision = 0u32;
    ensure!(
        unsafe { GetSecurityDescriptorControl(descriptor.0, &mut control, &mut revision) } != 0,
        "Cannot read Windows descriptor control"
    );
    let mut present = 0;
    let mut defaulted = 0;
    let mut dacl: *mut ACL = ptr::null_mut();
    ensure!(
        unsafe { GetSecurityDescriptorDacl(descriptor.0, &mut present, &mut dacl, &mut defaulted) }
            != 0,
        "Cannot read Windows DACL"
    );
    // A null DACL would grant everyone access; never copy it.
    ensure!(present != 0 && !dacl.is_null(), "Source has no DACL");
    let protection = if control & SE_DACL_PROTECTED != 0 {
        PROTECTED_DACL_SECURITY_INFORMATION
    } else {
        UNPROTECTED_DACL_SECURITY_INFORMATION
    };
    set_dacl(target, DACL_SECURITY_INFORMATION | protection, dacl)
}
