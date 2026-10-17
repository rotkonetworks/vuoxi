//! The vuoxi engine: local changes into the journal, journals between peers,
//! chunks into files.

#![forbid(unsafe_code)]

pub mod folder;
pub mod fs;
pub mod node;
pub mod rules;
pub mod watch;

pub use folder::{BasicFilter, Filter, Folder, Plan};
pub use node::{Event, FolderSpec, Node, NodeConfig, PeerSpec};
pub use rules::{RulesFilter, parse_tag};
