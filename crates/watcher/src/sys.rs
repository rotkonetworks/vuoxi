//! The only unsafe code in vuoxi: fanotify setup and the sandbox probe.
//!
//! `nix` 0.31's fanotify module cannot be used: it lacks the `FAN_REPORT_*`
//! init flags and its `read_events` discards the info records that carry the
//! file handles and names. So we call `fanotify_init`/`fanotify_mark` from
//! `libc` directly. Reading is plain `read(2)` on a [`File`], which is safe.

use std::ffi::CStr;
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

use crate::parse::WANTED;

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
