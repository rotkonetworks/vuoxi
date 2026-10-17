//! End to end: a real `vuoxi daemon` on a socket under target/, real client calls.

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

struct Daemon {
    child: Child,
    socket: PathBuf,
}

impl Daemon {
    fn start(name: &str, extra: &[&str]) -> Self {
        let socket = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("{name}.sock"));
        let child = Command::new(env!("CARGO_BIN_EXE_vuoxi"))
            .arg("daemon")
            .args(extra)
            .arg("--socket")
            .arg(&socket)
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let d = Daemon { child, socket };
        let t = Instant::now();
        while std::os::unix::net::UnixStream::connect(&d.socket).is_err() {
            assert!(
                t.elapsed() < Duration::from_secs(10),
                "daemon did not come up"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        d
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_vuoxi"))
            .arg("--socket")
            .arg(&self.socket)
            .args(args)
            .env_remove("VUOXI_FORMAT")
            .output()
            .unwrap()
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.socket);
    }
}

fn out(o: &Output) -> String {
    String::from_utf8(o.stdout.clone()).unwrap()
}

#[test]
fn text_and_json_formats() {
    let d = Daemon::start("cli-formats", &[]);

    let o = d.run(&["status"]);
    assert!(o.status.success());
    let s = out(&o);
    assert_eq!(s.lines().count(), 1);
    assert!(
        s.starts_with("head=250 ") && s.contains("conflicts=3"),
        "{s}"
    );

    let o = d.run(&["changes", "--limit", "3"]);
    let s = out(&o);
    assert_eq!(s.lines().count(), 4, "3 items + next: {s}");
    assert!(s.lines().next().unwrap().starts_with("#1 "));
    assert_eq!(s.lines().last().unwrap(), "next=3");

    let o = d.run(&["--json", "changes", "--since", "3", "--limit", "2"]);
    let v: Value = serde_json::from_str(&out(&o)).unwrap();
    assert_eq!(
        (v["kind"].as_str(), v["schema"].as_u64()),
        (Some("changes"), Some(1))
    );
    assert_eq!(v["items"][0]["seq"], 4);
    assert_eq!(v["next_cursor"], 5);

    // env selects JSON too; --text overrides it.
    let o = Command::new(env!("CARGO_BIN_EXE_vuoxi"))
        .args(["--socket"])
        .arg(&d.socket)
        .arg("status")
        .env("VUOXI_FORMAT", "json")
        .output()
        .unwrap();
    assert!(serde_json::from_str::<Value>(&out(&o)).is_ok());
}

#[test]
fn fields_filter_both_formats() {
    let d = Daemon::start("cli-fields", &[]);
    let o = d.run(&["--json", "--fields", "seq,op", "changes", "--limit", "2"]);
    let v: Value = serde_json::from_str(&out(&o)).unwrap();
    let item = v["items"][0].as_object().unwrap();
    assert_eq!(item.len(), 2);
    assert!(item.contains_key("seq") && item.contains_key("op"));
    assert_eq!(v["kind"], "changes");

    let o = d.run(&["--fields", "seq", "changes", "--limit", "2"]);
    assert_eq!(out(&o), "seq=1\nseq=2\nnext=2\n");
}

#[test]
fn plan_apply_revert_and_errors() {
    let d = Daemon::start("cli-act", &[]);
    let s = out(&d.run(&["plan", "docs"]));
    assert!(s.starts_with("plan-1 pending files=3 "), "{s}");
    let s = out(&d.run(&["apply", "plan-1"]));
    assert!(
        s.starts_with("applied plan-1 ") && s.contains("head=253"),
        "{s}"
    );
    let s = out(&d.run(&["revert", "plan-1"]));
    assert_eq!(s, "reverted plan-1 head=250\n");

    let o = d.run(&["apply", "plan-9"]);
    assert_eq!(o.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&o.stderr).contains("not_found"));

    let o = d.run(&["--json", "plan", "nope"]);
    assert_eq!(o.status.code(), Some(3));
    let v: Value = serde_json::from_str(&out(&o)).unwrap();
    assert_eq!(v["data"]["code"], "not_found");
}

#[test]
fn read_only_daemon_denies_acts() {
    let d = Daemon::start("cli-ro", &["--read-only"]);
    assert!(d.run(&["status"]).status.success());
    let o = d.run(&["plan"]);
    assert_eq!(o.status.code(), Some(4));
    assert!(String::from_utf8_lossy(&o.stderr).contains("capability_denied"));
}

#[test]
fn unreachable_and_stub() {
    let o = Command::new(env!("CARGO_BIN_EXE_vuoxi"))
        .args(["--socket", "target/tmp/none.sock", "status"])
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(12));

    let o = Command::new(env!("CARGO_BIN_EXE_vuoxi"))
        .arg("mcp-serve")
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&o.stderr).contains("not implemented yet"));
    assert!(
        out(&o).is_empty(),
        "stdout stays clean for the stdio protocol"
    );

    let o = Command::new(env!("CARGO_BIN_EXE_vuoxi"))
        .args(["plan", "--help"])
        .output()
        .unwrap();
    assert!(out(&o).starts_with("vuoxi plan"));
}

mod local {
    use serde_json::Value;
    use std::path::PathBuf;
    use std::process::{Command, Output};

    struct Home(PathBuf);

    impl Home {
        fn new(name: &str) -> Self {
            let d = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("cfg-{name}"));
            let _ = std::fs::remove_dir_all(&d);
            Home(d)
        }

        fn run(&self, args: &[&str]) -> Output {
            Command::new(env!("CARGO_BIN_EXE_vuoxi"))
                .args(args)
                .arg("--config")
                .arg(self.0.join("config.toml"))
                .env_remove("VUOXI_FORMAT")
                .output()
                .unwrap()
        }
    }

    fn out(o: &Output) -> String {
        String::from_utf8(o.stdout.clone()).unwrap()
    }

    fn err(o: &Output) -> String {
        String::from_utf8_lossy(&o.stderr).into_owned()
    }

    #[test]
    fn init_id_pair_folder_flow() {
        let h = Home::new("flow");
        let o = h.run(&["id"]);
        assert_eq!(o.status.code(), Some(1));
        assert!(err(&o).contains("vuoxi init"));

        let o = h.run(&["init"]);
        assert!(o.status.success(), "{}", err(&o));
        let s = out(&o);
        assert!(
            s.starts_with("id=") && s.contains("key=created config=created"),
            "{s}"
        );
        let id = s
            .trim()
            .strip_prefix("id=")
            .unwrap()
            .split(' ')
            .next()
            .unwrap()
            .to_string();
        assert_eq!(id.len(), 52);

        let s = out(&h.run(&["init"]));
        assert!(s.contains("key=kept config=kept") && s.contains(&id), "{s}");
        assert_eq!(out(&h.run(&["id"])), format!("{id}\n"));
        let v: Value = serde_json::from_str(&out(&h.run(&["--json", "id"]))).unwrap();
        assert_eq!(
            v,
            serde_json::json!({"kind": "device", "schema": 1, "id": id})
        );

        let peer = "a".repeat(52);
        let s = out(&h.run(&["pair", "laptop", &peer, "[2001:db8::1]:41641"]));
        assert!(
            s.starts_with("added laptop id=aaaa") && s.contains("addrs=[2001:db8::1]:41641"),
            "{s}"
        );
        let s = out(&h.run(&[
            "pair",
            "laptop",
            &peer,
            "192.0.2.1:41641",
            "[2001:db8::2]:1",
        ]));
        assert!(s.starts_with("replaced laptop "), "{s}");
        let v: Value = serde_json::from_str(&out(&h.run(&[
            "--json",
            "pair",
            "laptop",
            &peer,
            "192.0.2.1:1",
        ])))
        .unwrap();
        assert_eq!(
            (v["kind"].as_str(), v["change"].as_str()),
            (Some("pair"), Some("replaced"))
        );

        assert_eq!(out(&h.run(&["folder", "list"])), "(none)\n");
        let o = h.run(&[
            "folder",
            "add",
            "docs",
            "/data/docs",
            "--tag",
            "work",
            "--peer",
            "laptop",
        ]);
        assert!(o.status.success(), "{}", err(&o));
        assert_eq!(
            out(&o),
            "added docs path=/data/docs tag=work peers=laptop\n"
        );
        assert_eq!(
            out(&h.run(&["folder", "list"])),
            "docs path=/data/docs tag=work peers=laptop\n"
        );
        let v: Value = serde_json::from_str(&out(&h.run(&["--json", "folder", "list"]))).unwrap();
        assert_eq!(v["items"][0]["peers"][0], "laptop");

        // Rejections leave the config as it was.
        for args in [
            vec!["folder", "add", "Bad", "/x"],
            vec!["folder", "add", "docs", "/other"],
            vec!["folder", "add", "sub", "/data/docs/sub"],
            vec!["folder", "add", "x", "/x", "--peer", "ghost"],
            vec!["pair", "n", "tooshort", "[::1]:1"],
            vec!["pair", "n", &peer, "no-port"],
        ] {
            let o = h.run(&args);
            assert_eq!(o.status.code(), Some(1), "{args:?}");
            assert!(err(&o).starts_with("vuoxi: "), "{args:?}");
        }
        assert_eq!(out(&h.run(&["folder", "list"])).lines().count(), 1);
    }

    #[test]
    fn config_path_and_relative_folder() {
        let h = Home::new("path");
        let s = out(&h.run(&["config", "path"]));
        assert_eq!(s.trim(), h.0.join("config.toml").to_str().unwrap());
        let v: Value = serde_json::from_str(&out(&h.run(&["--json", "config", "path"]))).unwrap();
        assert_eq!(v["kind"], "config_path");

        let o = h.run(&["folder", "add", "rel", "some/dir"]);
        assert!(o.status.success(), "{}", err(&o));
        assert!(out(&o).contains(" path=/"), "made absolute: {}", out(&o));
        // Folder commands never create a key.
        assert!(!h.0.join("key").exists());
    }
}
