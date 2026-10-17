use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

// core: needs serde + JsonSchema (feature-gated) on PathId/PeerId/ChunkId/VersionVector;
// until then ids cross the API as integers and hex strings.

/// A synced folder, named by the string the user chose.
#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(transparent)]
pub struct FolderId(pub String);

// core: needs `Mode` to live in core (or layers to derive serde) so this copy can go.
/// Mirror of the power mode that selects policy layers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PowerMode {
    Live,
    Batched,
    Frugal,
    Hold,
}

impl PowerMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::Batched => "batched",
            Self::Frugal => "frugal",
            Self::Hold => "hold",
        }
    }
}

/// Folder summary: counts first, detail on request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Status {
    pub folder: FolderId,
    pub files: u64,
    pub bytes: u64,
    pub conflicts: u64,
    /// Index root, 64 hex chars. Equal roots mean equal folders.
    pub root: String,
    /// Newest journal sequence number; the cursor for `changes`.
    pub head: u64,
    pub mode: PowerMode,
    pub peers_online: u32,
    pub peers_total: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ChangeOp {
    Created,
    Modified,
    Deleted,
}

impl ChangeOp {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Modified => "modified",
            Self::Deleted => "deleted",
        }
    }
}

/// One journal entry.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ChangeEntry {
    /// Journal sequence number, 1, 2, 3, …
    pub seq: u64,
    /// Opaque file id.
    pub path: u64,
    pub op: ChangeOp,
    /// Principal that made the change (device or agent name), if recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by: Option<String>,
}

/// Journal entries after a cursor, oldest first.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ChangePage {
    pub items: Vec<ChangeEntry>,
    /// Journal head at read time. Resume later with `since = head`.
    pub head: u64,
    /// `since` for the next page; absent once caught up to `head`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<u64>,
}

/// One side of a conflict.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Side {
    /// Version vector: device id (hex) to counter.
    pub version: BTreeMap<String, u64>,
    pub bytes: u64,
}

/// Concurrent edits; both versions are kept.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Conflict {
    /// Stable id; detail and resolution address it.
    pub id: u64,
    pub path: u64,
    pub ours: Side,
    pub theirs: Side,
    /// Peer that holds `theirs`, 64 hex chars.
    pub peer: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PeerState {
    Connected,
    Idle,
    Offline,
}

impl PeerState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Connected => "connected",
            Self::Idle => "idle",
            Self::Offline => "offline",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PeerRole {
    Device,
    Relay,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Peer {
    /// Public key, 64 hex chars.
    pub id: String,
    pub role: PeerRole,
    pub state: PeerState,
    /// Highest of the peer's journal entries we have seen.
    pub seen: u64,
    /// Entries of ours the peer has not yet seen.
    pub lag: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rt<T>(v: T)
    where
        T: Serialize + for<'a> Deserialize<'a> + PartialEq + std::fmt::Debug,
    {
        let j = serde_json::to_string(&v).unwrap();
        assert_eq!(serde_json::from_str::<T>(&j).unwrap(), v, "{j}");
    }

    pub(crate) fn side() -> Side {
        Side {
            version: BTreeMap::from([("aa".repeat(32), 3)]),
            bytes: 10,
        }
    }

    #[test]
    fn round_trips() {
        rt(FolderId("docs".into()));
        for m in [
            PowerMode::Live,
            PowerMode::Batched,
            PowerMode::Frugal,
            PowerMode::Hold,
        ] {
            rt(m);
        }
        rt(Status {
            folder: FolderId("docs".into()),
            files: 3,
            bytes: 4096,
            conflicts: 1,
            root: "00".repeat(32),
            head: 48_211,
            mode: PowerMode::Batched,
            peers_online: 1,
            peers_total: 2,
        });
        rt(ChangeEntry {
            seq: 1,
            path: 7,
            op: ChangeOp::Deleted,
            by: Some("agent-a".into()),
        });
        rt(ChangePage {
            items: vec![],
            head: 5,
            next_cursor: None,
        });
        rt(Conflict {
            id: 1,
            path: 7,
            ours: side(),
            theirs: side(),
            peer: "bb".repeat(32),
        });
        rt(Peer {
            id: "cc".repeat(32),
            role: PeerRole::Relay,
            state: PeerState::Idle,
            seen: 9,
            lag: 0,
        });
    }

    #[test]
    fn unknown_fields_are_ignored() {
        let e: ChangeEntry =
            serde_json::from_str(r#"{"seq":1,"path":2,"op":"created","new_field":true}"#).unwrap();
        assert_eq!(e.by, None);
    }
}
