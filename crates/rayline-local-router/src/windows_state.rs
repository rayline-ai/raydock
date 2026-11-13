//! Windows support for hybrid runtime state, shared by the CLI and the local
//! process.
//!
//! Unix keeps hybrid state private with 0700/0600 modes. Windows has no mode
//! bits, so the same files get a protected DACL that grants only the current
//! user. A named event gives the CLI a graceful way to stop a detached local
//! process that has no console it can signal and no gateway serving yet.

use std::ffi::OsStr;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, ERROR_FILE_NOT_FOUND, GetLastError, HANDLE, LocalFree,
    WAIT_OBJECT_0,
};
use windows_sys::Win32::Security::Authorization::{
    EXPLICIT_ACCESS_W, GetNamedSecurityInfoW, NO_MULTIPLE_TRUSTEE, SE_FILE_OBJECT, SET_ACCESS,
    SetEntriesInAclW, SetNamedSecurityInfoW, TRUSTEE_IS_SID, TRUSTEE_IS_USER, TRUSTEE_W,
};
use windows_sys::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, ACL_SIZE_INFORMATION, AclSizeInformation,
    DACL_SECURITY_INFORMATION, EqualSid, GetAce, GetAclInformation, GetSecurityDescriptorControl,
    GetTokenInformation, INHERITED_ACE, NO_INHERITANCE, PROTECTED_DACL_SECURITY_INFORMATION,
    PSECURITY_DESCRIPTOR, PSID, SE_DACL_PROTECTED, SUB_CONTAINERS_AND_OBJECTS_INHERIT, TOKEN_QUERY,
    TOKEN_USER, TokenUser,
};
use windows_sys::Win32::Storage::FileSystem::FILE_ALL_ACCESS;
use windows_sys::Win32::System::Threading::{
    CreateEventW, EVENT_MODIFY_STATE, GetCurrentProcess, INFINITE, OpenEventW, OpenProcessToken,
    SetEvent, WaitForSingleObject,
};

const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;

fn wide(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain(std::iter::once(0)).collect()
}

fn win32(code: u32) -> io::Error {
    io::Error::from_raw_os_error(code as i32)
}

struct Handle(HANDLE);
impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: the handle was returned open by the API that created it.
        unsafe { CloseHandle(self.0) };
    }
}

struct Local<T>(*mut T);
impl<T> Drop for Local<T> {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: the pointer was allocated by the API with LocalAlloc.
            unsafe { LocalFree(self.0.cast()) };
        }
    }
}

/// The current user's SID, as the TOKEN_USER buffer that owns it.
struct CurrentUser(Vec<u64>);
impl CurrentUser {
    fn get() -> io::Result<Self> {
        let mut token: HANDLE = null_mut();
        // SAFETY: GetCurrentProcess is a pseudo-handle; token is an out pointer.
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let token = Handle(token);
        let mut needed = 0u32;
        // SAFETY: a null buffer asks only for the size.
        unsafe { GetTokenInformation(token.0, TokenUser, null_mut(), 0, &mut needed) };
        // u64 storage keeps TOKEN_USER suitably aligned.
        let mut buffer = vec![0u64; (needed as usize).div_ceil(8)];
        // SAFETY: the buffer holds `needed` bytes.
        if unsafe {
            GetTokenInformation(
                token.0,
                TokenUser,
                buffer.as_mut_ptr().cast(),
                needed,
                &mut needed,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(buffer))
    }
    fn sid(&self) -> PSID {
        // SAFETY: the buffer was filled with a TOKEN_USER above.
        unsafe { (*self.0.as_ptr().cast::<TOKEN_USER>()).User.Sid }
    }
}

/// Replace the DACL of `path` with a protected one granting full access to the
/// current user only, so no inherited or wider entry remains. With `inherit`
/// (a directory), files and folders created inside inherit the same entry.
pub fn restrict_to_owner(path: &Path, inherit: bool) -> io::Result<()> {
    let user = CurrentUser::get()?;
    let access = EXPLICIT_ACCESS_W {
        grfAccessPermissions: FILE_ALL_ACCESS,
        grfAccessMode: SET_ACCESS,
        grfInheritance: if inherit {
            SUB_CONTAINERS_AND_OBJECTS_INHERIT
        } else {
            NO_INHERITANCE
        },
        Trustee: TRUSTEE_W {
            pMultipleTrustee: null_mut(),
            MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: TRUSTEE_IS_USER,
            ptstrName: user.sid().cast(),
        },
    };
    let mut acl: *mut ACL = null_mut();
    // SAFETY: one entry, no old ACL; the new ACL is freed by `Local`.
    let error = unsafe { SetEntriesInAclW(1, &access, null(), &mut acl) };
    let acl = Local(acl);
    if error != 0 {
        return Err(win32(error));
    }
    let name = wide(path.as_os_str());
    // SAFETY: a NUL-terminated path and a valid ACL; owner and group unchanged.
    let error = unsafe {
        SetNamedSecurityInfoW(
            name.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            acl.0,
            null(),
        )
    };
    if error != 0 {
        return Err(win32(error));
    }
    Ok(())
}

/// Whether `path` has a protected DACL whose every entry allows only the
/// current user: nothing inherited, no other trustee, no deny entries.
pub fn is_owner_only(path: &Path) -> io::Result<bool> {
    let user = CurrentUser::get()?;
    let name = wide(path.as_os_str());
    let mut dacl: *mut ACL = null_mut();
    let mut descriptor: PSECURITY_DESCRIPTOR = null_mut();
    // SAFETY: out pointers; the descriptor owns the DACL and is freed by `Local`.
    let error = unsafe {
        GetNamedSecurityInfoW(
            name.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            &mut dacl,
            null_mut(),
            &mut descriptor,
        )
    };
    let descriptor = Local(descriptor);
    if error != 0 {
        return Err(win32(error));
    }
    if dacl.is_null() {
        // A null DACL grants everyone everything.
        return Ok(false);
    }
    let (mut control, mut revision) = (0u16, 0u32);
    // SAFETY: a valid descriptor returned above.
    if unsafe { GetSecurityDescriptorControl(descriptor.0, &mut control, &mut revision) } == 0 {
        return Err(io::Error::last_os_error());
    }
    if control & SE_DACL_PROTECTED == 0 {
        return Ok(false);
    }
    let mut info = ACL_SIZE_INFORMATION::default();
    // SAFETY: the buffer is an ACL_SIZE_INFORMATION.
    if unsafe {
        GetAclInformation(
            dacl,
            (&mut info as *mut ACL_SIZE_INFORMATION).cast(),
            size_of::<ACL_SIZE_INFORMATION>() as u32,
            AclSizeInformation,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    if info.AceCount == 0 {
        return Ok(false);
    }
    for index in 0..info.AceCount {
        let mut ace: *mut core::ffi::c_void = null_mut();
        // SAFETY: index is below the ACE count.
        if unsafe { GetAce(dacl, index, &mut ace) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let ace = std::ptr::NonNull::new(ace)
            .ok_or_else(|| io::Error::other("GetAce returned a null ACE"))?;
        // SAFETY: GetAce succeeded, so `ace` points at an ACE inside the DACL,
        // and every ACE starts with an ACE_HEADER.
        let header = unsafe { ace.cast::<ACE_HEADER>().as_ptr().read_unaligned() };
        if header.AceType != ACCESS_ALLOWED_ACE_TYPE
            || u32::from(header.AceFlags) & INHERITED_ACE != 0
        {
            return Ok(false);
        }
        // SAFETY: an ACCESS_ALLOWED_ACE stores its SID from SidStart.
        let sid = unsafe {
            std::ptr::addr_of!((*ace.cast::<ACCESS_ALLOWED_ACE>().as_ptr()).SidStart) as PSID
        };
        // SAFETY: both SIDs are valid for the duration of the call.
        if unsafe { EqualSid(sid, user.sid()) } == 0 {
            return Ok(false);
        }
    }
    Ok(true)
}

fn stop_event_name(pid: u32) -> Vec<u16> {
    wide(OsStr::new(&format!("Local\\rayline-hybrid-stop-{pid}")))
}

/// The event a running local process waits on; `request_stop` with its pid sets it.
pub struct StopEvent(Handle);

// SAFETY: an event handle may be waited on from any thread.
unsafe impl Send for StopEvent {}

impl StopEvent {
    /// Create this process's stop event. Refuses an event that already
    /// exists: another process holding the name could otherwise stop us.
    pub fn create() -> io::Result<Self> {
        let name = stop_event_name(std::process::id());
        // SAFETY: default security (the creating user), manual reset, unset.
        let handle = unsafe { CreateEventW(null(), 1, 0, name.as_ptr()) };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        let handle = Handle(handle);
        // SAFETY: reads the calling thread's last error from CreateEventW.
        if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
            return Err(io::Error::other("hybrid stop event already exists"));
        }
        Ok(Self(handle))
    }

    /// Block until the event is set.
    pub fn wait(&self) -> io::Result<()> {
        // SAFETY: a valid event handle.
        match unsafe { WaitForSingleObject(self.0.0, INFINITE) } {
            WAIT_OBJECT_0 => Ok(()),
            _ => Err(io::Error::last_os_error()),
        }
    }
}

/// Ask the local process with `pid` to stop gracefully. `NotFound` means no such
/// event: the process is gone, or it is not a hybrid supervisor.
pub fn request_stop(pid: u32) -> io::Result<()> {
    let name = stop_event_name(pid);
    // SAFETY: a NUL-terminated name; the handle is closed by `Handle`.
    let handle = unsafe { OpenEventW(EVENT_MODIFY_STATE, 0, name.as_ptr()) };
    if handle.is_null() {
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(ERROR_FILE_NOT_FOUND as i32) {
            return Err(io::Error::new(io::ErrorKind::NotFound, error));
        }
        return Err(error);
    }
    let handle = Handle(handle);
    // SAFETY: opened with EVENT_MODIFY_STATE.
    if unsafe { SetEvent(handle.0) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restricted_directory_and_new_children_are_owner_only() {
        let dir = std::env::temp_dir().join(format!(
            "rayline-windows-state-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&dir).unwrap();
        // The temp folder grants more than the user; a fresh child inherits it.
        assert!(!is_owner_only(&dir).unwrap());
        restrict_to_owner(&dir, true).unwrap();
        assert!(is_owner_only(&dir).unwrap());
        let file = dir.join("token");
        std::fs::write(&file, b"secret").unwrap();
        // Inherited from the folder: owner-only content, but not protected yet.
        assert!(!is_owner_only(&file).unwrap());
        restrict_to_owner(&file, false).unwrap();
        assert!(is_owner_only(&file).unwrap());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn stop_event_is_unique_and_reaches_the_waiter() {
        let event = StopEvent::create().unwrap();
        assert!(StopEvent::create().is_err(), "a second owner is refused");
        let waiter = std::thread::spawn(move || event.wait());
        request_stop(std::process::id()).unwrap();
        waiter.join().unwrap().unwrap();
        assert_eq!(
            request_stop(u32::MAX - 1).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
    }
}
