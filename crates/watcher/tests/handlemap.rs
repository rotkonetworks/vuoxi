//! Live test of [`HandleMap`] against real fanotify events. Needs
//! `CAP_SYS_ADMIN`, so it is `#[ignore]`d.
//!
//! Run it as root, in a scratch target dir so root does not own your build:
//!
//! ```text
//! CARGO_TARGET_DIR=/var/tmp/vuoxi-root-target \
//!     sudo -E env "PATH=$PATH" cargo test -p vuoxi-watcher --test handlemap -- --ignored
//! ```
//!
//! The scratch directory lives under `VUOXI_TEST_DIR` (default: the system
//! temp dir); its filesystem must support file handles (ext4, xfs, btrfs, tmpfs).

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::time::{Duration, Instant};

use vuoxi_watcher::handlemap::{HandleMap, Resolved};
use vuoxi_watcher::parse::parse_events;
use vuoxi_watcher::record::Record;
use vuoxi_watcher::sys::Group;

#[test]
#[ignore = "needs root (CAP_SYS_ADMIN); see module docs"]
fn resolves_real_events_to_paths() {
    let base = std::env::var_os("VUOXI_TEST_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let dir = base.join(format!("vuoxi-handlemap-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("root")).unwrap();
    let root = dir.join("root");

    let mut group = Group::init().expect("fanotify_init (are you root?)");
    group
        .mark_filesystem(&CString::new(root.as_os_str().as_bytes()).unwrap())
        .unwrap();
    let mut map = HandleMap::build(&root, |_| false).unwrap();

    std::fs::create_dir(root.join("d")).unwrap();
    std::fs::write(root.join("d/f"), b"hello").unwrap();
    std::fs::rename(root.join("d"), root.join("e")).unwrap();
    std::fs::remove_file(root.join("e/f")).unwrap();
    std::fs::remove_dir(root.join("e")).unwrap();

    let changed = |rel: &str, is_dir| Resolved::Changed {
        rel: rel.into(),
        is_dir,
    };
    let want = [
        changed("d", true),
        changed("d/f", false),
        Resolved::Moved {
            from: "d".into(),
            to: "e".into(),
            is_dir: true,
        },
        Resolved::Removed {
            rel: "e/f".into(),
            is_dir: false,
        },
        Resolved::Removed {
            rel: "e".into(),
            is_dir: true,
        },
    ];

    // The mark covers the whole filesystem, so unrelated events arrive too;
    // the map reports those as Outside. Reads block, so bound by the expected
    // final event; the queue already holds everything above.
    let mut got: Vec<Resolved> = Vec::new();
    let mut buf = vec![0u8; 64 * 1024];
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline && got.last() != want.last() {
        let n = group.read(&mut buf).unwrap();
        let mut records: Vec<Record> = Vec::new();
        parse_events(&buf[..n], &mut records).unwrap();
        let mut out = Vec::new();
        for r in &records {
            map.resolve(r, |_| false, &mut out);
        }
        map.flush(&mut out);
        for r in out {
            // a write is Create + Modify (+ more): collapse repeats
            if r != Resolved::Outside && got.last() != Some(&r) {
                got.push(r);
            }
        }
    }
    std::fs::remove_dir_all(&dir).unwrap();
    assert_eq!(got, want);
}
