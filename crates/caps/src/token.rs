//! Attenuable capability tokens (biscuit).
//!
//! The authority block, signed by the folder's seeder, carries the facts
//! `folder`, `grant(right, prefix)`, `relay_limit(bytes, days)` and optionally
//! `no_reshare(true)`; expiry and holder binding are authority checks. Holders
//! append blocks of *checks* only. A block can add facts, but facts outside the
//! authority block are never trusted by the authorizer, so a holder can narrow
//! and cannot widen.
//!
//! `no_reshare` is enforced at authorization time: such a token is valid only
//! as minted (one block). Biscuit cannot stop a holder appending a block, but
//! the result is refused everywhere.

use crate::{Biscuit, Denied, Error, PeerId, RelayLimit, Right, Scope, Signer, hex, public_key};
use biscuit_auth::AuthorizerLimits;
use biscuit_auth::builder::{AuthorizerBuilder, BiscuitBuilder, BlockBuilder};
use std::time::Duration;

/// What the seeder puts in a root token.
#[derive(Clone, Debug)]
pub struct Mint {
    pub folder: String,
    pub grants: Vec<(Right, Scope)>,
    /// Unix seconds; the token is refused at or after this instant.
    pub expires: Option<u64>,
    /// Bind to one principal. `None` makes a bearer token (a share link).
    pub holder: Option<PeerId>,
    /// Required if any grant is [`Right::Relay`].
    pub relay: Option<RelayLimit>,
    pub no_reshare: bool,
}

/// A narrowing. Every field only ever tightens.
#[derive(Clone, Debug, Default)]
pub struct Narrow {
    pub prefix: Option<Scope>,
    pub verbs: Option<Vec<Right>>,
    pub expires: Option<u64>,
    pub holder: Option<PeerId>,
    pub relay: Option<RelayLimit>,
}

/// What a relay is asked to retain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RelayUse {
    pub bytes: u64,
    pub days: u32,
}

/// One authorization question.
#[derive(Clone, Debug)]
pub struct Request<'a> {
    pub principal: PeerId,
    pub verb: Right,
    pub folder: &'a str,
    pub prefix: &'a Scope,
    /// Unix seconds.
    pub now: u64,
    /// Meaningful for [`Right::Relay`]; absent counts as zero.
    pub relay: Option<RelayUse>,
}

fn int(n: u64) -> Result<i64, Error> {
    i64::try_from(n).map_err(|_| Error::OutOfRange)
}

fn lit(s: &str) -> String {
    // Scope and folder ids are validated to hold no quote or backslash.
    format!("\"{s}\"")
}

fn check_folder_id(f: &str) -> Result<(), Error> {
    if f.is_empty() || f.chars().any(|c| c.is_control() || c == '"' || c == '\\') {
        return Err(Error::BadScope(f.to_owned()));
    }
    Ok(())
}

/// Mint a root token. Only the folder's seeder should hold the signing key.
pub fn mint(seeder: &Signer, m: &Mint) -> Result<Biscuit, Error> {
    check_folder_id(&m.folder)?;
    let mut code = format!("folder({});\n", lit(&m.folder));
    for (right, scope) in &m.grants {
        code += &format!("grant({}, {});\n", lit(right.as_str()), lit(scope.as_str()));
    }
    if m.grants.iter().any(|(r, _)| *r == Right::Relay) {
        let l = m.relay.ok_or(Error::MissingRelayLimit)?;
        code += &format!("relay_limit({}, {});\n", int(l.max_bytes)?, l.max_days);
    }
    if m.no_reshare {
        code += "no_reshare(true);\n";
    }
    if let Some(t) = m.expires {
        code += &format!("check if now($t), $t < {};\n", int(t)?);
    }
    if let Some(h) = &m.holder {
        code += &format!("check if principal({});\n", lit(&hex(h)));
    }
    Ok(BiscuitBuilder::new().code(code)?.build(seeder.pair())?)
}

/// Append a narrowing block. Always succeeds mathematically; widening is
/// impossible because the block only carries checks.
pub fn attenuate(token: &Biscuit, n: &Narrow) -> Result<Biscuit, Error> {
    let mut code = String::new();
    if let Some(p) = &n.prefix {
        code += &format!(
            "check if req_prefix($p), $p.starts_with({});\n",
            lit(p.as_str())
        );
    }
    if let Some(vs) = &n.verbs {
        if vs.is_empty() {
            code += "check if false;\n";
        } else {
            let alts: Vec<_> = vs
                .iter()
                .map(|v| format!("verb({})", lit(v.as_str())))
                .collect();
            code += &format!("check if {};\n", alts.join(" or "));
        }
    }
    if let Some(t) = n.expires {
        code += &format!("check if now($t), $t < {};\n", int(t)?);
    }
    if let Some(h) = &n.holder {
        code += &format!("check if principal({});\n", lit(&hex(h)));
    }
    if let Some(l) = n.relay {
        code += &format!(
            "check if relay_use($b, $d), $b <= {}, $d <= {};\n",
            int(l.max_bytes)?,
            l.max_days
        );
    }
    Ok(token.append(BlockBuilder::new().code(code)?)?)
}

/// Parse and verify a serialized token against the folder seeder's key.
pub fn parse(bytes: &[u8], seeder: &PeerId) -> Result<Biscuit, Error> {
    Ok(Biscuit::from(bytes, public_key(seeder)?)?)
}

/// Decide whether `token` allows `req`. Pure: no clock, no I/O.
pub fn authorize(token: &Biscuit, req: &Request<'_>) -> Result<(), Denied> {
    let u = req.relay.unwrap_or(RelayUse { bytes: 0, days: 0 });
    let code = format!(
        r#"principal({p});
verb({v});
req_folder({f});
req_prefix({x});
now({now});
relay_use({rb}, {rd});
blocks({blocks});
deny if no_reshare(true), blocks($n), $n > 1;
allow if folder($f), req_folder($f), verb($v), $v != "relay",
  req_prefix($p), grant($v, $g), $p.starts_with($g);
allow if folder($f), req_folder($f), verb("relay"),
  req_prefix($p), grant("relay", $g), $p.starts_with($g),
  relay_limit($mb, $md), relay_use($b, $d), $b <= $mb, $d <= $md;
"#,
        p = lit(&hex(&req.principal)),
        v = lit(req.verb.as_str()),
        f = lit(req.folder),
        x = lit(req.prefix.as_str()),
        now = int(req.now).map_err(|_| Denied("time out of range".into()))?,
        rb = int(u.bytes).map_err(|_| Denied("bytes out of range".into()))?,
        rd = u.days,
        blocks = token.block_count(),
    );
    if req
        .folder
        .chars()
        .any(|c| c == '"' || c == '\\' || c.is_control())
    {
        return Err(Denied("bad folder id".into()));
    }
    // biscuit's default 1 ms budget denies valid tokens on a busy or cold CPU;
    // the fact and iteration caps still bound the work.
    let limits = AuthorizerLimits {
        max_time: Duration::from_millis(50),
        ..AuthorizerLimits::default()
    };
    let mut a = AuthorizerBuilder::new()
        .set_limits(limits)
        .code(code)
        .and_then(|b| b.build(token))
        .map_err(|e| Denied(e.to_string()))?;
    a.authorize().map(|_| ()).map_err(|e| Denied(e.to_string()))
}
