//! VCS-agnostic foundation shared by Magritte's backend engines.
//!
//! This crate holds the pieces of a CLI-driving VCS engine that contain no
//! git (or any other VCS) semantics: spawning and collecting child processes
//! with cancellation/timeout and process-tree kill, the ring-buffered command
//! log the `$` view reads, the output/error primitives every command wrapper
//! shares, and the pure unified-diff data model + parser (git-format, which
//! other tools such as jj also emit). Command *construction* — argv shapes,
//! config pins, environment — belongs to the per-VCS engine crates.

pub mod diff;
pub mod error;
pub mod log;
pub mod output;
pub mod process;

pub use diff::{parse_diff, unquote_path, DiffLine, FileDiff, Hunk, LineChange, LineKind};
pub use error::{Error, Result};
pub use log::{CommandEntry, CommandLog};
pub use output::{CommandRun, Output};
pub use process::{prepare_spawn, ProcessControl};
