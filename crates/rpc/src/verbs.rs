//! The verbs: plain functions over state, lifted into services.
//!
//! FAKE STATE: [`FakeState`] is an in-memory stand-in so the surface can be
//! exercised end to end. The real store is built elsewhere; the verb bodies
//! change, the signatures and the wire shape do not.

use crate::service::{Router, handler};
use crate::types::*;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::json;
use std::sync::{Arc, Mutex, PoisonError};

pub type Shared = Arc<Mutex<FakeState>>;

const FOLDERS: [&str; 2] = ["docs", "photos"];
const PLAN_FILES: u64 = 3;

struct PlanRec {
    plan: Plan,
    /// Journal head before the apply; revert rolls back to it.
    seq_before: u64,
}

/// In-memory journal, conflicts and plans. FAKE: replaced by the real store.
pub struct FakeState {
    journal: Vec<Change>,
    conflicts: Vec<Conflict>,
    plans: Vec<PlanRec>,
}

impl Default for FakeState {
    fn default() -> Self {
        let ops = ["create", "modify", "delete"];
        let journal = (1..=250u64)
            .map(|seq| Change {
                seq,
                op: ops[(seq % 3) as usize].into(),
                path: format!("p{:04x}", (seq * 7919) % 4096),
                bytes: (seq * 517) % 65536,
                peer: (seq % 3 == 0).then(|| "d1".to_string()),
            })
            .collect();
        let conflicts = [(40, "p0a11"), (120, "p0b22"), (200, "p0c33")]
            .into_iter()
            .map(|(seq, path)| Conflict {
                seq,
                path: path.into(),
                ours: format!("v{}", seq / 10),
                theirs: format!("v{}", seq / 10 - 1),
                peer: "d1".into(),
            })
            .collect();
        Self {
            journal,
            conflicts,
            plans: Vec::new(),
        }
    }
}

impl FakeState {
    fn head(&self) -> u64 {
        self.journal.last().map_or(0, |c| c.seq)
    }
}

/// Journal cursor and page size, shared by every list verb.
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PageParams {
    #[serde(default)]
    since: u64,
    limit: Option<u64>,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NoParams {}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanParams {
    folder: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanRef {
    plan: String,
}

fn bad(detail: impl Into<String>) -> Error {
    Error::new(ErrorCode::InvalidRequest, detail)
}

/// Items after `since`, at most `limit` (clamped to [`MAX_LIMIT`]), plus the
/// cursor of the next page when more remain.
fn page<T: Clone>(
    all: &[T],
    seq: impl Fn(&T) -> u64,
    p: &PageParams,
) -> Result<(Vec<T>, Option<u64>), Error> {
    let limit = match p.limit {
        None => DEFAULT_LIMIT,
        Some(0) => return Err(bad("limit must be at least 1")),
        Some(n) => n.min(MAX_LIMIT),
    } as usize;
    let mut rest = all.iter().filter(|x| seq(x) > p.since);
    let items: Vec<T> = rest.by_ref().take(limit).cloned().collect();
    let next = rest.next().and(items.last().map(&seq));
    Ok((items, next))
}

fn status(st: &mut FakeState, _: NoParams) -> Result<Status, Error> {
    Ok(Status {
        head: st.head(),
        folders: FOLDERS.len() as u64,
        files: 1800,
        bytes: 1_288_490_188,
        conflicts: st.conflicts.len() as u64,
        peers: 1,
        mode: "live".into(),
    })
}

fn changes(st: &mut FakeState, p: PageParams) -> Result<Changes, Error> {
    let (items, next_cursor) = page(&st.journal, |c| c.seq, &p)?;
    Ok(Changes {
        head: st.head(),
        items,
        next_cursor,
    })
}

fn conflicts(st: &mut FakeState, p: PageParams) -> Result<Conflicts, Error> {
    let (items, next_cursor) = page(&st.conflicts, |c| c.seq, &p)?;
    Ok(Conflicts { items, next_cursor })
}

fn plan(st: &mut FakeState, p: PlanParams) -> Result<Plan, Error> {
    if let Some(f) = &p.folder
        && !FOLDERS.contains(&f.as_str())
    {
        return Err(Error::new(ErrorCode::NotFound, format!("no folder: {f}")));
    }
    let plan = Plan {
        id: format!("plan-{}", st.plans.len() + 1),
        effect: Effect {
            files: PLAN_FILES,
            bytes: PLAN_FILES * 4096,
            peers: 1,
        },
        state: "pending".into(),
    };
    st.plans.push(PlanRec {
        plan: plan.clone(),
        seq_before: 0,
    });
    Ok(plan)
}

fn find<'a>(st: &'a mut FakeState, id: &str) -> Result<&'a mut PlanRec, Error> {
    st.plans
        .iter_mut()
        .find(|r| r.plan.id == id)
        .ok_or_else(|| Error::new(ErrorCode::NotFound, format!("no plan: {id}")))
}

fn apply(st: &mut FakeState, p: PlanRef) -> Result<Applied, Error> {
    let head = st.head();
    let rec = find(st, &p.plan)?;
    if rec.plan.state != "pending" {
        return Err(bad(format!("{} is {}", p.plan, rec.plan.state)));
    }
    rec.plan.state = "applied".into();
    rec.seq_before = head;
    let effect = rec.plan.effect;
    let n = effect.files;
    for k in 1..=n {
        st.journal.push(Change {
            seq: head + k,
            op: "modify".into(),
            path: format!("pa{:03x}", head + k),
            bytes: effect.bytes / n,
            peer: None,
        });
    }
    Ok(Applied {
        plan: p.plan,
        effect,
        head: st.head(),
    })
}

fn revert(st: &mut FakeState, p: PlanRef) -> Result<Reverted, Error> {
    let rec = find(st, &p.plan)?;
    if rec.plan.state != "applied" {
        return Err(bad(format!("{} is {}", p.plan, rec.plan.state)));
    }
    let to = rec.seq_before;
    if st
        .plans
        .iter()
        .any(|r| r.plan.state == "applied" && r.seq_before > to)
    {
        return Err(bad("newer plans are applied; revert them first"));
    }
    st.journal.retain(|c| c.seq <= to);
    find(st, &p.plan)?.plan.state = "reverted".into();
    Ok(Reverted {
        plan: p.plan,
        head: st.head(),
    })
}

/// Lift `f` into a service: decode params (invalid ones are `invalid_request`),
/// run it on the shared state, wrap the result in the envelope.
fn lift<P, R>(
    state: &Shared,
    f: fn(&mut FakeState, P) -> Result<R, Error>,
) -> impl crate::service::Service<
    RpcRequest,
    Response = RpcResponse,
    Error = std::convert::Infallible,
    Future = crate::service::RpcFuture,
> + Send
+ use<P, R>
where
    P: DeserializeOwned + 'static,
    R: serde::Serialize + Kind + 'static,
{
    let state = Arc::clone(state);
    handler(move |req: RpcRequest| {
        let params = req.params.unwrap_or_else(|| json!({}));
        let p: P = match serde_json::from_value(params) {
            Ok(p) => p,
            Err(e) => return RpcResponse::err(req.id, ErrorCode::InvalidRequest, e.to_string()),
        };
        let mut st = state.lock().unwrap_or_else(PoisonError::into_inner);
        match f(&mut st, p) {
            Ok(r) => RpcResponse::ok(req.id, &r),
            Err(e) => RpcResponse::err(req.id, e.code, e.detail),
        }
    })
}

/// The full verb table over `state`.
pub fn router(state: &Shared) -> Router {
    Router::new()
        .route("status", lift(state, status))
        .route("changes", lift(state, changes))
        .route("conflicts", lift(state, conflicts))
        .route("plan", lift(state, plan))
        .route("apply", lift(state, apply))
        .route("revert", lift(state, revert))
}

/// A router over fresh fake state.
pub fn fake_router() -> Router {
    router(&Arc::new(Mutex::new(FakeState::default())))
}
