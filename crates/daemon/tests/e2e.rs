//! Two devices on loopback QUIC, one folder, the whole lifecycle.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;
use vuoxi_core::{PeerId, RelPath};
use vuoxi_daemon::{
    BasicFilter, Event, FolderSpec, Node, NodeConfig, PeerSpec, RelayHandler, RelayStore,
};
use vuoxi_layers::Mode;
use vuoxi_transport_iroh::{Allowlist, Config as TransportConfig, IrohTransport};

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
    start_ro(me, other, false).await
}

/// `other_read_only`: `me` accepts nothing from `other` in the folder.
async fn start_ro(me: &Dev, other: &Dev, other_read_only: bool) -> Node {
    Node::start(NodeConfig {
        secret: me.secret,
        bind: vec![format!("127.0.0.1:{}", me.port).parse().unwrap()],
        state_dir: me.state.clone(),
        folders: vec![FolderSpec {
            id: FOLDER.into(),
            root: me.root.clone(),
            filter: Box::new(BasicFilter),
            key: None,
        }],
        peers: vec![PeerSpec {
            id: id_of(&other.secret),
            addrs: vec![format!("127.0.0.1:{}", other.port).parse().unwrap()],
            folders: vec![FOLDER.into()],
            read_only: if other_read_only {
                vec![FOLDER.into()]
            } else {
                vec![]
            },
            relay: false,
        }],
        pull_every: Duration::from_millis(500),
        power_root: None,
        trash_keep: Duration::from_secs(86400),
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
                let r = p.strip_prefix(root).unwrap();
                if r != Path::new("target") && r != Path::new(".vuoxi-trash") {
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

/// (rel, bytes) of everything in the trash, any stamp.
fn trashed(root: &Path) -> Vec<(String, Vec<u8>)> {
    let t = root.join(".vuoxi-trash");
    let Ok(stamps) = fs::read_dir(&t) else {
        return vec![];
    };
    let mut out = vec![];
    for s in stamps {
        let s = s.unwrap().path();
        out.extend(tree(&s));
    }
    out.sort();
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

    // B's previous a.txt, replaced by A's edit, is in B's trash.
    assert_eq!(
        trashed(&b.root),
        vec![("a.txt".to_string(), b"hello".to_vec())]
    );

    // Delete on B: A keeps its copy in the trash.
    fs::remove_file(b.root.join("only-b.md")).unwrap();
    nb.apply_event(FOLDER, Event::Removed(rel("only-b.md")))
        .await
        .unwrap();
    eventually("delete", || !a.root.join("only-b.md").exists()).await;
    assert!(
        trashed(&a.root).contains(&("only-b.md".to_string(), b"from b".to_vec())),
        "a deleted file is recoverable from the trash"
    );

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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn read_only_peer_receives_but_cannot_change() {
    let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/e2e/read-only");
    let _ = fs::remove_dir_all(&base);
    let a = dev(&base, "a", 3);
    let b = dev(&base, "b", 4);
    fs::create_dir_all(&a.root).unwrap();
    fs::create_dir_all(&b.root).unwrap();
    fs::write(a.root.join("doc.txt"), b"original").unwrap();

    // A treats B as read-only; B trusts A normally.
    let na = start_ro(&a, &b, true).await;
    let nb = start(&b, &a).await;
    na.sync_now();
    nb.sync_now();
    eventually("B receives", || {
        fs::read(b.root.join("doc.txt")).ok().as_deref() == Some(b"original")
    })
    .await;

    // B edits and adds; A must not take either, by offer or by pull.
    fs::write(b.root.join("doc.txt"), b"tampered").unwrap();
    fs::write(b.root.join("planted.txt"), b"x").unwrap();
    nb.apply_event(FOLDER, Event::Changed(rel("doc.txt")))
        .await
        .unwrap();
    nb.apply_event(FOLDER, Event::Changed(rel("planted.txt")))
        .await
        .unwrap();
    na.sync_now();
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(fs::read(a.root.join("doc.txt")).unwrap(), b"original");
    assert!(!a.root.join("planted.txt").exists());

    // A still updates B.
    fs::write(a.root.join("doc.txt"), b"from a").unwrap();
    na.apply_event(FOLDER, Event::Changed(rel("doc.txt")))
        .await
        .unwrap();
    eventually("A still reaches B", || {
        fs::read_dir(&b.root)
            .unwrap()
            .any(|e| fs::read(e.unwrap().path()).ok().as_deref() == Some(b"from a"))
    })
    .await;

    na.shutdown().await;
    nb.shutdown().await;
}

/// A device that talks only to the relay `r`.
async fn start_via_relay(me: &Dev, r: &Dev, key: [u8; 32]) -> Node {
    Node::start(NodeConfig {
        secret: me.secret,
        bind: vec![format!("127.0.0.1:{}", me.port).parse().unwrap()],
        state_dir: me.state.clone(),
        folders: vec![FolderSpec {
            id: FOLDER.into(),
            root: me.root.clone(),
            filter: Box::new(BasicFilter),
            key: Some(key),
        }],
        peers: vec![PeerSpec {
            id: id_of(&r.secret),
            addrs: vec![format!("127.0.0.1:{}", r.port).parse().unwrap()],
            folders: vec![FOLDER.into()],
            read_only: vec![],
            relay: true,
        }],
        pull_every: Duration::from_millis(300),
        power_root: None,
        trash_keep: Duration::from_secs(86400),
    })
    .await
    .unwrap()
}

/// Every byte the relay stored, concatenated.
fn relay_bytes(dir: &Path) -> Vec<u8> {
    fn go(d: &Path, out: &mut Vec<u8>) {
        for e in fs::read_dir(d).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                go(&p, out);
            } else {
                out.extend(fs::read(&p).unwrap());
            }
        }
    }
    let mut out = vec![];
    go(dir, &mut out);
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn devices_sync_through_a_relay_that_cannot_read() {
    let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/e2e/relay");
    let _ = fs::remove_dir_all(&base);
    let a = dev(&base, "a", 5);
    let b = dev(&base, "b", 6);
    let r = dev(&base, "r", 7);
    let key = [42u8; 32];
    fs::create_dir_all(&a.root).unwrap();
    fs::create_dir_all(&b.root).unwrap();
    fs::write(a.root.join("diary.txt"), b"SECRET-DIARY-CONTENT").unwrap();
    fs::write(a.root.join("big.bin"), big(9, 700_000)).unwrap();

    // The relay knows the devices' keys (to let them in), never the folder key.
    let mut tc = TransportConfig::new(
        r.secret,
        Allowlist::new([id_of(&a.secret), id_of(&b.secret)]),
    );
    tc.bind = vec![format!("127.0.0.1:{}", r.port).parse().unwrap()];
    let relay = IrohTransport::bind(tc).await.unwrap();
    let _serve = relay.serve(RelayHandler(RelayStore::open(&r.state).unwrap()));

    let na = start_via_relay(&a, &r, key).await;
    let nb = start_via_relay(&b, &r, key).await;
    eventually("A -> relay -> B", || {
        fs::read(b.root.join("diary.txt")).ok().as_deref() == Some(b"SECRET-DIARY-CONTENT")
            && fs::read(b.root.join("big.bin")).ok() == fs::read(a.root.join("big.bin")).ok()
    })
    .await;

    // B changes something and goes away; A, starting later, still gets it.
    na.shutdown().await;
    fs::write(b.root.join("diary.txt"), b"SECRET-EDIT-WHILE-A-SLEPT").unwrap();
    nb.apply_event(FOLDER, Event::Changed(rel("diary.txt")))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(800)).await;
    nb.shutdown().await;
    let na = start_via_relay(&a, &r, key).await;
    eventually("relay holds B's change for A", || {
        fs::read(a.root.join("diary.txt")).ok().as_deref() == Some(b"SECRET-EDIT-WHILE-A-SLEPT")
    })
    .await;

    // Nothing on the relay's disk is readable: no content, no names.
    let stored = relay_bytes(&r.state);
    assert!(stored.len() > 700_000, "the relay does hold the data");
    for needle in [&b"SECRET"[..], b"diary", b"big.bin", FOLDER.as_bytes()] {
        assert!(
            !stored.windows(needle.len()).any(|w| w == needle),
            "relay stored {:?} in the clear",
            String::from_utf8_lossy(needle)
        );
    }
    let plain = big(9, 700_000);
    assert!(
        !stored.windows(64).any(|w| w == &plain[1000..1064]),
        "file bytes in the clear"
    );

    na.shutdown().await;
}
