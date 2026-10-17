//! The only unsafe code in vuoxi: fanotify setup and the sandbox probe.
//!
//! `nix` 0.31's fanotify module cannot be used: it lacks the `FAN_REPORT_*`
//! init flags and its `read_events` discards the info records that carry the
//! file handles and names. So we call `fanotify_init`/`fanotify_mark` from
//! `libc` directly. Reading is plain `read(2)` on a [`File`], which is safe.

use std::ffi::{CStr, CString};
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use crate::parse::WANTED;
use crate::record::{Handle, MAX_HANDLE};

/// An fanotify group reporting directory handle, name and target handle.
pub struct Group(File);

impl Group {
    /// Needs `CAP_SYS_ADMIN`. Handles-only reporting means events never carry
    /// open file descriptors, so a hostile file cannot make us open anything.
    pub fn init() -> io::Result<Group> {
        let flags = libc::FAN_CLOEXEC | libc::FAN_CLASS_NOTIF | libc::FAN_REPORT_DFID_NAME_TARGET;
        // SAFETY: plain syscall wrapper, no pointers.
        let fd = unsafe { libc::fanotify_init(flags, libc::O_RDONLY as libc::c_uint) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: fd is a fresh descriptor we exclusively own.
        Ok(Group(File::from(unsafe { OwnedFd::from_raw_fd(fd) })))
    }

    /// Mark the whole filesystem containing `path` for [`WANTED`] events.
    pub fn mark_filesystem(&self, path: &CStr) -> io::Result<()> {
        // SAFETY: `path` is a valid NUL-terminated string for the call's duration.
        let rc = unsafe {
            libc::fanotify_mark(
                self.0.as_raw_fd(),
                libc::FAN_MARK_ADD | libc::FAN_MARK_FILESYSTEM,
                WANTED,
                libc::AT_FDCWD,
                path.as_ptr(),
            )
        };
        if rc < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    pub fn raw_fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }

    /// Blocking read of whole events into `buf`.
    pub fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        io::Read::read(&mut self.0, buf)
    }
}

/// Attempt a syscall the filter must forbid (`socket(2)`). Used only by the
/// sandbox self-test, where the process is expected to die with SIGSYS.
#[doc(hidden)]
pub fn probe_forbidden_syscall() -> i32 {
    // SAFETY: plain syscall wrapper, no pointers.
    unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0) }
}

fn cpath(path: &Path) -> io::Result<CString> {
    CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))
}

/// The kernel file handle of `path` (not following a final symlink), via
/// `name_to_handle_at(2)`, which needs no privilege.
///
/// The result is byte-for-byte what fanotify reports in its FID info records
/// for the same object: `handle_type` and the `f_handle` bytes, without the
/// length word. That is what makes handle-keyed lookups hit.
pub fn path_handle(path: &Path) -> io::Result<Handle> {
    /// `struct file_handle` with room for the largest legal handle.
    #[repr(C)]
    struct Buf {
        handle_bytes: u32,
        handle_type: i32,
        f_handle: [u8; MAX_HANDLE],
    }
    let c = cpath(path)?;
    let mut buf = Buf {
        handle_bytes: MAX_HANDLE as u32,
        handle_type: 0,
        f_handle: [0; MAX_HANDLE],
    };
    let mut mount_id: libc::c_int = 0;
    // SAFETY: `c` is NUL-terminated; `buf` is a live, writable, 4-aligned
    // `struct file_handle` with `handle_bytes` bytes of trailing space, which
    // is exactly what we declared in `handle_bytes`; `mount_id` is a valid out pointer.
    let rc = unsafe {
        libc::name_to_handle_at(
            libc::AT_FDCWD,
            c.as_ptr(),
            (&raw mut buf).cast::<libc::file_handle>(),
            &mut mount_id,
            0,
        )
    };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    let n = (buf.handle_bytes as usize).min(MAX_HANDLE);
    Ok(Handle {
        ty: buf.handle_type,
        bytes: buf.f_handle[..n].to_vec(),
    })
}

/// The filesystem id of `path` as fanotify reports it: `statfs(2)` `f_fsid`,
/// the two `int`s in native byte order (the raw `__kernel_fsid_t`).
pub fn fs_id(path: &Path) -> io::Result<[u8; 8]> {
    let c = cpath(path)?;
    let mut st = std::mem::MaybeUninit::<libc::statfs>::zeroed();
    // SAFETY: `c` is NUL-terminated and `st` is a valid out pointer.
    let rc = unsafe { libc::statfs(c.as_ptr(), st.as_mut_ptr()) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: statfs succeeded and fully initialised the struct.
    let st = unsafe { st.assume_init() };
    let raw = st.f_fsid;
    let mut out = [0u8; 8];
    out[..4].copy_from_slice(&fsid_word(&raw, 0).to_ne_bytes());
    out[4..].copy_from_slice(&fsid_word(&raw, 1).to_ne_bytes());
    Ok(out)
}

fn fsid_word(f: &libc::fsid_t, i: usize) -> i32 {
    // SAFETY: `fsid_t` is `#[repr(C)] { __val: [c_int; 2] }`, so word `i < 2` is in bounds.
    unsafe { *(f as *const libc::fsid_t).cast::<i32>().add(i) }
}
