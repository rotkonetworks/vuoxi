//! The change journal and the manifest index, as a [`vuoxi_core::Store`].
//!
//! The journal is append-only and is the reason vuoxi never rescans: every
//! change the watcher saw is here, so a restart replays the journal instead of
//! walking the tree. Backed by commonware `storage::journal`.

#![forbid(unsafe_code)]
