//! Magritte's UI-agnostic git engine.
//!
//! This crate knows nothing about GPUI or any UI. It drives the `git` command
//! line and returns plain data structures, so it can be unit-tested against
//! throwaway repositories with no graphics stack. The frontend is responsible
//! for running these (blocking) calls off the UI thread and for cancellation.

pub mod commit;
pub mod diff;
pub mod error;
pub mod remote;
pub mod repo;
pub mod stage;
pub mod status;
pub mod transient;

pub use commit::CommitMode;
pub use diff::{DiffLine, DiffSource, FileDiff, Hunk, LineKind};
pub use error::{Error, Result};
pub use remote::{RemoteTargets, Upstream};
pub use repo::{GitCommand, GitOutput, Repo};
pub use stage::ApplyTarget;
pub use status::{Change, EntryKind, FileEntry, HeadInfo, Status};
pub use transient::{Command, Transient};
