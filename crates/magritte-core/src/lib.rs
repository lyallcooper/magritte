//! Magritte's UI-agnostic git engine **and command model**.
//!
//! This crate knows nothing about GPUI or any UI. It drives the `git` command
//! line and returns plain data structures, so it can be unit-tested against
//! throwaway repositories with no graphics stack. The frontend is responsible
//! for running these (blocking) calls off the UI thread and for cancellation.
//!
//! Scope note: besides the git engine, this crate also owns the **declarative
//! command/menu model** in [`transient`] — magit-style command variants, keys,
//! descriptions, and transient (popup) layouts. That's interaction-model data,
//! not git plumbing, but it lives here *deliberately*: it is UI-agnostic (the
//! app renders it and dispatches keys against it), so keeping it in core gives
//! the app one declarative command surface to share and keeps it unit-testable.
//! If a third consumer or heavier UI modeling ever appears, it can be split into
//! its own `magritte-command` crate; until then the one boundary is intentional.

pub mod branch;
pub mod commit;
pub mod conflict;
pub mod diff;
pub mod error;
pub mod files;
pub mod ignore;
pub mod log;
pub mod merge;
pub mod pick;
pub mod rebase;
pub mod remote;
pub mod repo;
pub mod reset;
pub mod sequence;
pub mod stage;
pub mod stash;
pub mod status;
pub mod transient;

pub use commit::CommitMode;
pub use conflict::ConflictSide;
pub use diff::{DiffLine, DiffSource, FileDiff, Hunk, LineKind};
pub use error::{Error, Result};
pub use ignore::IgnoreDest;
pub use log::LogEntry;
pub use rebase::{RebaseAction, RebaseStep};
pub use remote::{RemoteTargets, Upstream};
pub use repo::{CommandRun, GitCommand, GitOutput, Repo, TagDistance, TagsAround};
pub use reset::ResetMode;
pub use sequence::{Sequence, SequenceKind, SequenceStep};
pub use stage::ApplyTarget;
pub use stash::Stash;
pub use status::{Change, EntryKind, FileEntry, HeadInfo, Status};
pub use transient::{Command, Transient};
