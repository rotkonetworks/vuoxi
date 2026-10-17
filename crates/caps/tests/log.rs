use vuoxi_caps::log::*;
use vuoxi_caps::*;

const F: &str = "f1";

fn key(n: u8) -> Signer {
    Signer::from_seed([n; 32])
}
fn sc(s: &str) -> Scope {
    Scope::new(s).unwrap()
}
fn intro(by: &Signer, who: &Signer, scope: &str) -> Entry {
    Entry::sign(
        by,
        F,
        Body::Introduce {
            member: who.peer_id(),
            scope: sc(scope),
        },
    )
    .unwrap()
}
fn grant(by: &Signer, who: &Signer, scope: &str, rights: &[Right]) -> Entry {
    Entry::sign(
        by,
        F,
        Body::Grant {
            member: who.peer_id(),
            scope: sc(scope),
            rights: Rights::of(rights),
        },
    )
    .unwrap()
}
fn revoke(by: &Signer, who: &Signer) -> Entry {
    Entry::sign(
        by,
        F,
        Body::Revoke {
            member: who.peer_id(),
        },
    )
    .unwrap()
}
fn reasons(m: &Membership) -> Vec<(usize, Reason)> {
    m.rejected.iter().map(|r| (r.index, r.reason)).collect()
}

#[test]
fn seeder_introduces_anyone_and_strangers_cannot() {
    let (s, a, b, mallory) = (key(1), key(2), key(3), key(66));
    let log = [intro(&s, &a, ""), intro(&mallory, &b, "")];
    let m = verify_log(F, &s.peer_id(), &log);
    assert!(m.is_member(&a.peer_id()));
    assert!(!m.is_member(&b.peer_id()));
    assert_eq!(reasons(&m), [(1, Reason::AuthorNotMember)]);
}

#[test]
fn delegated_introducer_within_scope_only() {
    let (s, i, x, y) = (key(1), key(2), key(3), key(4));
    let log = [
        intro(&s, &i, ""),
        grant(&s, &i, "photos/", &[Right::Introduce]),
        intro(&i, &x, "photos/2026/"), // inside scope: ok
        intro(&i, &y, "docs/"),        // outside scope: rejected
        intro(&i, &y, ""),             // wider than scope: rejected
    ];
    let m = verify_log(F, &s.peer_id(), &log);
    assert!(m.is_member(&x.peer_id()));
    assert!(!m.is_member(&y.peer_id()));
    assert_eq!(m.members[&x.peer_id()].introduced_by, i.peer_id());
    assert_eq!(
        reasons(&m),
        [(3, Reason::NotAuthorized), (4, Reason::NotAuthorized)]
    );
}

#[test]
fn introducer_cannot_grant_what_it_lacks() {
    let (s, i, x) = (key(1), key(2), key(3));
    let log = [
        intro(&s, &i, ""),
        grant(&s, &i, "", &[Right::Introduce, Right::Read]),
        intro(&i, &x, ""),
        grant(&i, &x, "", &[Right::Write]),     // i holds no write
        grant(&i, &x, "", &[Right::Read]),      // ok
        grant(&i, &x, "", &[Right::Introduce]), // ok: i holds introduce
    ];
    let m = verify_log(F, &s.peer_id(), &log);
    assert_eq!(reasons(&m), [(3, Reason::NotAuthorized)]);
    assert!(m.holds(&x.peer_id(), Right::Read, &sc("a/")));
    assert!(m.holds(&x.peer_id(), Right::Introduce, &sc("a/")));
    assert!(!m.holds(&x.peer_id(), Right::Write, &sc("a/")));
}

#[test]
fn grant_needs_introduction_inside_scope() {
    let (s, a) = (key(1), key(2));
    let log = [
        grant(&s, &a, "", &[Right::Read]), // not introduced yet
        intro(&s, &a, "photos/"),
        grant(&s, &a, "docs/", &[Right::Read]), // outside introduced scope
        grant(&s, &a, "photos/2026/", &[Right::Read]),
    ];
    let m = verify_log(F, &s.peer_id(), &log);
    assert_eq!(
        reasons(&m),
        [(0, Reason::NotIntroduced), (2, Reason::NotIntroduced)]
    );
    assert!(m.holds(&a.peer_id(), Right::Read, &sc("photos/2026/x/")));
    assert!(!m.holds(&a.peer_id(), Right::Read, &sc("photos/2025/")));
}

#[test]
fn revocation_takes_effect_from_its_position() {
    let (s, i, x, y, z) = (key(1), key(2), key(3), key(4), key(5));
    let log = [
        intro(&s, &i, ""),
        grant(&s, &i, "", &[Right::Introduce]),
        intro(&i, &x, ""), // before revocation: counts
        revoke(&s, &i),
        intro(&i, &y, ""), // after: ignored
        intro(&s, &z, ""),
    ];
    let m = verify_log(F, &s.peer_id(), &log);
    assert!(!m.is_member(&i.peer_id()));
    assert!(
        m.is_member(&x.peer_id()),
        "earlier work stands, revocation is not retroactive"
    );
    assert!(!m.is_member(&y.peer_id()));
    assert!(m.is_member(&z.peer_id()));
    assert_eq!(reasons(&m), [(4, Reason::AuthorNotMember)]);
}

#[test]
fn revoke_authority() {
    let (s, i, x, other) = (key(1), key(2), key(3), key(4));
    let log = [
        intro(&s, &i, ""),
        grant(&s, &i, "", &[Right::Introduce]),
        intro(&i, &x, ""),
        intro(&s, &other, ""),
        revoke(&other, &x),   // neither seeder nor x's introducer
        revoke(&x, &s),       // seeder is immutable
        revoke(&s, &key(77)), // unknown
        revoke(&i, &x),       // introducer: ok
    ];
    let m = verify_log(F, &s.peer_id(), &log);
    assert!(!m.is_member(&x.peer_id()));
    assert_eq!(
        reasons(&m),
        [
            (4, Reason::NotAuthorized),
            (5, Reason::SeederImmutable),
            (6, Reason::UnknownTarget)
        ]
    );
}

#[test]
fn tampered_signature_rejected() {
    let (s, a) = (key(1), key(2));
    let good = intro(&s, &a, "");
    let mut bytes = good.to_bytes();
    let last = bytes.len() - 1;
    bytes[last] ^= 1;
    let bad_sig = Entry::from_bytes(&bytes).unwrap();
    assert!(!bad_sig.signature_ok());

    // tampered body: retarget the introduction at someone else
    let mut bytes = good.to_bytes();
    let target = 1 + 2 + F.len() + 32 + 1; // version, folder, author, tag
    bytes[target] ^= 1;
    let retargeted = Entry::from_bytes(&bytes).unwrap();
    assert!(!retargeted.signature_ok());

    let m = verify_log(F, &s.peer_id(), &[bad_sig, retargeted, good]);
    assert_eq!(
        reasons(&m),
        [(0, Reason::BadSignature), (1, Reason::BadSignature)]
    );
    assert!(m.is_member(&a.peer_id()));
}

#[test]
fn author_cannot_be_swapped() {
    // an entry signed by Mallory, relabelled as the seeder's
    let (s, a, mallory) = (key(1), key(2), key(66));
    let e = intro(&mallory, &a, "");
    let mut bytes = e.to_bytes();
    let at = 1 + 2 + F.len();
    bytes[at..at + 32].copy_from_slice(&s.peer_id().0);
    let forged = Entry::from_bytes(&bytes).unwrap();
    let m = verify_log(F, &s.peer_id(), &[forged]);
    assert_eq!(reasons(&m), [(0, Reason::BadSignature)]);
}

#[test]
fn entries_do_not_cross_folders() {
    let (s, a) = (key(1), key(2));
    let other = Entry::sign(
        &s,
        "f2",
        Body::Introduce {
            member: a.peer_id(),
            scope: sc(""),
        },
    )
    .unwrap();
    let m = verify_log(F, &s.peer_id(), &[other]);
    assert_eq!(reasons(&m), [(0, Reason::WrongFolder)]);
}

#[test]
fn relays_need_root_introduce_and_can_be_revoked() {
    let (s, i, scoped, r) = (key(1), key(2), key(3), key(9));
    let lim = RelayLimit {
        max_bytes: 50_000_000_000,
        max_days: 7,
    };
    let add = |by: &Signer| {
        Entry::sign(
            by,
            F,
            Body::AddRelay {
                relay: r.peer_id(),
                limit: lim,
            },
        )
        .unwrap()
    };
    let log = [
        intro(&s, &i, ""),
        grant(&s, &i, "", &[Right::Introduce]),
        intro(&s, &scoped, "a/"),
        grant(&s, &scoped, "a/", &[Right::Introduce]),
        add(&scoped), // introduce only under a/: rejected
        add(&i),
        revoke(&s, &r),
    ];
    let m = verify_log(F, &s.peer_id(), &log[..6]);
    assert_eq!(m.relays[&r.peer_id()], lim);
    assert_eq!(reasons(&m), [(4, Reason::NotAuthorized)]);
    let m = verify_log(F, &s.peer_id(), &log);
    assert!(m.relays.is_empty());
}

#[test]
fn replay_is_deterministic_and_encoding_round_trips() {
    let (s, i, x) = (key(1), key(2), key(3));
    let log = vec![
        intro(&s, &i, ""),
        grant(&s, &i, "", &[Right::Introduce, Right::Read]),
        intro(&i, &x, "a/"),
        Entry::sign(
            &s,
            F,
            Body::AddRelay {
                relay: key(9).peer_id(),
                limit: RelayLimit {
                    max_bytes: 1,
                    max_days: 2,
                },
            },
        )
        .unwrap(),
        revoke(&s, &x),
        intro(&x, &i, ""),
    ];
    let decoded: Vec<Entry> = log
        .iter()
        .map(|e| Entry::from_bytes(&e.to_bytes()).unwrap())
        .collect();
    assert_eq!(decoded, log);
    let a = verify_log(F, &s.peer_id(), &log);
    let b = verify_log(F, &s.peer_id(), &decoded);
    assert_eq!(a, b);
    assert_eq!(a, verify_log(F, &s.peer_id(), &log));
    // stable bytes: same input signs to the same encoding (ed25519 is deterministic)
    assert_eq!(log[0].to_bytes(), intro(&s, &i, "").to_bytes());
}

#[test]
fn decode_is_strict() {
    let e = intro(&key(1), &key(2), "a/");
    let mut b = e.to_bytes();
    b.push(0);
    assert!(Entry::from_bytes(&b).is_err());
    let b = e.to_bytes();
    assert!(Entry::from_bytes(&b[..b.len() - 1]).is_err());
    let mut v = e.to_bytes();
    v[0] = 2;
    assert!(Entry::from_bytes(&v).is_err());
    // a non-canonical scope inside an otherwise well-formed entry is refused
    let g = grant(&key(1), &key(2), "a/", &[Right::Read]);
    let mut b = g.to_bytes();
    let scope_at = 1 + 2 + F.len() + 32 + 1 + 32 + 2;
    b[scope_at] = b'/';
    assert!(Entry::from_bytes(&b).is_err());
}

#[test]
fn log_key_is_the_token_root_key() {
    use vuoxi_caps::token::{Mint, mint, parse};
    let s = key(1);
    let t = mint(
        &s,
        &Mint {
            folder: F.into(),
            grants: vec![(Right::Read, Scope::root())],
            expires: None,
            holder: None,
            relay: None,
            no_reshare: false,
        },
    )
    .unwrap();
    assert!(parse(&t.to_vec().unwrap(), &s.peer_id()).is_ok());
}
