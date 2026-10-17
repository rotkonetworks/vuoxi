//! vuoxi-watcher: the only part of vuoxi that runs with privilege.
//!
//! It holds one fanotify filesystem mark (CAP_SYS_ADMIN), turns kernel events
//! into opaque change records, and writes them to the unprivileged daemon over
//! a socket. It has no network, no parser for peer data, and drops every
//! capability except the one fanotify needs before entering its loop; a seccomp
//! filter then pins it to the handful of syscalls that loop uses.
//!
//! Unsafe is allowed here only for the fanotify bindings, and nowhere else in
//! the workspace.
//!
//! ```text
//! vuoxi-watcher <mount-point> <daemon-socket>
//! ```
//!
//! The daemon listens on `<daemon-socket>`; the watcher connects, sends
//! [`HELLO`](vuoxi_watcher::record::HELLO), then frames.

#![deny(unsafe_op_in_unsafe_fn)]

use std::ffi::CString;
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::ExitCode;

use vuoxi_watcher::ignore::IgnoreTracker;
use vuoxi_watcher::parse::{FAN_CREATE, FAN_MODIFY, WANTED, build_event};
use vuoxi_watcher::pump::{Pump, Step};
use vuoxi_watcher::record::{HELLO, Handle};
use vuoxi_watcher::sandbox::{self, Fds};
use vuoxi_watcher::sys::{self, Group};

fn main() -> ExitCode {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    let res = match args.iter().map(|a| a.to_str()).collect::<Vec<_>>()[..] {
        [Some("--selftest-sandbox"), Some(mode), _] => selftest(mode, Path::new(&args[2])),
        [_, _] => run(Path::new(&args[0]), Path::new(&args[1])),
        _ => {
            eprintln!("usage: vuoxi-watcher <mount-point> <daemon-socket>");
            return ExitCode::from(2);
        }
    };
    match res {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("vuoxi-watcher: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(mount: &Path, socket: &Path) -> Result<(), Box<dyn std::error::Error>> {
    // Everything that needs privilege or allocation-time setup happens first.
    let mut group = Group::init().map_err(|e| format!("fanotify_init: {e}"))?;
    let mount_c = CString::new(mount.as_os_str().as_bytes())?;
    group
        .mark_filesystem(&mount_c)
        .map_err(|e| format!("fanotify_mark {}: {e}", mount.display()))?;
    debug_assert_ne!(WANTED, 0);
    let mut out =
        UnixStream::connect(socket).map_err(|e| format!("connect {}: {e}", socket.display()))?;
    out.write_all(&HELLO)?;
    let mut pump = Pump::new(IgnoreTracker::default());

    sandbox::drop_capabilities()?;
    sandbox::set_no_new_privs()?;
    sandbox::install_seccomp(Fds {
        input: group.raw_fd(),
        output: out.as_raw_fd(),
    })?;

    loop {
        match pump.step(&mut ReadAdapter(&mut group), &mut out)? {
            Step::Forwarded(_) => {}
            Step::Eof => return Ok(()),
        }
    }
}

struct ReadAdapter<'a>(&'a mut Group);

impl io::Read for ReadAdapter<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.0.read(buf)
    }
}

/// Hidden test hook: runs the real loop under the real seccomp filter, with a
/// socketpair feeding synthetic events instead of fanotify. Needs no root.
/// `clean` should exit 0 after forwarding two records; `violate` attempts
/// `socket(2)` and must die of SIGSYS.
fn selftest(mode: &str, socket: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let (mut src, mut feed) = std::io::pipe()?;
    let h = |n: u8| Handle {
        ty: 1,
        bytes: vec![n; 8],
    };
    feed.write_all(&build_event(
        FAN_CREATE,
        [0; 8],
        Some(&h(2)),
        Some((&h(1), b"hello")),
    ))?;
    feed.write_all(&build_event(
        FAN_MODIFY,
        [0; 8],
        Some(&h(2)),
        Some((&h(1), b"hello")),
    ))?;
    drop(feed);
    let mut out = UnixStream::connect(socket)?;
    out.write_all(&HELLO)?;
    let mut pump = Pump::new(IgnoreTracker::default());

    sandbox::set_no_new_privs()?;
    sandbox::install_seccomp(Fds {
        input: src.as_raw_fd(),
        output: out.as_raw_fd(),
    })?;

    if mode == "violate" {
        eprintln!("about to violate");
        let rc = sys::probe_forbidden_syscall();
        eprintln!("survived socket(): {rc}");
        return Err("filter did not fire".into());
    }
    while pump.step(&mut src, &mut out)? != Step::Eof {}
    eprintln!("selftest clean");
    Ok(())
}
