use crate::read::FolderId;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Identifies a plan; `apply` takes it, `revert` undoes it.
#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(transparent)]
pub struct PlanId(pub String);

/// Which side of a conflict survives as the main file. The other is kept
/// beside it, never discarded.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Keep {
    Ours,
    Theirs,
}

/// Something the daemon can be asked to do, planned before it is done.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Action {
    /// Reconcile with one peer, or with all if absent.
    Sync {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        peer: Option<String>,
    },
    /// Settle one conflict by id.
    Resolve { conflict: u64, keep: Keep },
}

/// The exact effect of a plan, computed without doing it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Effect {
    pub files: u64,
    pub bytes: u64,
    pub peers: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PlanState {
    Planned,
    Applied,
    Reverted,
}

impl PlanState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Planned => "planned",
            Self::Applied => "applied",
            Self::Reverted => "reverted",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Plan {
    pub id: PlanId,
    pub folder: FolderId,
    pub action: Action,
    pub effect: Effect,
    pub state: PlanState,
    /// Index root the plan was computed against; `apply` fails with
    /// `stale_plan` if the folder has moved on.
    pub base_root: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Applied {
    pub plan: PlanId,
    pub effect: Effect,
    /// Index root after applying; the rollback point for `revert`.
    pub root: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Reverted {
    pub plan: PlanId,
    /// Index root after the rollback.
    pub root: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_is_tagged() {
        let a = Action::Resolve {
            conflict: 3,
            keep: Keep::Ours,
        };
        let j = serde_json::to_string(&a).unwrap();
        assert_eq!(j, r#"{"kind":"resolve","conflict":3,"keep":"ours"}"#);
        assert_eq!(serde_json::from_str::<Action>(&j).unwrap(), a);
        let s = Action::Sync { peer: None };
        let j = serde_json::to_string(&s).unwrap();
        assert_eq!(j, r#"{"kind":"sync"}"#);
        assert_eq!(serde_json::from_str::<Action>(&j).unwrap(), s);
    }

    #[test]
    fn plan_round_trip() {
        let eff = Effect {
            files: 2,
            bytes: 99,
            peers: 1,
        };
        let p = Plan {
            id: PlanId("p1".into()),
            folder: FolderId("docs".into()),
            action: Action::Sync {
                peer: Some("aa".repeat(32)),
            },
            effect: eff.clone(),
            state: PlanState::Planned,
            base_root: "00".repeat(32),
        };
        let j = serde_json::to_string(&p).unwrap();
        assert_eq!(serde_json::from_str::<Plan>(&j).unwrap(), p);
        let a = Applied {
            plan: p.id.clone(),
            effect: eff,
            root: "11".repeat(32),
        };
        let j = serde_json::to_string(&a).unwrap();
        assert_eq!(serde_json::from_str::<Applied>(&j).unwrap(), a);
        let r = Reverted {
            plan: p.id,
            root: "00".repeat(32),
        };
        let j = serde_json::to_string(&r).unwrap();
        assert_eq!(serde_json::from_str::<Reverted>(&j).unwrap(), r);
    }
}
