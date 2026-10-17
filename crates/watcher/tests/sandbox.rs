//! Runs the real binary's loop under the real seccomp filter, unprivileged.

use std::io::Read;
use std::os::unix::net::UnixListener;
use std::os::unix::process::ExitStatusExt;
use std::process::{Command, Stdio};

use vuoxi_watcher::record::{HELLO, Kind, Record};

fn run(mode: &str) -> (std::process::ExitStatus, String, Vec<u8>) {
    let dir =
        std::env::temp_dir().join(format!("vuoxi-watcher-test-{}-{mode}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let sock = dir.join("s");
    let listener = UnixListener::bind(&sock).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_vuoxi-watcher"))
        .args(["--selftest-sandbox", mode])
        .arg(&sock)
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let (mut conn, _) = listener.accept().unwrap();
    let mut wire = Vec::new();
    conn.read_to_end(&mut wire).unwrap();
    let mut err = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut err)
        .unwrap();
    let status = child.wait().unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
    (status, err, wire)
}

#[test]
fn loop_runs_under_filter_and_emits_records() {
    let (status, err, wire) = run("clean");
    assert!(status.success(), "{status:?}: {err}");
    assert!(err.contains("selftest clean"));
    assert_eq!(&wire[..4], &HELLO);
    let mut rest = &wire[4..];
    let mut kinds = Vec::new();
    while let Some((rec, n)) = Record::decode(rest).unwrap() {
        assert_eq!(rec.name, b"hello");
        kinds.push(rec.kind);
        rest = &rest[n..];
    }
    assert!(rest.is_empty());
    assert_eq!(kinds, [Kind::Create, Kind::Modify]);
}

#[test]
fn forbidden_syscall_kills_with_sigsys() {
    let (status, err, _) = run("violate");
    assert!(err.contains("about to violate"), "{err}");
    assert!(!err.contains("survived"), "{err}");
    assert_eq!(status.signal(), Some(libc::SIGSYS), "{status:?}");
}
