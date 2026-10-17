use biscuit_auth::builder::BlockBuilder;
use vuoxi_caps::token::*;
use vuoxi_caps::*;

const NOW: u64 = 1_000;

fn seeder() -> Signer {
    Signer::from_seed([1; 32])
}
fn peer(n: u8) -> PeerId {
    Signer::from_seed([n; 32]).peer_id()
}
fn sc(s: &str) -> Scope {
    Scope::new(s).unwrap()
}
fn root(grants: Vec<(Right, Scope)>) -> Mint {
    Mint {
        folder: "f1".into(),
        grants,
        expires: None,
        holder: None,
        relay: None,
        no_reshare: false,
    }
}
fn ask(tok: &Biscuit, who: PeerId, verb: Right, folder: &str, prefix: &str) -> Result<(), Denied> {
    no_malformed(authorize(
        tok,
        &Request {
            principal: who,
            verb,
            folder,
            prefix: &sc(prefix),
            now: NOW,
            relay: None,
        },
    ))
}
/// A denial caused by broken datalog would make every negative test pass vacuously.
fn no_malformed(r: Result<(), Denied>) -> Result<(), Denied> {
    if let Err(Denied(m)) = &r {
        assert!(
            !m.contains("parsing error") && !m.contains("generating Datalog"),
            "{m}"
        );
    }
    r
}

#[test]
fn root_grants_within_prefix_only() {
    let t = mint(&seeder(), &root(vec![(Right::Read, sc("photos/"))])).unwrap();
    assert!(ask(&t, peer(9), Right::Read, "f1", "photos/2026/").is_ok());
    assert!(ask(&t, peer(9), Right::Read, "f1", "photos/").is_ok());
    assert!(ask(&t, peer(9), Right::Read, "f1", "").is_err());
    assert!(ask(&t, peer(9), Right::Read, "f1", "photos-private/").is_err());
    assert!(ask(&t, peer(9), Right::Write, "f1", "photos/").is_err());
    assert!(ask(&t, peer(9), Right::Read, "f2", "photos/").is_err());
}

#[test]
fn scope_rejects_traversal_and_quotes() {
    for bad in ["a", "/a/", "a//b/", "../", "a/../b/", "a\"/", "a\\/"] {
        assert!(Scope::new(bad).is_err(), "{bad}");
    }
    assert!(Scope::new("").is_ok() && Scope::new("a/b/").is_ok());
}

#[test]
fn expiry_and_holder_binding() {
    let mut m = root(vec![(Right::Read, Scope::root())]);
    m.expires = Some(NOW + 1);
    m.holder = Some(peer(5));
    let t = mint(&seeder(), &m).unwrap();
    assert!(ask(&t, peer(5), Right::Read, "f1", "").is_ok());
    assert!(ask(&t, peer(6), Right::Read, "f1", "").is_err());
    let late = authorize(
        &t,
        &Request {
            principal: peer(5),
            verb: Right::Read,
            folder: "f1",
            prefix: &sc(""),
            now: NOW + 1,
            relay: None,
        },
    );
    assert!(late.is_err());
}

#[test]
fn holder_can_narrow() {
    let t = mint(
        &seeder(),
        &root(vec![(Right::Read, sc("a/")), (Right::Write, sc("a/"))]),
    )
    .unwrap();
    let n = attenuate(
        &t,
        &Narrow {
            prefix: Some(sc("a/b/")),
            verbs: Some(vec![Right::Read]),
            expires: Some(NOW + 10),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(ask(&n, peer(9), Right::Read, "f1", "a/b/c/").is_ok());
    assert!(ask(&n, peer(9), Right::Read, "f1", "a/x/").is_err());
    assert!(ask(&n, peer(9), Right::Write, "f1", "a/b/").is_err());
    // narrowing again, and re-binding to a specific holder
    let n2 = attenuate(
        &n,
        &Narrow {
            holder: Some(peer(7)),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(ask(&n2, peer(7), Right::Read, "f1", "a/b/").is_ok());
    assert!(ask(&n2, peer(9), Right::Read, "f1", "a/b/").is_err());
}

#[test]
fn holder_cannot_widen() {
    let t = mint(&seeder(), &root(vec![(Right::Read, sc("a/b/"))])).unwrap();
    // a "wider" narrowing is just a no-op on top of the original limits
    let n = attenuate(
        &t,
        &Narrow {
            prefix: Some(sc("a/")),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(ask(&n, peer(9), Right::Read, "f1", "a/").is_err());
    assert!(ask(&n, peer(9), Right::Read, "f1", "a/b/").is_ok());
    // a hand-rolled block that claims new rights is ignored by the authorizer
    let forged = t
        .append(
            BlockBuilder::new()
                .code(r#"grant("write", ""); grant("read", ""); folder("f2");"#)
                .unwrap(),
        )
        .unwrap();
    assert!(ask(&forged, peer(9), Right::Write, "f1", "").is_err());
    assert!(ask(&forged, peer(9), Right::Read, "f1", "").is_err());
    assert!(ask(&forged, peer(9), Right::Read, "f2", "a/b/").is_err());
    assert!(ask(&forged, peer(9), Right::Read, "f1", "a/b/").is_ok());
    // an expired token cannot be revived by a later block either
    let mut m = root(vec![(Right::Read, Scope::root())]);
    m.expires = Some(NOW);
    let old = mint(&seeder(), &m).unwrap();
    let revived = attenuate(
        &old,
        &Narrow {
            expires: Some(NOW + 999),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(ask(&revived, peer(9), Right::Read, "f1", "").is_err());
}

#[test]
fn agent_token_is_one_folder_read_only() {
    let agent = peer(42);
    let t = mint(
        &seeder(),
        &Mint {
            folder: "notes".into(),
            holder: Some(agent),
            ..root(vec![
                (Right::Read, Scope::root()),
                (Right::Write, Scope::root()),
            ])
        },
    )
    .unwrap();
    // the owner narrows it to read-only before handing it to the agent
    let t = attenuate(
        &t,
        &Narrow {
            verbs: Some(vec![Right::Read]),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(ask(&t, agent, Right::Read, "notes", "x/").is_ok());
    assert!(ask(&t, agent, Right::Write, "notes", "x/").is_err());
    assert!(ask(&t, agent, Right::Read, "photos", "").is_err());
    assert!(ask(&t, peer(43), Right::Read, "notes", "").is_err());
}

#[test]
fn no_reshare_is_valid_only_as_minted() {
    let mut m = root(vec![(Right::Read, Scope::root())]);
    m.no_reshare = true;
    let t = mint(&seeder(), &m).unwrap();
    assert!(ask(&t, peer(9), Right::Read, "f1", "").is_ok());
    let n = attenuate(
        &t,
        &Narrow {
            prefix: Some(sc("a/")),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(ask(&n, peer(9), Right::Read, "f1", "a/").is_err());
}

#[test]
fn relay_retention_limits() {
    let mut m = root(vec![(Right::Relay, Scope::root())]);
    assert!(matches!(mint(&seeder(), &m), Err(Error::MissingRelayLimit)));
    m.relay = Some(RelayLimit {
        max_bytes: 50_000,
        max_days: 7,
    });
    let t = mint(&seeder(), &m).unwrap();
    let want = |t: &Biscuit, bytes, days| {
        authorize(
            t,
            &Request {
                principal: peer(9),
                verb: Right::Relay,
                folder: "f1",
                prefix: &sc(""),
                now: NOW,
                relay: Some(RelayUse { bytes, days }),
            },
        )
    };
    assert!(want(&t, 50_000, 7).is_ok());
    assert!(want(&t, 50_001, 7).is_err());
    assert!(want(&t, 1, 8).is_err());
    let n = attenuate(
        &t,
        &Narrow {
            relay: Some(RelayLimit {
                max_bytes: 100,
                max_days: 1,
            }),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(want(&n, 100, 1).is_ok());
    assert!(want(&n, 101, 1).is_err());
    assert!(want(&n, 100, 2).is_err());
}

#[test]
fn serialization_verifies_against_seeder_key() {
    let t = mint(&seeder(), &root(vec![(Right::Read, Scope::root())])).unwrap();
    let bytes = t.to_vec().unwrap();
    let back = parse(&bytes, &seeder().peer_id()).unwrap();
    assert!(ask(&back, peer(9), Right::Read, "f1", "").is_ok());
    assert!(parse(&bytes, &peer(2)).is_err());
}
