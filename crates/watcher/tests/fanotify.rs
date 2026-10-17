//! Live fanotify test. Needs `CAP_SYS_ADMIN`, so it is `#[ignore]`d.
//!
//! Run it as root, in a scratch target dir so root does not own your build:
//!
//! ```text
//! CARGO_TARGET_DIR=/var/tmp/vuoxi-root-target \
//!     sudo -E env "PATH=$PATH" cargo test -p vuoxi-watcher --test fanotify -- --ignored
//! ```
//!
//! The scratch directory lives under `VUOXI_TEST_DIR` (default: the system
//! temp dir); its filesystem must support file handles (ext4, xfs, btrfs, tmpfs).

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::time::{Duration, Instant};

use vuoxi_watcher::ignore::IgnoreTracker;
use vuoxi_watcher::parse::parse_events;
use vuoxi_watcher::record::{Kind, Record};
use vuoxi_watcher::sys::Group;

#[test]
#[ignore = "needs root (CAP_SYS_ADMIN); see module docs"]
fn sees_create_modify_delete_and_filters_target() {
    let base = std::env::var_os("VUOXI_TEST_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let dir = base.join(format!("vuoxi-fanotify-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();

    let mut group = Group::init().expect("fanotify_init (are you root?)");
    group
        .mark_filesystem(&CString::new(dir.as_os_str().as_bytes()).unwrap())
        .unwrap();

    std::fs::create_dir(dir.join("target")).unwrap();
    std::fs::write(dir.join("target").join("junk.o"), b"x").unwrap();
    std::fs::write(dir.join("keep.txt"), b"hello").unwrap();
    std::fs::remove_file(dir.join("keep.txt")).unwrap();

    // Drain until the delete shows up; the queue is nonblocking-free, so bound by time.
    let mut records = Vec::new();
    let mut buf = vec![0u8; 64 * 1024];
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline
        && !records
            .iter()
            .any(|r: &Record| r.kind == Kind::Delete && r.name == b"keep.txt")
    {
        let n = group.read(&mut buf).unwrap();
        parse_events(&buf[..n], &mut records).unwrap();
    }
    std::fs::remove_dir_all(&dir).unwrap();

    let mut tracker = IgnoreTracker::default();
    let kept: Vec<_> = records.iter().filter(|r| tracker.admit(r)).collect();
    let has = |k: Kind, name: &[u8]| kept.iter().any(|r| r.kind == k && r.name == name);
    assert!(has(Kind::Create, b"keep.txt"));
    assert!(has(Kind::Modify, b"keep.txt"));
    assert!(has(Kind::Delete, b"keep.txt"));
    assert!(
        kept.iter()
            .all(|r| r.name != b"target" && r.name != b"junk.o")
    );
    assert!(
        kept.iter()
            .all(|r| r.object.is_some() && r.parent.is_some())
    );
}
