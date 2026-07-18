//! Magritte's UI-agnostic git engine.
//!
//! This crate knows nothing about GPUI or any UI. It drives the `git` command
//! line and returns plain data structures, so it can be unit-tested against
//! throwaway repositories with no graphics stack. The frontend is responsible
//! for running these (blocking) calls off the UI thread and for cancellation.
//! (The transient popup *model* lives in `magritte-ui`; the app's git command
//! vocabulary and menu definitions live in the app crate's `git_transient`.)

pub mod bisect;
pub mod blame;
pub mod branch;
pub mod commit;
pub mod conflict;
pub mod diff;
pub mod error;
pub mod files;
pub mod ignore;
pub mod log;
pub mod merge;
pub mod patch;
pub mod pick;
pub mod rebase;
pub mod remote;
pub mod repo;
pub mod reset;
pub mod sequence;
pub mod stage;
pub mod stash;
pub mod status;
pub mod tag;
pub mod worktree;

pub use bisect::{Bisect, BisectMark};
pub use blame::BlameLine;
pub use branch::LocalBranch;
pub use commit::{CommitMetadata, CommitMode};
pub use conflict::{ConflictSide, Resolution};
pub use diff::{DiffLine, DiffSource, FileDiff, Hunk, LineChange, LineKind};
pub use error::{Error, Result};
pub use ignore::IgnoreDest;
pub use log::LogEntry;
pub use rebase::{RebaseAction, RebaseStep};
pub use remote::{RemoteTargets, Upstream};
pub use repo::{CommandRun, GitCommand, GitOutput, Repo, TagDistance};
pub use reset::ResetMode;
pub use sequence::{Sequence, SequenceKind, SequenceStep};
pub use stage::ApplyTarget;
pub use stash::{SnapshotKind, Stash, StashKind, StashUntracked};
pub use status::{Change, EntryKind, FileEntry, HeadInfo, RefreshNeeds, RefreshSnapshot, Status};
pub use worktree::Worktree;
