use crate::act::{Action, Applied, Plan, Reverted};
use crate::error::ApiError;
use crate::message::{Envelope, Response};
use crate::page::Page;
use crate::read::{ChangePage, Conflict, Peer, Status};
use std::fmt::{self, Write};

/// Compact text form of a value: the CLI default, short for humans and for
/// model context. It renders the same typed value that `--json` (or
/// `VUOXI_FORMAT=json`) serializes and carries nothing the JSON lacks, though it
/// may abbreviate (hex ids are shortened, schema and kind are implicit).
///
/// A scalar response is one line. A list is one header line followed by one
/// line per item, so `head`, `grep` and `wc -l` work on it.
pub trait Render {
    fn render(&self, out: &mut dyn Write) -> fmt::Result;

    fn to_text(&self) -> String {
        let mut s = String::new();
        let _ = self.render(&mut s);
        s
    }
}

/// First 8 hex chars, enough to tell ids apart in a listing.
fn short(hex: &str) -> &str {
    hex.get(..8).unwrap_or(hex)
}

fn next(out: &mut dyn Write, cursor: Option<u64>) -> fmt::Result {
    match cursor {
        Some(c) => write!(out, " next={c}"),
        None => Ok(()),
    }
}

impl Render for Status {
    fn render(&self, out: &mut dyn Write) -> fmt::Result {
        write!(
            out,
            "status {} files={} bytes={} conflicts={} root={} head={} mode={} peers={}/{}",
            self.folder.0,
            self.files,
            self.bytes,
            self.conflicts,
            short(&self.root),
            self.head,
            self.mode.as_str(),
            self.peers_online,
            self.peers_total
        )
    }
}

impl Render for ChangePage {
    fn render(&self, out: &mut dyn Write) -> fmt::Result {
        write!(out, "changes n={} head={}", self.items.len(), self.head)?;
        next(out, self.next_cursor)?;
        for c in &self.items {
            write!(out, "\n{} {} {}", c.seq, c.path, c.op.as_str())?;
            if let Some(by) = &c.by {
                write!(out, " by={by}")?;
            }
        }
        Ok(())
    }
}

impl Render for Page<Conflict> {
    fn render(&self, out: &mut dyn Write) -> fmt::Result {
        write!(out, "conflicts n={}", self.items.len())?;
        if let Some(t) = self.total {
            write!(out, " total={t}")?;
        }
        next(out, self.next_cursor)?;
        for c in &self.items {
            write!(
                out,
                "\n#{} path={} peer={} ours={}B theirs={}B",
                c.id,
                c.path,
                short(&c.peer),
                c.ours.bytes,
                c.theirs.bytes
            )?;
        }
        Ok(())
    }
}

impl Render for Page<Peer> {
    fn render(&self, out: &mut dyn Write) -> fmt::Result {
        write!(out, "peers n={}", self.items.len())?;
        if let Some(t) = self.total {
            write!(out, " total={t}")?;
        }
        next(out, self.next_cursor)?;
        for p in &self.items {
            write!(
                out,
                "\n{} {} {} seen={} lag={}",
                short(&p.id),
                match p.role {
                    crate::PeerRole::Device => "device",
                    crate::PeerRole::Relay => "relay",
                },
                p.state.as_str(),
                p.seen,
                p.lag
            )?;
        }
        Ok(())
    }
}

impl Render for Action {
    fn render(&self, out: &mut dyn Write) -> fmt::Result {
        match self {
            Action::Sync { peer: None } => write!(out, "sync"),
            Action::Sync { peer: Some(p) } => write!(out, "sync peer={}", short(p)),
            Action::Resolve { conflict, keep } => {
                let k = match keep {
                    crate::Keep::Ours => "ours",
                    crate::Keep::Theirs => "theirs",
                };
                write!(out, "resolve #{conflict} keep={k}")
            }
        }
    }
}

impl Render for Plan {
    fn render(&self, out: &mut dyn Write) -> fmt::Result {
        write!(out, "plan {} {} ", self.id.0, self.folder.0)?;
        self.action.render(out)?;
        write!(
            out,
            " files={} bytes={} peers={} state={} base={}",
            self.effect.files,
            self.effect.bytes,
            self.effect.peers,
            self.state.as_str(),
            short(&self.base_root)
        )
    }
}

impl Render for Applied {
    fn render(&self, out: &mut dyn Write) -> fmt::Result {
        write!(
            out,
            "applied {} files={} bytes={} peers={} root={}",
            self.plan.0,
            self.effect.files,
            self.effect.bytes,
            self.effect.peers,
            short(&self.root)
        )
    }
}

impl Render for Reverted {
    fn render(&self, out: &mut dyn Write) -> fmt::Result {
        write!(out, "reverted {} root={}", self.plan.0, short(&self.root))
    }
}

impl Render for ApiError {
    fn render(&self, out: &mut dyn Write) -> fmt::Result {
        write!(out, "error {}: {}", self.code.as_str(), self.message)?;
        if let Some(s) = self.retry_after_secs {
            write!(out, " retry={s}s")?;
        }
        Ok(())
    }
}

impl Render for Response {
    fn render(&self, out: &mut dyn Write) -> fmt::Result {
        match self {
            Response::Status(v) => v.render(out),
            Response::Changes(v) => v.render(out),
            Response::Conflicts(v) => v.render(out),
            Response::Peers(v) => v.render(out),
            Response::Plan(v) => v.render(out),
            Response::Applied(v) => v.render(out),
            Response::Reverted(v) => v.render(out),
            Response::Error(v) => v.render(out),
        }
    }
}

impl<T: Render> Render for Envelope<T> {
    fn render(&self, out: &mut dyn Write) -> fmt::Result {
        self.body.render(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::tests::responses;

    #[test]
    fn every_response_renders_and_scalars_are_one_line() {
        for r in responses() {
            let t = r.to_text();
            assert!(t.starts_with(r.kind()), "{t}");
            if matches!(
                r,
                Response::Status(_)
                    | Response::Plan(_)
                    | Response::Applied(_)
                    | Response::Reverted(_)
                    | Response::Error(_)
            ) {
                assert!(!t.contains('\n'), "{t}");
            }
        }
    }

    #[test]
    fn exact_lines() {
        let rs = responses();
        assert_eq!(
            rs[0].to_text(),
            "status docs files=1 bytes=5 conflicts=0 root=abababab head=3 mode=live peers=1/1"
        );
        assert_eq!(rs[1].to_text(), "changes n=1 head=9 next=4\n4 1 created");
        assert_eq!(rs[7].to_text(), "error not_found: no such folder");
    }
}
