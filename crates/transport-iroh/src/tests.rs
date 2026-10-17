use super::*;
use vuoxi_core::{ChunkId, Manifest, PathId, VersionVector};

#[derive(Clone)]
struct Echoer;

impl Service<(PeerId, Wire)> for Echoer {
    type Response = Option<Wire>;
    type Error = Error;
    type Future = std::future::Ready<Result<Option<Wire>, Error>>;
    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Error>> {
        Poll::Ready(Ok(()))
    }
    fn call(&mut self, (_, w): (PeerId, Wire)) -> Self::Future {
        std::future::ready(Ok(match w {
            Wire::Offer(_) => None,
            Wire::Want(c) => Some(Wire::Chunk(c, vec![c.0[0]; 100_000])),
            Wire::Chunk(..) => None,
        }))
    }
}

async fn node(seed: u8, allow: &Allowlist) -> IrohTransport {
    let mut cfg = Config::new([seed; 32], allow.clone());
    cfg.bind = vec!["127.0.0.1:0".parse().unwrap()];
    IrohTransport::bind(cfg).await.unwrap()
}

fn id(seed: u8) -> PeerId {
    PeerId(*SecretKey::from_bytes(&[seed; 32]).public().as_bytes())
}

#[tokio::test]
async fn peer_id_derives_from_key() {
    let t = node(1, &Allowlist::default()).await;
    assert_eq!(t.peer_id(), id(1));
    t.close().await;
}

#[tokio::test]
async fn loopback_offer_want_chunk() {
    let allow = Allowlist::new([id(1), id(2)]);
    let (a, b) = (node(1, &allow).await, node(2, &allow).await);
    let _serve = b.serve(Echoer);
    a.add_peer(b.peer_id(), b.local_addrs());

    let mut version = VersionVector::new();
    version.bump(a.peer_id());
    let offer = Wire::Offer(Manifest {
        path: PathId(5),
        version,
        chunks: vec![ChunkId([3; 32])],
    });
    let mut a2 = a.clone();
    assert_eq!(a2.call((b.peer_id(), offer)).await.unwrap(), None);

    let want = Wire::Want(ChunkId([3; 32]));
    let got = a2.call((b.peer_id(), want)).await.unwrap();
    assert_eq!(got, Some(Wire::Chunk(ChunkId([3; 32]), vec![3; 100_000])));

    a.close().await;
    b.close().await;
}

#[tokio::test]
async fn stranger_is_refused() {
    let server_allow = Allowlist::new([id(1)]);
    let b = node(2, &server_allow).await;
    let _serve = b.serve(Echoer);

    // c is not on b's list, though c itself is willing to dial b.
    let c = node(3, &Allowlist::new([id(2)])).await;
    c.add_peer(b.peer_id(), b.local_addrs());
    let mut c2 = c.clone();
    let r = tokio::time::timeout(
        Duration::from_secs(10),
        c2.call((b.peer_id(), Wire::Want(ChunkId([0; 32])))),
    )
    .await
    .expect("refusal should be prompt");
    assert!(r.is_err());

    // and a caller refuses to dial someone it never allowed
    let mut c3 = c.clone();
    let r = c3.call((id(9), Wire::Want(ChunkId([0; 32])))).await;
    assert!(matches!(r, Err(Error::Transport(_))));
    c.close().await;
    b.close().await;
}

#[tokio::test]
async fn idle_connections_are_closed() {
    let allow = Allowlist::new([id(1), id(2)]);
    let mut cfg = Config::new([1; 32], allow.clone());
    cfg.bind = vec!["127.0.0.1:0".parse().unwrap()];
    cfg.idle_close = Some(Duration::from_millis(200));
    let a = IrohTransport::bind(cfg).await.unwrap();
    let b = node(2, &allow).await;
    let _serve = b.serve(Echoer);
    a.add_peer(b.peer_id(), b.local_addrs());
    let mut a2 = a.clone();
    a2.call((b.peer_id(), Wire::Want(ChunkId([1; 32]))))
        .await
        .unwrap();
    let conn = a.connection(b.peer_id(), false).await.unwrap();
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert!(conn.close_reason().is_some());
    // next request transparently redials
    a2.call((b.peer_id(), Wire::Want(ChunkId([1; 32]))))
        .await
        .unwrap();
    a.close().await;
    b.close().await;
}
