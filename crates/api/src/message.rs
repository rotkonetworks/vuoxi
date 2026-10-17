use crate::SCHEMA;
use crate::act::{Action, Applied, Plan, PlanId, Reverted};
use crate::error::ApiError;
use crate::page::Page;
use crate::read::{ChangePage, Conflict, FolderId, Peer, Status};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Wraps every top-level value on the wire: `{"schema":1,"kind":"status",…}`.
/// `kind` comes from the body's enum tag, `schema` from here.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Envelope<T> {
    pub schema: u32,
    #[serde(flatten)]
    pub body: T,
}

impl<T> Envelope<T> {
    pub fn new(body: T) -> Self {
        Self {
            schema: SCHEMA,
            body,
        }
    }

    /// Whether this build understands the sender's schema version.
    pub fn is_supported(&self) -> bool {
        self.schema <= SCHEMA
    }
}

/// Every verb. Read verbs first; act verbs follow plan, apply, revert.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Request {
    Status {
        folder: FolderId,
    },
    /// Journal entries with `seq > since`.
    Changes {
        folder: FolderId,
        since: u64,
        /// Default [`DEFAULT_LIMIT`](crate::DEFAULT_LIMIT), capped at [`MAX_LIMIT`](crate::MAX_LIMIT).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        limit: Option<u32>,
    },
    Conflicts {
        folder: FolderId,
        /// Resume after this conflict id.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cursor: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        limit: Option<u32>,
    },
    Peers {
        folder: FolderId,
        /// Resume after this peer index.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cursor: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        limit: Option<u32>,
    },
    /// Compute the exact effect of an action without doing it.
    Plan {
        folder: FolderId,
        action: Action,
    },
    Apply {
        plan: PlanId,
    },
    /// Undo an applied plan by rolling the index back.
    Revert {
        plan: PlanId,
    },
}

impl Request {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Status { .. } => "status",
            Self::Changes { .. } => "changes",
            Self::Conflicts { .. } => "conflicts",
            Self::Peers { .. } => "peers",
            Self::Plan { .. } => "plan",
            Self::Apply { .. } => "apply",
            Self::Revert { .. } => "revert",
        }
    }
}

/// Every answer, errors included, so a client decodes one type.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Response {
    Status(Status),
    Changes(ChangePage),
    Conflicts(Page<Conflict>),
    Peers(Page<Peer>),
    Plan(Plan),
    Applied(Applied),
    Reverted(Reverted),
    Error(ApiError),
}

impl Response {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Status(_) => "status",
            Self::Changes(_) => "changes",
            Self::Conflicts(_) => "conflicts",
            Self::Peers(_) => "peers",
            Self::Plan(_) => "plan",
            Self::Applied(_) => "applied",
            Self::Reverted(_) => "reverted",
            Self::Error(_) => "error",
        }
    }
}

impl From<ApiError> for Response {
    fn from(e: ApiError) -> Self {
        Self::Error(e)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::act::{Effect, Keep, PlanState};
    use crate::read::{ChangeEntry, ChangeOp, PeerRole, PeerState, PowerMode, Side};
    use std::collections::BTreeMap;

    fn rt<T>(v: T)
    where
        T: Serialize + for<'a> Deserialize<'a> + PartialEq + std::fmt::Debug,
    {
        let j = serde_json::to_string(&v).unwrap();
        assert_eq!(serde_json::from_str::<T>(&j).unwrap(), v, "{j}");
    }

    fn f() -> FolderId {
        FolderId("docs".into())
    }

    fn requests() -> Vec<Request> {
        vec![
            Request::Status { folder: f() },
            Request::Changes {
                folder: f(),
                since: 10,
                limit: Some(5),
            },
            Request::Changes {
                folder: f(),
                since: 0,
                limit: None,
            },
            Request::Conflicts {
                folder: f(),
                cursor: Some(2),
                limit: None,
            },
            Request::Peers {
                folder: f(),
                cursor: None,
                limit: Some(1),
            },
            Request::Plan {
                folder: f(),
                action: Action::Resolve {
                    conflict: 1,
                    keep: Keep::Theirs,
                },
            },
            Request::Apply {
                plan: PlanId("p".into()),
            },
            Request::Revert {
                plan: PlanId("p".into()),
            },
        ]
    }

    pub(crate) fn responses() -> Vec<Response> {
        let side = Side {
            version: BTreeMap::from([("aa".repeat(32), 1)]),
            bytes: 5,
        };
        let effect = Effect {
            files: 1,
            bytes: 5,
            peers: 1,
        };
        vec![
            Response::Status(Status {
                folder: f(),
                files: 1,
                bytes: 5,
                conflicts: 0,
                root: "ab".repeat(32),
                head: 3,
                mode: PowerMode::Live,
                peers_online: 1,
                peers_total: 1,
            }),
            Response::Changes(ChangePage {
                items: vec![ChangeEntry {
                    seq: 4,
                    path: 1,
                    op: ChangeOp::Created,
                    by: None,
                }],
                head: 9,
                next_cursor: Some(4),
            }),
            Response::Conflicts(Page {
                items: vec![Conflict {
                    id: 1,
                    path: 2,
                    ours: side.clone(),
                    theirs: side,
                    peer: "bb".repeat(32),
                }],
                next_cursor: None,
                total: Some(1),
            }),
            Response::Peers(Page {
                items: vec![Peer {
                    id: "cc".repeat(32),
                    role: PeerRole::Device,
                    state: PeerState::Connected,
                    seen: 3,
                    lag: 1,
                }],
                next_cursor: None,
                total: None,
            }),
            Response::Plan(Plan {
                id: PlanId("p".into()),
                folder: f(),
                action: Action::Sync { peer: None },
                effect: effect.clone(),
                state: PlanState::Planned,
                base_root: "00".repeat(32),
            }),
            Response::Applied(Applied {
                plan: PlanId("p".into()),
                effect,
                root: "11".repeat(32),
            }),
            Response::Reverted(Reverted {
                plan: PlanId("p".into()),
                root: "00".repeat(32),
            }),
            Response::Error(ApiError::not_found("no such folder")),
        ]
    }

    #[test]
    fn requests_round_trip_with_kind() {
        for r in requests() {
            let e = Envelope::new(r.clone());
            let v = serde_json::to_value(&e).unwrap();
            assert_eq!(v["kind"], r.kind());
            assert_eq!(v["schema"], SCHEMA);
            rt(e);
        }
    }

    #[test]
    fn responses_round_trip_with_kind() {
        for r in responses() {
            let e = Envelope::new(r.clone());
            let v = serde_json::to_value(&e).unwrap();
            assert_eq!(v["kind"], r.kind());
            assert_eq!(v["schema"], SCHEMA);
            rt(e);
        }
    }

    #[test]
    fn added_fields_do_not_break_readers() {
        let j = r#"{"schema":2,"kind":"status","folder":"docs","future":[1],"limit":3}"#;
        let e: Envelope<Request> = serde_json::from_str(j).unwrap();
        assert!(!e.is_supported());
        assert_eq!(e.body, Request::Status { folder: f() });
    }

    #[test]
    fn schema_names_kind_and_schema() {
        let req = serde_json::to_string(&schemars::schema_for!(Envelope<Request>)).unwrap();
        let resp = serde_json::to_string(&schemars::schema_for!(Envelope<Response>)).unwrap();
        for s in [&req, &resp] {
            assert!(s.contains("\"kind\""), "{s}");
            assert!(s.contains("\"schema\""), "{s}");
        }
        assert!(req.contains("\"changes\"") && resp.contains("\"capability_denied\""));
        let bare = serde_json::to_string(&schemars::schema_for!(Request)).unwrap();
        assert!(bare.contains("\"kind\""));
    }
}
