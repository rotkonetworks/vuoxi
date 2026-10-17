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

#![deny(unsafe_op_in_unsafe_fn)]

fn main() {
    eprintln!("vuoxi-watcher: not implemented yet");
    std::process::exit(1);
}
