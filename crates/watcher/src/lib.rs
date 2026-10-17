//! Library half of vuoxi-watcher: everything except the privileged glue is
//! pure and unit-testable. [`record`] is the wire format to the daemon,
//! [`parse`] turns raw fanotify bytes into records, [`ignore`] drops build
//! noise, [`handlemap`] resolves handles to paths for the daemon, [`sandbox`] hardens the process. Only [`sys`] contains unsafe.

#![deny(unsafe_op_in_unsafe_fn)]
#![deny(unsafe_code)]

pub mod handlemap;
pub mod ignore;
pub mod parse;
pub mod pump;
pub mod record;
pub mod sandbox;
#[allow(unsafe_code)]
pub mod sys;

pub use handlemap::{HandleMap, Resolved};
pub use ignore::{Ignore, IgnoreTracker};
pub use record::{Handle, Kind, Record};
