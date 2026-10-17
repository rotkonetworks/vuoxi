//! Two devices on loopback QUIC, one folder, the whole lifecycle.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;
use vuoxi_core::{PeerId, RelPath};
use vuoxi_daemon::{BasicFilter, Event, FolderSpec, Node, NodeConfig, PeerSpec};
use vuoxi_layers::Mode;

const FOLDER: &str = "test";

struct Dev {
    secret: [u8; 32],
    port: u16,
    root: PathBuf,
    state: PathBuf,
}

fn id_of(secret: &[u8; 32]) -> PeerId {
    PeerId(
        ed25519_dalek::SigningKey::from_bytes(secret)
            .verifying_key()
            .to_bytes(),
    )
}

fn free_port() -> u16 {
    std::net::UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn dev(base: &Path, name: &str, seed: u8) -> Dev {
    let d = base.join(name);
    Dev {
        secret: [seed; 32],
        port: free_port(),
        root: d.join("root"),
        state: d.join("state"),
    }
}

async fn start(me: &Dev, other: &Dev) -> Node {
    Node::start(NodeConfig {
        secret: me.secret,
        bind: vec![format!("127.0.0.1:{}", me.port).parse().unwrap()],
        state_dir: me.state.clone(),
        folders: vec![FolderSpec {
            id: FOLDER.into(),
            root: me.root.clone(),
            filter: Box::new(BasicFilter),
        }],
        peers: vec![PeerSpec {
            id: id_of(&other.secret),
            addrs: vec![format!("127.0.0.1:{}", other.port).parse().unwrap()],
            folders: vec![FOLDER.into()],
        }],
        pull_every: Duration::from_millis(500),
        power_root: None,
    })
    .await
    .unwrap()
}

fn rel(s: &str) -> RelPath {
    RelPath::new(s).unwrap()
}

/// Every regular file under `root` with its bytes, skipping our temp files and
/// the cache dir that never syncs.
fn tree(root: &Path) -> BTreeMap<String, Vec<u8>> {
    fn go(root: &Path, dir: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
        for e in fs::read_dir(dir).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                if p.strip_prefix(root).unwrap() != Path::new("target") {
                    go(root, &p, out);
                }
            } else if !p.to_string_lossy().ends_with(".vuoxi-tmp") {
                let k = p.strip_prefix(root).unwrap().to_string_lossy().into_owned();
                out.insert(k, fs::read(&p).unwrap());
            }
        }
    }
    let mut out = BTreeMap::new();
    go(root, root, &mut out);
    out
}

fn keys(root: &Path) -> Vec<String> {
    tree(root).into_keys().collect()
}

async fn eventually(what: &str, mut ok: impl FnMut() -> bool) {
    for _ in 0..150 {
        if ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("timed out waiting for: {what}");
}

fn big(seed: u64, len: usize) -> Vec<u8> {
    let mut x = seed;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_devices_converge() {
    let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/e2e/converge");
    let _ = fs::remove_dir_all(&base);
    let a = dev(&base, "a", 1);
    let b = dev(&base, "b", 2);

    // First import, both directions, plus a cache dir that must stay home.
    fs::create_dir_all(a.root.join("sub")).unwrap();
    fs::create_dir_all(a.root.join("target")).unwrap();
    fs::create_dir_all(&b.root).unwrap();
    fs::write(a.root.join("a.txt"), b"hello").unwrap();
    fs::write(a.root.join("sub/b.bin"), big(7, 1 << 20)).unwrap();
    fs::write(
        a.root.join("target/CACHEDIR.TAG"),
        b"Signature: 8a477f597d28d172789f06886806bc55\n",
    )
    .unwrap();
    fs::write(a.root.join("target/x.o"), b"build output").unwrap();
    fs::write(b.root.join("only-b.md"), b"from b").unwrap();

    let na = start(&a, &b).await;
    let mut nb = start(&b, &a).await;
    na.sync_now();
    nb.sync_now();
    eventually("first import", || {
        let (ta, tb) = (tree(&a.root), tree(&b.root));
        tb.get("a.txt").map(Vec::as_slice) == Some(b"hello")
            && tb.get("sub/b.bin") == ta.get("sub/b.bin")
            && ta.get("only-b.md").map(Vec::as_slice) == Some(b"from b")
    })
    .await;
    assert!(
        !b.root.join("target").exists(),
        "cache dirs never leave home"
    );

    // Modify, with mode and mtime carried across.
    fs::write(a.root.join("a.txt"), b"hello world").unwrap();
    fs::set_permissions(a.root.join("a.txt"), fs::Permissions::from_mode(0o600)).unwrap();
    na.apply_event(FOLDER, Event::Changed(rel("a.txt")))
        .await
        .unwrap();
    eventually("modify", || {
        fs::read(b.root.join("a.txt")).ok().as_deref() == Some(b"hello world")
    })
    .await;
    let (ma, mb) = (
        fs::metadata(a.root.join("a.txt")).unwrap(),
        fs::metadata(b.root.join("a.txt")).unwrap(),
    );
    assert_eq!(mb.mode() & 0o7777, 0o600);
    assert_eq!((mb.mtime(), mb.mtime_nsec()), (ma.mtime(), ma.mtime_nsec()));

    // Our own write, seen by B's detector, is not a new version. Wait until B
    // has recorded the file it wrote (the rename lands before the record).
    eventually("B recorded a.txt", || {
        nb.with_folder(FOLDER, |f| f.known_meta(&rel("a.txt")))
            .flatten()
            .is_some_and(|m| m.mode == 0o600)
    })
    .await;
    let head = nb.with_folder(FOLDER, |f| f.head()).unwrap();
    nb.apply_event(FOLDER, Event::Changed(rel("a.txt")))
        .await
        .unwrap();
    assert_eq!(
        nb.with_folder(FOLDER, |f| f.head()).unwrap(),
        head,
        "echo suppressed"
    );

    // Delete on B.
    fs::remove_file(b.root.join("only-b.md")).unwrap();
    nb.apply_event(FOLDER, Event::Removed(rel("only-b.md")))
        .await
        .unwrap();
    eventually("delete", || !a.root.join("only-b.md").exists()).await;

    // Rename on A: a tombstone and a new file whose chunks B already has.
    fs::rename(a.root.join("sub/b.bin"), a.root.join("sub/c.bin")).unwrap();
    na.apply_event(
        FOLDER,
        Event::Moved {
            from: rel("sub/b.bin"),
            to: rel("sub/c.bin"),
        },
    )
    .await
    .unwrap();
    eventually("rename", || {
        b.root.join("sub/c.bin").exists() && !b.root.join("sub/b.bin").exists()
    })
    .await;
    assert_eq!(keys(&a.root), keys(&b.root));
    assert_eq!(tree(&a.root), tree(&b.root));

    // Conflict: both edit while held apart, then meet.
    na.mode().set(Mode::Hold);
    nb.mode().set(Mode::Hold);
    fs::write(a.root.join("x.txt"), b"from a").unwrap();
    na.apply_event(FOLDER, Event::Changed(rel("x.txt")))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    fs::write(b.root.join("x.txt"), b"from b, later").unwrap();
    nb.apply_event(FOLDER, Event::Changed(rel("x.txt")))
        .await
        .unwrap();
    na.mode().set(Mode::Live);
    nb.mode().set(Mode::Live);
    na.sync_now();
    nb.sync_now();
    eventually("conflict converges", || {
        let (ta, tb) = (tree(&a.root), tree(&b.root));
        ta == tb && ta.keys().filter(|k| k.contains(".vuoxi-conflict-")).count() == 1
    })
    .await;
    let t = tree(&a.root);
    assert_eq!(
        t["x.txt"], b"from b, later",
        "the newer edit keeps the path"
    );
    let copy = t
        .iter()
        .find(|(k, _)| k.contains(".vuoxi-conflict-"))
        .unwrap();
    assert_eq!(copy.1, b"from a", "the other edit is kept beside it");

    // Restart: B misses a change while down and catches up from its cursor.
    nb.shutdown().await;
    fs::write(a.root.join("while-down.txt"), b"missed").unwrap();
    na.apply_event(FOLDER, Event::Changed(rel("while-down.txt")))
        .await
        .unwrap();
    nb = start(&b, &a).await;
    nb.sync_now();
    eventually("catch up after restart", || {
        fs::read(b.root.join("while-down.txt")).ok().as_deref() == Some(b"missed")
    })
    .await;
    assert_eq!(keys(&a.root), keys(&b.root));
    assert_eq!(tree(&a.root), tree(&b.root));
    assert_eq!(
        na.with_folder(FOLDER, |f| f.root_hash()),
        nb.with_folder(FOLDER, |f| f.root_hash()),
        "index roots agree once the folders do"
    );

    na.shutdown().await;
    nb.shutdown().await;
}
