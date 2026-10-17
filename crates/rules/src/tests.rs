use std::fs;
use std::path::{Path, PathBuf};

use super::*;

struct Tree(PathBuf);

impl Tree {
    fn new(name: &str) -> Tree {
        let p = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/tmp")
            .join(format!("rules-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        Tree(p)
    }
    fn write(&self, rel: &str, body: &str) -> &Self {
        let p = self.0.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, body).unwrap();
        self
    }
    fn mkdir(&self, rel: &str) -> &Self {
        fs::create_dir_all(self.0.join(rel)).unwrap();
        self
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn rp(s: &str) -> RelPath {
    RelPath::new(s).unwrap()
}

#[test]
fn cachedir_tag_skips() {
    let t = Tree::new("cachedir");
    t.write(
        "out/CACHEDIR.TAG",
        &format!("{CACHEDIR_SIGNATURE}\n# cache"),
    );
    t.write("fake/CACHEDIR.TAG", "hello");
    let mut r = Rules::new(&t.0, Tag::Code);
    assert_eq!(r.dir(&rp("out")), Decision::Skip(Reason::CacheDirTag));
    assert_eq!(r.dir(&rp("fake")), Decision::Sync(Reason::Default));
}

#[test]
fn rebuildable_needs_manifest() {
    let t = Tree::new("rebuild");
    t.write("a/Cargo.toml", "")
        .mkdir("a/target")
        .mkdir("b/target");
    t.write("n/package.json", "{}")
        .mkdir("n/node_modules")
        .mkdir("n/dist");
    t.write("v/pyvenv.cfg", "").mkdir("p/__pycache__");
    let mut r = Rules::new(&t.0, Tag::Code);
    assert!(matches!(
        r.dir(&rp("a/target")),
        Decision::Skip(Reason::Rebuildable("target/ beside Cargo.toml"))
    ));
    assert_eq!(r.dir(&rp("b/target")), Decision::Sync(Reason::Default));
    assert!(!r.dir(&rp("n/node_modules")).is_sync());
    assert!(!r.dir(&rp("n/dist")).is_sync());
    assert!(!r.dir(&rp("v")).is_sync());
    assert!(!r.dir(&rp("p/__pycache__")).is_sync());
}

#[test]
fn keep_beats_skip() {
    let t = Tree::new("keep");
    t.write("a/Cargo.toml", "").mkdir("a/target");
    t.write(".vuoxi", "keep = [\"a/target/\"]\nignore = [\"*.log\"]\n");
    t.write("a/target/x.bin", "");
    t.write("run.log", "");
    t.write("a/target/y.log", "");
    let mut r = Rules::new(&t.0, Tag::Code);
    assert_eq!(
        r.dir(&rp("a/target")),
        Decision::Sync(Reason::Keep("a/target/".into()))
    );
    assert!(matches!(
        r.file(&rp("a/target/x.bin")),
        Decision::Sync(Reason::Keep(_))
    ));
    assert!(matches!(
        r.file(&rp("a/target/y.log")),
        Decision::Sync(Reason::Keep(_))
    ));
    assert_eq!(
        r.file(&rp("run.log")),
        Decision::Skip(Reason::Ignore("*.log".into()))
    );
}

#[test]
fn ignore_is_relative_to_its_directory_and_nearest_wins() {
    let t = Tree::new("ignore-rel");
    t.write("sub/.vuoxi", "ignore = [\"/local.txt\", \"coverage/\"]\n");
    t.write("sub/deep/.vuoxi", "ignore = [\"!coverage/\"]\n");
    let mut r = Rules::new(&t.0, Tag::Code);
    assert!(!r.file(&rp("sub/local.txt")).is_sync());
    assert!(r.file(&rp("local.txt")).is_sync());
    assert!(r.file(&rp("sub/deep/local.txt")).is_sync());
    assert!(!r.dir(&rp("sub/coverage")).is_sync());
    assert!(r.dir(&rp("sub/deep/coverage")).is_sync());
}

#[test]
fn git_dir_skipped() {
    let t = Tree::new("git");
    t.mkdir("repo/.git");
    let mut r = Rules::new(&t.0, Tag::Code);
    assert_eq!(r.dir(&rp("repo/.git")), Decision::Skip(Reason::GitDir));
    assert_eq!(
        r.file(&rp("repo/.git/HEAD")),
        Decision::Skip(Reason::GitDir)
    );
}

#[test]
fn env_inside_code_is_secret() {
    let rel = rp("proj/sub/.env.local");
    assert!(Rules::is_secret_file(&rel));
    for s in [
        "a/id_ed25519",
        "x.pem",
        "k.KEY",
        "v.kdbx",
        "s.age",
        "s.gpg",
        "wallet.dat",
    ] {
        assert!(Rules::is_secret_file(&rp(s)), "{s}");
    }
    for s in ["a/id_ed25519.pub", ".env.example", "main.rs", "keyboard.md"] {
        assert!(!Rules::is_secret_file(&rp(s)), "{s}");
    }
    let t = Tree::new("envpol");
    t.write("proj/.env", "");
    let mut r = Rules::new(&t.0, Tag::Code);
    assert!(r.policy(Some(&rp("proj"))).relays);
    let p = r.file_policy(&rp("proj/.env"));
    assert_eq!(
        (p.tag, p.relays, p.agents_read),
        (Tag::Secrets, false, false)
    );
    let w = r.why(&rp("proj/.env"), false);
    assert!(w.secret_file && !w.policy.relays);
}

#[test]
fn nested_code_under_secrets_stays_sealed() {
    let t = Tree::new("sealed");
    t.write("vault/.vuoxi", "tag = \"code\"\n");
    t.write("vault2/.vuoxi", "tag = \"code\"\nunseal = [\"relays\"]\n");
    t.mkdir("vault/deep");
    let mut r = Rules::new(&t.0, Tag::Secrets);
    let p = r.policy(Some(&rp("vault")));
    assert_eq!(p.tag, Tag::Code);
    assert!(!p.relays && !p.agents_read);
    assert_eq!(p.priority, Priority::Normal);
    assert!(!r.policy(Some(&rp("vault/deep"))).relays);
    let q = r.policy(Some(&rp("vault2")));
    assert!(q.relays);
    assert!(!q.agents_read);
    // Tightening downward from code works too.
    let t2 = Tree::new("tighten");
    t2.write("s/.vuoxi", "tag = \"secrets\"\n")
        .write("s/c/.vuoxi", "tag = \"notes\"\n");
    let mut r2 = Rules::new(&t2.0, Tag::Code);
    assert!(!r2.policy(Some(&rp("s/c"))).relays);
    let w = r2.why(&rp("s/c/f.md"), false);
    assert_eq!(w.chain.len(), 3);
    assert_eq!(w.chain[1].policy.tag, Tag::Secrets);
}

#[test]
fn gitignore_exclude_vs_keep_vs_ask() {
    let t = Tree::new("gitignore");
    t.write(".gitignore", "*.db\nscratch/\n.env\n");
    t.write("a.db", "").mkdir("scratch");
    let mut keep = Rules::new(&t.0, Tag::Code);
    assert_eq!(keep.file(&rp("a.db")), Decision::Sync(Reason::Default));
    assert_eq!(keep.dir(&rp("scratch")), Decision::Sync(Reason::Default));

    t.write(".vuoxi", "gitignore = \"exclude\"\n");
    let mut ex = Rules::new(&t.0, Tag::Code);
    assert_eq!(
        ex.file(&rp("a.db")),
        Decision::Skip(Reason::Gitignored("*.db".into()))
    );
    assert!(!ex.dir(&rp("scratch")).is_sync());
    assert!(!ex.file(&rp("scratch/x")).is_sync());
    assert!(ex.file(&rp(".env")).is_sync(), "secrets survive exclusion");

    t.write(".vuoxi", "gitignore = \"ask\"\n");
    let mut ask = Rules::new(&t.0, Tag::Code);
    assert_eq!(
        ask.file(&rp("a.db")),
        Decision::Sync(Reason::Gitignored("*.db".into()))
    );
}

#[test]
fn malformed_vuoxi_is_ignored_with_error() {
    let t = Tree::new("malformed");
    t.write("a/.vuoxi", "tag = [oops\n");
    t.write("b/.vuoxi", "tag = \"nonsense\"\n");
    t.write("c/.vuoxi", "ignore = [\"ok\"]\n");
    let mut r = Rules::new(&t.0, Tag::Notes);
    assert_eq!(r.policy(Some(&rp("a"))).tag, Tag::Notes);
    assert_eq!(r.policy(Some(&rp("b"))).tag, Tag::Notes);
    assert!(r.file(&rp("a/x")).is_sync());
    let errs = r.errors();
    assert_eq!(errs.len(), 2);
    assert_eq!(errs[0].dir, Some(rp("a")));
    assert!(r.errors().iter().all(|e| !e.message.is_empty()));
    assert!(!r.file(&rp("c/ok")).is_sync());
    // Fixing the file and invalidating clears the error.
    t.write("a/.vuoxi", "tag = \"code\"\n");
    r.invalidate(Some(&rp("a")));
    assert_eq!(r.policy(Some(&rp("a"))).tag, Tag::Code);
    assert_eq!(r.errors().len(), 1);
}

#[test]
fn tag_skip_and_priority_override() {
    let t = Tree::new("tagskip");
    t.write(".vuoxi", "priority = \"bulk\"\n");
    let mut r = Rules::new(&t.0, Tag::Notes);
    assert_eq!(r.policy(None).priority, Priority::Bulk);
    assert!(!r.file(&rp("n/.todo.swp")).is_sync());
    assert!(r.file(&rp("n/todo.md")).is_sync());
}

#[test]
fn detect_code_and_secrets() {
    let t = Tree::new("detect-code");
    t.write("p/Cargo.toml", "")
        .mkdir("p/.git")
        .mkdir("p/target")
        .write("p/src/main.rs", "");
    t.write("p/.env", "");
    let d = detect(&t.0, 1000);
    assert_eq!(d.tag, Some(Tag::Code));
    assert!(d.confidence >= 0.9);
    assert_eq!(
        (
            d.signals.git_dirs,
            d.signals.cargo_manifests,
            d.signals.env_files
        ),
        (1, 1, 1)
    );

    let v = Tree::new("detect-secrets");
    v.write("a.kdbx", "")
        .write("sub/b.kdbx", "")
        .write("c.kdbx", "")
        .write("readme.txt", "");
    let d = detect(&v.0, 1000);
    assert_eq!(d.tag, Some(Tag::Secrets));
    assert_eq!(d.signals.secret_files, 3);

    let e = Tree::new("detect-empty");
    e.write("misc.bin", "");
    assert_eq!(detect(&e.0, 10).tag, None);

    let p = Tree::new("detect-photos");
    p.write("DCIM/1.jpg", "").write("DCIM/2.jpg", "");
    assert_eq!(detect(&p.0, 100).tag, Some(Tag::Photos));
    assert_eq!(detect(&p.0, 0).signals.sampled, 0);
}

#[test]
fn defaults_table() {
    let s = tag_defaults(Tag::Secrets);
    assert!(!s.relays && !s.agents_read && s.priority == Priority::Urgent);
    assert!(tag_defaults(Tag::Photos).whole_file_hash);
    assert_eq!(tag_defaults(Tag::Code).gitignore, GitignorePolicy::Keep);
}
