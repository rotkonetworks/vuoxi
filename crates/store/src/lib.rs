//! The change journal and the manifest index, as a [`vuoxi_core::Store`].
//!
//! The journal is append-only and is the reason vuoxi never rescans: every
//! change the watcher saw is here, so a restart replays the journal instead of
//! walking the tree. Journal on commonware `storage::journal`; the manifest
//! index on NOMT, whose Merkle root doubles as the folder state peers compare.

#![forbid(unsafe_code)]
