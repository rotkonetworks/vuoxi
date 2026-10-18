//! vuoxi: thin client. Each command is one RPC and one renderer; the daemon
//! owns every decision.

#![forbid(unsafe_code)]

mod args;
mod local;
mod render;
mod run;
mod why;

use args::{Cmd, Opts, USAGE};
use serde_json::{Value, json};
use std::path::Path;
use vuoxi_rpc::client::{self, ClientError};
use vuoxi_rpc::server;
use vuoxi_rpc::service::{AllowLayer, Layer};
use vuoxi_rpc::verbs::fake_router;

/// Exit status when no daemon answers on the socket.
const EXIT_UNREACHABLE: i32 = 12;

fn main() {
    std::process::exit(run());
}

fn want_json(o: &Opts) -> bool {
    o.json.unwrap_or_else(|| {
        std::env::var("VUOXI_FORMAT").is_ok_and(|v| v.eq_ignore_ascii_case("json"))
    })
}

fn run() -> i32 {
    let (cmd, o) = match args::parse(std::env::args().skip(1)) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("vuoxi: {e}\n{USAGE}");
            return 2;
        }
    };
    let socket = o.socket.clone().unwrap_or_else(server::default_socket);
    let (method, params) = match cmd {
        Cmd::Help(h) => {
            println!("{}", h.unwrap_or(USAGE));
            return 0;
        }
        Cmd::McpServe => {
            eprintln!("vuoxi: mcp-serve: not implemented yet");
            return 1;
        }
        Cmd::Daemon => return daemon(&socket, o.read_only),
        Cmd::Run => return run::run(&o),
        Cmd::Why(p) => return why::run(&p, &o, want_json(&o)),
        Cmd::Init
        | Cmd::Id
        | Cmd::Pair { .. }
        | Cmd::FolderAdd { .. }
        | Cmd::FolderList
        | Cmd::FolderKey(_)
        | Cmd::ConfigPath => return local::run(cmd, &o, want_json(&o)),
        Cmd::Status => ("status", None),
        Cmd::Changes => ("changes", page(&o)),
        Cmd::Conflicts => ("conflicts", page(&o)),
        Cmd::Plan(folder) => ("plan", folder.map(|f| json!({ "folder": f }))),
        Cmd::Apply(plan) => ("apply", Some(json!({ "plan": plan }))),
        Cmd::Revert(plan) => ("revert", Some(json!({ "plan": plan }))),
    };
    let json_out = want_json(&o);
    let resp = match client::call(&socket, method, params) {
        Ok(r) => r,
        Err(ClientError::Unreachable(e)) => {
            eprintln!(
                "vuoxi: cannot connect to {} ({e}); is `vuoxi daemon` running?",
                socket.display()
            );
            return EXIT_UNREACHABLE;
        }
        Err(e) => {
            eprintln!("vuoxi: {e}");
            return 1;
        }
    };
    if let Some(e) = resp.error {
        if json_out {
            println!("{}", serde_json::to_string(&e).expect("errors serialize"));
        } else {
            eprintln!("vuoxi: {}: {}", e.data.code.as_str(), e.message);
        }
        return e.data.code.exit_code();
    }
    let result = resp.result.unwrap_or(Value::Null);
    let fields = o.fields.as_deref();
    let out = if json_out {
        let v = fields.map_or(result.clone(), |f| render::filter(&result, f));
        Ok(serde_json::to_string(&v).expect("values serialize"))
    } else {
        render::text(&result, fields)
    };
    match out {
        Ok(s) => {
            println!("{s}");
            0
        }
        Err(e) => {
            eprintln!("vuoxi: {e}");
            1
        }
    }
}

fn page(o: &Opts) -> Option<Value> {
    let mut m = serde_json::Map::new();
    if let Some(s) = o.since {
        m.insert("since".into(), s.into());
    }
    if let Some(l) = o.limit {
        m.insert("limit".into(), l.into());
    }
    (!m.is_empty()).then_some(Value::Object(m))
}

/// Serve the socket over FAKE state; blocks until killed.
fn daemon(socket: &Path, read_only: bool) -> i32 {
    let listener = match server::bind(socket) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("vuoxi: bind {}: {e}", socket.display());
            return 1;
        }
    };
    eprintln!("vuoxi daemon (fake state) on {}", socket.display());
    let res = if read_only {
        server::serve(listener, AllowLayer::read_only().layer(fake_router()))
    } else {
        server::serve(listener, fake_router())
    };
    match res {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("vuoxi: {e}");
            1
        }
    }
}
