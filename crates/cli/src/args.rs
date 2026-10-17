//! Hand-rolled argument parsing: a handful of flags, no clap.

use std::path::PathBuf;

pub const USAGE: &str = "vuoxi - battery-aware file sync
usage: vuoxi [--json] [--socket PATH] [--fields a,b] <command>
  status                            head, files, conflicts, peers
  changes [--since N] [--limit N]   journal tail; `next=N` continues
  conflicts [--since N] [--limit N] concurrent edits, both kept
  plan [FOLDER]                     preview the effect; changes nothing
  apply PLAN                        enact a plan
  revert PLAN                       undo an applied plan
  daemon [--read-only]              serve the socket (fake state for now)
  mcp-serve                         MCP over stdio (not implemented yet)
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
    McpServe,
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
        "daemon" => {
            "vuoxi daemon [--read-only] [--socket PATH]: serve fake state; read-only denies plan/apply/revert"
        }
        "mcp-serve" => "vuoxi mcp-serve: stdio MCP adapter (not implemented yet)",
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
    fn help_forms() {
        assert_eq!(p("").unwrap().0, Cmd::Help(None));
        assert_eq!(p("--help").unwrap().0, Cmd::Help(None));
        assert!(matches!(p("plan --help").unwrap().0, Cmd::Help(Some(_))));
        assert!(p("nope --help").is_err());
    }
}
