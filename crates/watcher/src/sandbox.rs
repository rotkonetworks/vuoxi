//! Privilege hardening, applied in this order once the fanotify mark is set
//! and the daemon socket is connected:
//!
//! 1. [`drop_capabilities`]: everything but `CAP_SYS_ADMIN`, from the bounding,
//!    inheritable, ambient, permitted and effective sets.
//! 2. [`set_no_new_privs`].
//! 3. [`install_seccomp`]: kill the process on any syscall outside [`ALLOWED`].
//!
//! # Syscall allowlist
//!
//! The loop is `read(fanotify)` → parse → `write(socket)`; the rest is the
//! allocator, panic and exit paths.
//!
//! | syscall | why | restriction |
//! |---|---|---|
//! | `read` | next batch of events | fd must be the fanotify fd |
//! | `write` `sendto` | frames to the daemon (std uses `sendto`); panic message | fd must be the socket (or stderr for `write`) |
//! | `close` `fcntl` | descriptor hygiene | `fcntl` only with `F_GETFD` |
//! | `mmap` `munmap` `mremap` `mprotect` `brk` `madvise` | allocator | none |
//! | `futex` | std's panic/exit machinery | none |
//! | `sigaltstack` `rt_sigprocmask` `rt_sigreturn` | stack-overflow handler, abort | none |
//! | `tgkill` `gettid` `getpid` | `abort()` after a double panic | none |
//! | `getrandom` | hash seed, if first used after the filter | none |
//! | `exit` `exit_group` `restart_syscall` | termination, EINTR restart | none |
//!
//! Notably absent: `socket`, `connect`, `open*`, `execve`, `ptrace`, `clone`,
//! `fanotify_*`. The socket is connected before the filter, so the watcher
//! can never reach the network.

use std::collections::{BTreeMap, HashSet};
use std::os::fd::RawFd;

use seccompiler::{
    BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter,
    SeccompRule, TargetArch,
};

/// Descriptors the loop may touch.
#[derive(Clone, Copy, Debug)]
pub struct Fds {
    /// Where events are read from (the fanotify group).
    pub input: RawFd,
    /// Where frames are written (the daemon socket).
    pub output: RawFd,
}

/// Syscalls allowed without argument checks (see the module table).
pub const ALLOWED: &[(&str, i64)] = &[
    ("close", libc::SYS_close),
    ("mmap", libc::SYS_mmap),
    ("munmap", libc::SYS_munmap),
    ("mremap", libc::SYS_mremap),
    ("mprotect", libc::SYS_mprotect),
    ("brk", libc::SYS_brk),
    ("madvise", libc::SYS_madvise),
    ("futex", libc::SYS_futex),
    ("sigaltstack", libc::SYS_sigaltstack),
    ("rt_sigprocmask", libc::SYS_rt_sigprocmask),
    ("rt_sigreturn", libc::SYS_rt_sigreturn),
    ("tgkill", libc::SYS_tgkill),
    ("gettid", libc::SYS_gettid),
    ("getpid", libc::SYS_getpid),
    ("getrandom", libc::SYS_getrandom),
    ("exit", libc::SYS_exit),
    ("exit_group", libc::SYS_exit_group),
    ("restart_syscall", libc::SYS_restart_syscall),
];

#[derive(Debug)]
pub enum Error {
    Caps(caps::errors::CapsError),
    Nix(nix::Error),
    Seccomp(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Caps(e) => write!(f, "capabilities: {e}"),
            Error::Nix(e) => write!(f, "prctl: {e}"),
            Error::Seccomp(e) => write!(f, "seccomp: {e}"),
        }
    }
}

impl std::error::Error for Error {}

/// Keep only `CAP_SYS_ADMIN`. Needs `CAP_SETPCAP` to prune the bounding set,
/// which a root start has; call it before [`set_no_new_privs`] and the filter.
pub fn drop_capabilities() -> Result<(), Error> {
    use caps::{CapSet, Capability};
    let keep = Capability::CAP_SYS_ADMIN;
    // The bounding set first: it needs CAP_SETPCAP, which the permitted drop removes.
    for cap in caps::read(None, CapSet::Bounding).map_err(Error::Caps)? {
        if cap != keep {
            caps::drop(None, CapSet::Bounding, cap).map_err(Error::Caps)?;
        }
    }
    caps::clear(None, CapSet::Ambient).map_err(Error::Caps)?;
    caps::clear(None, CapSet::Inheritable).map_err(Error::Caps)?;
    let only: HashSet<Capability> = [keep].into();
    caps::set(None, CapSet::Effective, &only).map_err(Error::Caps)?;
    caps::set(None, CapSet::Permitted, &only).map_err(Error::Caps)?;
    Ok(())
}

pub fn set_no_new_privs() -> Result<(), Error> {
    nix::sys::prctl::set_no_new_privs().map_err(Error::Nix)
}

fn fd_is(fd: RawFd) -> Result<SeccompRule, Error> {
    let cond = SeccompCondition::new(0, SeccompCmpArgLen::Dword, SeccompCmpOp::Eq, fd as u64)
        .map_err(|e| Error::Seccomp(e.to_string()))?;
    SeccompRule::new(vec![cond]).map_err(|e| Error::Seccomp(e.to_string()))
}

/// Compile the filter without installing it.
pub fn seccomp_program(fds: Fds) -> Result<BpfProgram, Error> {
    let mut rules: BTreeMap<i64, Vec<SeccompRule>> =
        ALLOWED.iter().map(|&(_, nr)| (nr, Vec::new())).collect();
    rules.insert(libc::SYS_read, vec![fd_is(fds.input)?]);
    rules.insert(libc::SYS_write, vec![fd_is(fds.output)?, fd_is(2)?]);
    // std's debug builds check `F_GETFD` before closing an owned fd.
    let getfd = SeccompCondition::new(
        1,
        SeccompCmpArgLen::Dword,
        SeccompCmpOp::Eq,
        libc::F_GETFD as u64,
    )
    .and_then(|c| SeccompRule::new(vec![c]))
    .map_err(|e| Error::Seccomp(e.to_string()))?;
    rules.insert(libc::SYS_fcntl, vec![getfd]);
    // std sends on a UnixStream with send(2), which is sendto on every arch.
    rules.insert(libc::SYS_sendto, vec![fd_is(fds.output)?]);
    let arch =
        TargetArch::try_from(std::env::consts::ARCH).map_err(|e| Error::Seccomp(e.to_string()))?;
    let filter = SeccompFilter::new(
        rules,
        SeccompAction::KillProcess,
        SeccompAction::Allow,
        arch,
    )
    .map_err(|e| Error::Seccomp(e.to_string()))?;
    filter
        .try_into()
        .map_err(|e: seccompiler::BackendError| Error::Seccomp(e.to_string()))
}

/// Install the filter on the whole process (all threads).
pub fn install_seccomp(fds: Fds) -> Result<(), Error> {
    let prog = seccomp_program(fds)?;
    seccompiler::apply_filter_all_threads(&prog).map_err(|e| Error::Seccomp(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_compiles() {
        let prog = seccomp_program(Fds {
            input: 3,
            output: 4,
        })
        .unwrap();
        assert!(!prog.is_empty());
    }

    #[test]
    fn allowlist_has_no_duplicates_or_network() {
        let mut seen = HashSet::new();
        for &(name, nr) in ALLOWED {
            assert!(seen.insert(nr), "{name} listed twice");
            assert_ne!(nr, libc::SYS_socket);
            assert_ne!(nr, libc::SYS_connect);
            assert_ne!(nr, libc::SYS_execve);
        }
    }
}
