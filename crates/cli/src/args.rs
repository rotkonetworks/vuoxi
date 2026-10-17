//! Hand-rolled argument parsing: a handful of flags, no clap.

use std::path::PathBuf;

pub const USAGE: &str = "vuoxi - battery-aware file sync
usage: vuoxi [--json] [--socket PATH] [--config PATH] [--fields a,b] <command>
  status                            head, files, conflicts, peers
  changes [--since N] [--limit N]   journal tail; `next=N` continues
  conflicts [--since N] [--limit N] concurrent edits, both kept
  plan [FOLDER]                     preview the effect; changes nothing
  apply PLAN                        enact a plan
  revert PLAN                       undo an applied plan
  run                               run the sync engine for the configured folders
  daemon [--read-only]              serve the socket (fake state for now)
  mcp-serve                         MCP over stdio (not implemented yet)
  init                              create device key and config; print the id
  id                                print this device's id
  pair NAME ID ADDR...              add or replace a peer
  folder add ID PATH [--tag T] [--peer NAME]...
  folder list                       configured folders
  config path                       where the config file is
output: one line per item; --json or VUOXI_FORMAT=json for JSON
`vuoxi <command> --help` for one command";

#[derive(Debug, PartialEq, Eq)]
pub enum Cmd {
    Status,
    Changes,
    Conflicts,
    Plan(Option<String>),
    Apply(String),
    Revert(String),
    Daemon,
    Run,
    McpServe,
    Init,
    Id,
    Pair {
        name: String,
        id: String,
        addrs: Vec<String>,
    },
    FolderAdd {
        id: String,
        path: PathBuf,
    },
    FolderList,
    ConfigPath,
    Help(Option<&'static str>),
}

#[derive(Debug, Default)]
pub struct Opts {
    pub json: Option<bool>,
    pub socket: Option<PathBuf>,
    pub since: Option<u64>,
    pub limit: Option<u64>,
    pub fields: Option<Vec<String>>,
    pub read_only: bool,
    pub config: Option<PathBuf>,
    pub tag: Option<String>,
    pub peers: Vec<String>,
}

pub fn command_help(cmd: &str) -> Option<&'static str> {
    Some(match cmd {
        "status" => "vuoxi status: summary counts, journal head, power mode",
        "changes" => {
            "vuoxi changes [--since CURSOR] [--limit N]: journal entries after CURSOR (default 0),\n\
             at most N (default 50, max 200). `next=C` means more: rerun with --since C"
        }
        "conflicts" => "vuoxi conflicts [--since CURSOR] [--limit N]: unresolved concurrent edits",
        "plan" => "vuoxi plan [FOLDER]: print a plan id and its effect (files, bytes, peers)",
        "apply" => "vuoxi apply PLAN: enact a pending plan",
        "revert" => "vuoxi revert PLAN: roll back an applied plan (newest first)",
        "run" => {
            "vuoxi run [--config PATH]: sync the configured folders with their peers until stopped.\n\
             Changes come from the watcher socket in the config; without one, folders are\n\
             reconciled every VUOXI_POLL_SECS seconds (default 60)"
        }
        "daemon" => {
            "vuoxi daemon [--read-only] [--socket PATH]: serve fake state; read-only denies plan/apply/revert"
        }
        "mcp-serve" => "vuoxi mcp-serve: stdio MCP adapter (not implemented yet)",
        "init" => {
            "vuoxi init [--config PATH]: create the device key and config if absent; print the id"
        }
        "id" => "vuoxi id [--config PATH]: print this device's id (the key sits beside the config)",
        "pair" => {
            "vuoxi pair NAME ID ADDR... [--config PATH]: add or replace a peer; ADDR is host:port, e.g. [2001:db8::1]:41641"
        }
        "folder" => {
            "vuoxi folder add ID PATH [--tag T] [--peer NAME]... | vuoxi folder list  [--config PATH]\n\
             ID is [a-z0-9-]{1,64}; PATH is made absolute; folders may not nest"
        }
        "config" => "vuoxi config path [--config PATH]: print the config file location",
        _ => return None,
    })
}

fn num(flag: &str, v: Option<String>) -> Result<u64, String> {
    let v = v.ok_or(format!("{flag} needs a value"))?;
    v.parse().map_err(|_| format!("{flag}: not a number: {v}"))
}

pub fn parse<I: IntoIterator<Item = String>>(args: I) -> Result<(Cmd, Opts), String> {
    let mut o = Opts::default();
    let mut pos: Vec<String> = Vec::new();
    let mut help = false;
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        let (flag, inline) = match a.split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f.to_string(), Some(v.to_string())),
            _ => (a.clone(), None),
        };
        let mut value = || inline.clone().or_else(|| it.next());
        match flag.as_str() {
            "-h" | "--help" => help = true,
            "--json" => o.json = Some(true),
            "--text" => o.json = Some(false),
            "--read-only" => o.read_only = true,
            "--config" => o.config = Some(value().ok_or("--config needs a path")?.into()),
            "--tag" => o.tag = Some(value().ok_or("--tag needs a value")?),
            "--peer" => o.peers.push(value().ok_or("--peer needs a name")?),
            "--socket" => o.socket = Some(value().ok_or("--socket needs a path")?.into()),
            "--since" => o.since = Some(num("--since", value())?),
            "--limit" => o.limit = Some(num("--limit", value())?),
            "--fields" => {
                let v = value().ok_or("--fields needs a list")?;
                o.fields = Some(
                    v.split(',')
                        .filter(|s| !s.is_empty())
                        .map(String::from)
                        .collect(),
                );
            }
            f if f.starts_with('-') && f.len() > 1 => return Err(format!("unknown flag: {f}")),
            _ => pos.push(a),
        }
    }
    let mut pos = pos.into_iter();
    let Some(name) = pos.next() else {
        return if help || o.json.is_none() && o.socket.is_none() && o.fields.is_none() {
            Ok((Cmd::Help(None), o))
        } else {
            Err("missing command".into())
        };
    };
    if help {
        return match command_help(&name) {
            Some(_) => Ok((Cmd::Help(Some(command_help(&name).unwrap())), o)),
            None => Err(format!("unknown command: {name}")),
        };
    }
    let rest: Vec<String> = pos.collect();
    let paged = matches!(name.as_str(), "changes" | "conflicts");
    if !paged && (o.since.is_some() || o.limit.is_some()) {
        return Err(format!("--since/--limit do not apply to {name}"));
    }
    if name != "daemon" && o.read_only {
        return Err("--read-only applies to daemon only".into());
    }
    let folder_add = name == "folder" && rest.first().is_some_and(|s| s == "add");
    if (o.tag.is_some() || !o.peers.is_empty()) && !folder_add {
        return Err("--tag/--peer apply to `folder add` only".into());
    }
    let local = matches!(
        name.as_str(),
        "init" | "id" | "pair" | "folder" | "config" | "run"
    );
    if o.config.is_some() && !local {
        return Err(format!("--config does not apply to {name}"));
    }
    let arity = |max: usize, min: usize, what: &str| {
        if rest.len() < min || rest.len() > max {
            Err(format!("{name}: expected {what}"))
        } else {
            Ok(())
        }
    };
    let cmd = match name.as_str() {
        "status" => {
            arity(0, 0, "no arguments")?;
            Cmd::Status
        }
        "changes" => {
            arity(0, 0, "no arguments")?;
            Cmd::Changes
        }
        "conflicts" => {
            arity(0, 0, "no arguments")?;
            Cmd::Conflicts
        }
        "plan" => {
            arity(1, 0, "[FOLDER]")?;
            Cmd::Plan(rest.first().cloned())
        }
        "apply" => {
            arity(1, 1, "PLAN")?;
            Cmd::Apply(rest[0].clone())
        }
        "revert" => {
            arity(1, 1, "PLAN")?;
            Cmd::Revert(rest[0].clone())
        }
        "daemon" => {
            arity(0, 0, "no arguments")?;
            Cmd::Daemon
        }
        "mcp-serve" => {
            arity(0, 0, "no arguments")?;
            Cmd::McpServe
        }
        "run" => {
            arity(0, 0, "no arguments")?;
            Cmd::Run
        }
        "init" => {
            arity(0, 0, "no arguments")?;
            Cmd::Init
        }
        "id" => {
            arity(0, 0, "no arguments")?;
            Cmd::Id
        }
        "pair" => {
            arity(usize::MAX, 3, "NAME ID ADDR...")?;
            Cmd::Pair {
                name: rest[0].clone(),
                id: rest[1].clone(),
                addrs: rest[2..].to_vec(),
            }
        }
        "folder" => match rest.first().map(String::as_str) {
            Some("add") => {
                arity(3, 3, "add ID PATH")?;
                Cmd::FolderAdd {
                    id: rest[1].clone(),
                    path: rest[2].clone().into(),
                }
            }
            Some("list") => {
                arity(1, 1, "list")?;
                Cmd::FolderList
            }
            _ => return Err("folder: expected `add ID PATH` or `list`".into()),
        },
        "config" => match rest.first().map(String::as_str) {
            Some("path") => {
                arity(1, 1, "path")?;
                Cmd::ConfigPath
            }
            _ => return Err("config: expected `path`".into()),
        },
        _ => return Err(format!("unknown command: {name}")),
    };
    Ok((cmd, o))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Result<(Cmd, Opts), String> {
        parse(s.split_whitespace().map(String::from))
    }

    #[test]
    fn flags_anywhere_and_both_spellings() {
        let (c, o) = p("--json changes --since 5 --limit=9 --fields seq,op").unwrap();
        assert_eq!(c, Cmd::Changes);
        assert_eq!((o.since, o.limit, o.json), (Some(5), Some(9), Some(true)));
        assert_eq!(o.fields.unwrap(), ["seq", "op"]);
    }

    #[test]
    fn positionals_and_errors() {
        assert_eq!(p("apply plan-1").unwrap().0, Cmd::Apply("plan-1".into()));
        assert_eq!(p("plan").unwrap().0, Cmd::Plan(None));
        assert!(p("apply").is_err());
        assert!(p("status --since 3").is_err());
        assert!(p("changes --limit x").is_err());
        assert!(p("changes --bogus").is_err());
        assert!(p("nope").is_err());
        assert!(p("status --read-only").is_err());
    }

    #[test]
    fn config_commands() {
        assert_eq!(
            p("init --config x.toml").unwrap().1.config.unwrap(),
            PathBuf::from("x.toml")
        );
        let (c, o) = p("folder add docs /d --tag t --peer a --peer=b").unwrap();
        assert!(matches!(c, Cmd::FolderAdd { .. }));
        assert_eq!((o.tag.as_deref(), o.peers.len()), (Some("t"), 2));
        assert!(
            matches!(p("pair n id [::1]:1 1.2.3.4:5").unwrap().0, Cmd::Pair { addrs, .. } if addrs.len() == 2)
        );
        assert!(p("pair n id").is_err());
        assert!(p("folder").is_err());
        assert!(p("folder add docs").is_err());
        assert!(p("folder list --tag x").is_err());
        assert!(p("status --peer a").is_err());
        assert!(p("status --config x").is_err());
        assert_eq!(p("config path").unwrap().0, Cmd::ConfigPath);
        assert!(p("config").is_err());
    }

    #[test]
    fn help_forms() {
        assert_eq!(p("").unwrap().0, Cmd::Help(None));
        assert_eq!(p("--help").unwrap().0, Cmd::Help(None));
        assert!(matches!(p("plan --help").unwrap().0, Cmd::Help(Some(_))));
        assert!(p("nope --help").is_err());
    }
}
