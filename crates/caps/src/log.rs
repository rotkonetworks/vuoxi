//! The per-folder membership log.
//!
//! Entries are signed by their author and synced like data. [`verify_log`]
//! replays them in log order; the seeder may do anything, everyone else only
//! what a replayed prefix of the log lets them do. Entries that fail are
//! skipped, never trusted, and listed in [`Membership::rejected`].
//!
//! Ordering is the caller's job: this crate trusts the position of an entry in
//! the slice (the journal cursor), which is what makes a revocation take
//! effect "from its position". An entry is bound to its folder and author by
//! the signature, so it cannot be replayed into another folder.
//!
//! Revocation is not transitive: members the revoked party introduced stay
//! members until someone with authority revokes them.

use crate::{Error, PeerId, RelayLimit, Right, Rights, Scope, Signer};
use std::collections::{BTreeMap, BTreeSet};

const DOMAIN: &[u8] = b"vuoxi/membership/v1\0";
const VERSION: u8 = 1;
const SIG_LEN: usize = 64;

/// What an entry does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Body {
    /// Give an already-introduced member rights over `scope`.
    Grant {
        member: PeerId,
        scope: Scope,
        rights: Rights,
    },
    /// Remove a member (or relay) from this position on.
    Revoke { member: PeerId },
    /// Name a relay for the folder, with the retention it will provide.
    AddRelay { relay: PeerId, limit: RelayLimit },
    /// Admit a member; `scope` bounds what they can later be granted.
    Introduce { member: PeerId, scope: Scope },
}

/// A signed log entry. Build with [`Entry::sign`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub folder: String,
    pub author: PeerId,
    pub body: Body,
    sig: [u8; SIG_LEN],
}

fn put_scope(out: &mut Vec<u8>, s: &Scope) {
    out.extend_from_slice(&(s.as_str().len() as u16).to_be_bytes());
    out.extend_from_slice(s.as_str().as_bytes());
}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], Error> {
        if self.0.len() < n {
            return Err(Error::Decode("truncated"));
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Ok(a)
    }
    fn u8(&mut self) -> Result<u8, Error> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16, Error> {
        Ok(u16::from_be_bytes(self.take(2)?.try_into().unwrap()))
    }
    fn peer(&mut self) -> Result<PeerId, Error> {
        Ok(PeerId(self.take(32)?.try_into().unwrap()))
    }
    fn string(&mut self) -> Result<String, Error> {
        let n = self.u16()? as usize;
        String::from_utf8(self.take(n)?.to_vec()).map_err(|_| Error::Decode("utf-8"))
    }
    fn scope(&mut self) -> Result<Scope, Error> {
        Scope::new(&self.string()?)
    }
}

impl Entry {
    /// The signed part: version, folder, author, then the tagged payload.
    fn signed_bytes(&self) -> Vec<u8> {
        let mut o = vec![VERSION];
        o.extend_from_slice(&(self.folder.len() as u16).to_be_bytes());
        o.extend_from_slice(self.folder.as_bytes());
        o.extend_from_slice(&self.author.0);
        match &self.body {
            Body::Grant {
                member,
                scope,
                rights,
            } => {
                o.push(1);
                o.extend_from_slice(&member.0);
                put_scope(&mut o, scope);
                o.push(rights.bits());
            }
            Body::Revoke { member } => {
                o.push(2);
                o.extend_from_slice(&member.0);
            }
            Body::AddRelay { relay, limit } => {
                o.push(3);
                o.extend_from_slice(&relay.0);
                o.extend_from_slice(&limit.max_bytes.to_be_bytes());
                o.extend_from_slice(&limit.max_days.to_be_bytes());
            }
            Body::Introduce { member, scope } => {
                o.push(4);
                o.extend_from_slice(&member.0);
                put_scope(&mut o, scope);
            }
        }
        o
    }

    pub fn sign(author: &Signer, folder: &str, body: Body) -> Result<Entry, Error> {
        if folder.len() > u16::MAX as usize {
            return Err(Error::BadScope(folder.to_owned()));
        }
        let mut e = Entry {
            folder: folder.to_owned(),
            author: author.peer_id(),
            body,
            sig: [0; SIG_LEN],
        };
        let mut msg = DOMAIN.to_vec();
        msg.extend(e.signed_bytes());
        let sig = author
            .pair()
            .sign(&msg)
            .map_err(|e| Error::Token(e.to_string()))?;
        e.sig = sig
            .to_bytes()
            .try_into()
            .map_err(|_| Error::Decode("signature length"))?;
        Ok(e)
    }

    /// True if the signature matches author, folder and body.
    pub fn signature_ok(&self) -> bool {
        use ed25519_dalek::Verifier;
        let Ok(pk) = ed25519_dalek::VerifyingKey::from_bytes(&self.author.0) else {
            return false;
        };
        let mut msg = DOMAIN.to_vec();
        msg.extend(self.signed_bytes());
        pk.verify(&msg, &ed25519_dalek::Signature::from_bytes(&self.sig))
            .is_ok()
    }

    /// Stable encoding: signed part followed by the 64-byte signature.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut o = self.signed_bytes();
        o.extend_from_slice(&self.sig);
        o
    }

    /// Strict decode: unknown version, tag, rights bits, or trailing bytes fail.
    pub fn from_bytes(bytes: &[u8]) -> Result<Entry, Error> {
        let mut r = Reader(bytes);
        if r.u8()? != VERSION {
            return Err(Error::Decode("version"));
        }
        let folder = r.string()?;
        let author = r.peer()?;
        let body = match r.u8()? {
            1 => {
                let member = r.peer()?;
                let scope = r.scope()?;
                let rights = Rights::from_bits(r.u8()?).ok_or(Error::Decode("rights"))?;
                Body::Grant {
                    member,
                    scope,
                    rights,
                }
            }
            2 => Body::Revoke { member: r.peer()? },
            3 => {
                let relay = r.peer()?;
                let max_bytes = u64::from_be_bytes(r.take(8)?.try_into().unwrap());
                let max_days = u32::from_be_bytes(r.take(4)?.try_into().unwrap());
                Body::AddRelay {
                    relay,
                    limit: RelayLimit {
                        max_bytes,
                        max_days,
                    },
                }
            }
            4 => Body::Introduce {
                member: r.peer()?,
                scope: r.scope()?,
            },
            _ => return Err(Error::Decode("entry tag")),
        };
        let sig: [u8; SIG_LEN] = r.take(SIG_LEN)?.try_into().unwrap();
        if !r.0.is_empty() {
            return Err(Error::Decode("trailing bytes"));
        }
        Ok(Entry {
            folder,
            author,
            body,
            sig,
        })
    }
}

/// Why an entry was not applied.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    WrongFolder,
    BadSignature,
    /// The author was never introduced, or had been revoked by this point.
    AuthorNotMember,
    /// The author lacks the right, scope, or authority over the target.
    NotAuthorized,
    /// Grant to someone not introduced, or outside their introduced scope.
    NotIntroduced,
    UnknownTarget,
    /// The seeder cannot be introduced, granted to, or revoked.
    SeederImmutable,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rejection {
    pub index: usize,
    pub author: PeerId,
    pub reason: Reason,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Member {
    pub introduced_by: PeerId,
    /// Scopes the member was introduced into (ceiling for grants).
    pub scopes: BTreeSet<Scope>,
    pub grants: BTreeMap<Scope, Rights>,
}

/// The state a replayed log yields.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Membership {
    pub seeder: PeerId,
    pub members: BTreeMap<PeerId, Member>,
    pub relays: BTreeMap<PeerId, RelayLimit>,
    pub rejected: Vec<Rejection>,
}

impl Membership {
    pub fn is_member(&self, p: &PeerId) -> bool {
        *p == self.seeder || self.members.contains_key(p)
    }

    /// Does `p` hold `right` over everything under `scope`?
    pub fn holds(&self, p: &PeerId, right: Right, scope: &Scope) -> bool {
        if *p == self.seeder {
            return true;
        }
        self.members.get(p).is_some_and(|m| {
            m.grants
                .iter()
                .any(|(s, r)| s.covers(scope) && r.contains(right))
        })
    }
}

/// Replay `entries` (in log order) for `folder`, whose seeder is `seeder`.
pub fn verify_log(folder: &str, seeder: &PeerId, entries: &[Entry]) -> Membership {
    let mut m = Membership {
        seeder: *seeder,
        members: BTreeMap::new(),
        relays: BTreeMap::new(),
        rejected: Vec::new(),
    };
    for (index, e) in entries.iter().enumerate() {
        if let Err(reason) = apply(&mut m, folder, e) {
            m.rejected.push(Rejection {
                index,
                author: e.author,
                reason,
            });
        }
    }
    m
}

fn apply(m: &mut Membership, folder: &str, e: &Entry) -> Result<(), Reason> {
    use Reason::*;
    if e.folder != folder {
        return Err(WrongFolder);
    }
    if !e.signature_ok() {
        return Err(BadSignature);
    }
    let author = e.author;
    if !m.is_member(&author) {
        return Err(AuthorNotMember);
    }
    match &e.body {
        Body::Introduce { member, scope } => {
            if *member == m.seeder {
                return Err(SeederImmutable);
            }
            if !m.holds(&author, Right::Introduce, scope) {
                return Err(NotAuthorized);
            }
            m.members
                .entry(*member)
                .or_insert_with(|| Member {
                    introduced_by: author,
                    scopes: BTreeSet::new(),
                    grants: BTreeMap::new(),
                })
                .scopes
                .insert(scope.clone());
        }
        Body::Grant {
            member,
            scope,
            rights,
        } => {
            if *member == m.seeder {
                return Err(SeederImmutable);
            }
            // An introducer can hand out only what it holds itself, and only
            // within the scope it holds `introduce` for.
            let allowed = m.holds(&author, Right::Introduce, scope)
                && rights.iter().all(|r| m.holds(&author, r, scope));
            if !allowed {
                return Err(NotAuthorized);
            }
            let target = m.members.get_mut(member).ok_or(NotIntroduced)?;
            if !target.scopes.iter().any(|s| s.covers(scope)) {
                return Err(NotIntroduced);
            }
            let slot = target.grants.entry(scope.clone()).or_default();
            *slot = slot.union(*rights);
        }
        Body::Revoke { member } => {
            if *member == m.seeder {
                return Err(SeederImmutable);
            }
            let introducer = m.members.get(member).map(|t| t.introduced_by);
            if introducer.is_none() && !m.relays.contains_key(member) {
                return Err(UnknownTarget);
            }
            let ok = author == m.seeder || author == *member || introducer == Some(author);
            if !ok {
                return Err(NotAuthorized);
            }
            m.members.remove(member);
            m.relays.remove(member);
        }
        Body::AddRelay { relay, limit } => {
            if !m.holds(&author, Right::Introduce, &Scope::root()) {
                return Err(NotAuthorized);
            }
            m.relays.insert(*relay, *limit);
        }
    }
    Ok(())
}
