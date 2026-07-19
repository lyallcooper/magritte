//! Bounded repository filesystem monitoring and pure refresh scheduling.
//!
//! The native callback never performs Git work and never waits for the UI. It
//! folds events into one shared accumulator, then sends only a capacity-one
//! wake token. Scheduling and reconciliation remain on the GPUI thread.

use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use magritte_core::Repo;
use notify::{RecursiveMode, Watcher};

pub(crate) const PATH_CAP: usize = 256;
pub(crate) const QUIET_DELAY: Duration = Duration::from_millis(300);
pub(crate) const CONTINUOUS_PROBE: Duration = Duration::from_secs(1);
pub(crate) const BLOCK_RETRY: Duration = Duration::from_millis(250);
pub(crate) const LOCK_FORCE_AFTER: Duration = Duration::from_secs(30);
const LIVENESS_SETTLE: Duration = Duration::from_millis(100);
const LIVENESS_TIMEOUT: Duration = Duration::from_secs(2);
const IGNORE_CHECK_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChangeScope {
    StatusOnly,
    RepositoryWide,
}

impl ChangeScope {
    fn merge(self, other: Self) -> Self {
        if matches!(self, Self::RepositoryWide) || matches!(other, Self::RepositoryWide) {
            Self::RepositoryWide
        } else {
            Self::StatusOnly
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RepoChangeBatch {
    pub(crate) sequence: u64,
    /// Highest native watcher event carried by this batch. Synthetic focus,
    /// polling, and reconciliation requests deliberately leave this unset so
    /// they cannot cover a watcher event whose paths have not been delivered.
    native_sequence: Option<u64>,
    synthetic: bool,
    pub(crate) scope: ChangeScope,
    pub(crate) paths: Vec<PathBuf>,
    pub(crate) unknown: bool,
}

impl RepoChangeBatch {
    pub(crate) fn repository_wide(sequence: u64) -> Self {
        Self {
            sequence,
            native_sequence: None,
            synthetic: true,
            scope: ChangeScope::RepositoryWide,
            paths: Vec::new(),
            unknown: false,
        }
    }

    fn merge(&mut self, other: Self) {
        self.sequence = self.sequence.max(other.sequence);
        self.native_sequence = self.native_sequence.max(other.native_sequence);
        self.synthetic |= other.synthetic;
        self.scope = self.scope.merge(other.scope);
        self.unknown |= other.unknown;
        let mut seen: HashSet<String> = self
            .paths
            .iter()
            .map(|path| crate::path_identity::key(path))
            .collect();
        for path in other.paths {
            let identity = crate::path_identity::key(&path);
            if seen.contains(&identity) {
                continue;
            }
            if self.paths.len() == PATH_CAP {
                self.unknown = true;
                break;
            }
            seen.insert(identity);
            self.paths.push(path);
        }
    }

    pub(crate) fn covered_native_sequence(&self) -> u64 {
        self.native_sequence.unwrap_or(0)
    }

    fn needs_ignore_check(&self) -> bool {
        self.native_sequence.is_some()
            && self.scope == ChangeScope::StatusOnly
            && !self.unknown
            && !self.paths.is_empty()
    }

    /// Remove paths Git classifies as ignored. `false` means the entire native
    /// batch was ignored and its sequence can be covered without a status read.
    fn retain_non_ignored(&mut self, ignored: Vec<String>) -> bool {
        let ignored: HashSet<String> = ignored
            .into_iter()
            .map(|path| crate::path_identity::text_key(&path))
            .collect();
        self.paths
            .retain(|path| !ignored.contains(&crate::path_identity::key(path)));
        !self.paths.is_empty()
    }
}

#[derive(Default)]
struct PendingBatch {
    batch: Option<RepoChangeBatch>,
}

impl PendingBatch {
    fn merge(&mut self, batch: RepoChangeBatch) {
        match &mut self.batch {
            Some(pending) => pending.merge(batch),
            None => self.batch = Some(batch),
        }
    }

    fn take(&mut self) -> Option<RepoChangeBatch> {
        self.batch.take()
    }
}

/// Shared callback state.
///
/// The callback invariant is deliberately strict: lock and merge the event,
/// drop the lock, then `try_send(())` on the capacity-one channel. A failed
/// send is expected and safe—the already-pending wake will observe the newly
/// merged event in this accumulator. The channel carries no event data, and a
/// filesystem callback must never use a blocking send.
pub(crate) struct MonitorShared {
    pending: Mutex<PendingBatch>,
    sequence: AtomicU64,
    callback_seen: AtomicBool,
}

impl MonitorShared {
    fn merge(&self, batch: RepoChangeBatch) {
        self.pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .merge(batch);
    }

    fn note_callback(&self) {
        self.callback_seen.store(true, Ordering::Relaxed);
    }

    pub(crate) fn take(&self) -> Option<RepoChangeBatch> {
        self.pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
    }

    fn next_sequence(&self) -> u64 {
        self.sequence
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_add(1)
    }

    pub(crate) fn callback_seen(&self) -> bool {
        self.callback_seen.load(Ordering::Relaxed)
    }
}

#[derive(Clone)]
struct WatchRoots {
    worktree: PathBuf,
    git_dir: PathBuf,
    common_dir: PathBuf,
}

impl WatchRoots {
    fn classify(&self, path: &Path) -> Option<(ChangeScope, Option<PathBuf>)> {
        if let Some(rel) = path
            .strip_prefix(&self.git_dir)
            .ok()
            .or_else(|| path.strip_prefix(&self.common_dir).ok())
        {
            if ignore_git_path(rel) {
                return None;
            }
            let status = rel == Path::new("index");
            return Some((
                if status {
                    ChangeScope::StatusOnly
                } else {
                    ChangeScope::RepositoryWide
                },
                None,
            ));
        }
        path.strip_prefix(&self.worktree)
            .ok()
            .map(|rel| (ChangeScope::StatusOnly, Some(rel.to_path_buf())))
    }

    fn mandatory_roots_exist(&self) -> bool {
        self.worktree.is_dir() && self.git_dir.is_dir() && self.common_dir.is_dir()
    }
}

fn ignore_git_path(path: &Path) -> bool {
    let first = path
        .components()
        .next()
        .and_then(|c| c.as_os_str().to_str());
    if matches!(first, Some("objects" | "magritte")) {
        return true;
    }
    if path == Path::new("fsmonitor--daemon.ipc") || path.starts_with("fsmonitor--daemon/cookies") {
        return true;
    }
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    name.ends_with(".lock")
        || name.ends_with(".tmp")
        || name.ends_with(".temp")
        || name.ends_with('~')
        || name.starts_with(".#")
        || name.starts_with(".magritte-watch-probe-")
}

fn normalize_event(
    roots: &WatchRoots,
    sequence: u64,
    event: notify::Event,
) -> Option<RepoChangeBatch> {
    let mut batch = RepoChangeBatch {
        sequence,
        native_sequence: Some(sequence),
        synthetic: false,
        scope: ChangeScope::StatusOnly,
        paths: Vec::new(),
        unknown: event.need_rescan(),
    };
    if event.need_rescan()
        || matches!(
            event.kind,
            notify::EventKind::Any | notify::EventKind::Other
        )
    {
        batch.scope = ChangeScope::RepositoryWide;
    }
    for path in event.paths {
        let Some((scope, worktree_path)) = roots.classify(&path) else {
            continue;
        };
        batch.scope = batch.scope.merge(scope);
        if let Some(path) = worktree_path {
            if path.as_os_str().is_empty() {
                batch.unknown = true;
                continue;
            }
            if batch.paths.len() == PATH_CAP {
                batch.unknown = true;
            } else if !batch.paths.iter().any(|existing| {
                crate::path_identity::key(existing) == crate::path_identity::key(&path)
            }) {
                batch.paths.push(path);
            }
        } else if scope == ChangeScope::StatusOnly {
            // The index says status changed but carries no affected worktree
            // path. Keep the refresh status-scoped while invalidating all
            // status diffs, rather than dropping an empty-path batch.
            batch.unknown = true;
        }
    }
    (batch.unknown || batch.scope == ChangeScope::RepositoryWide || !batch.paths.is_empty())
        .then_some(batch)
}

pub(crate) struct RepositoryMonitor {
    _watcher: notify::RecommendedWatcher,
    shared: Arc<MonitorShared>,
    roots: WatchRoots,
    watched_optional: HashSet<PathBuf>,
}

impl RepositoryMonitor {
    pub(crate) fn shared(&self) -> Arc<MonitorShared> {
        self.shared.clone()
    }

    pub(crate) fn next_sequence(&self) -> u64 {
        self.shared.next_sequence()
    }

    pub(crate) fn mandatory_roots_exist(&self) -> bool {
        self.roots.mandatory_roots_exist()
    }

    pub(crate) fn optional_roots_current(&self) -> bool {
        optional_watch_dirs(&self.roots)
            .into_iter()
            .filter(|dir| dir.is_dir())
            .all(|dir| self.watched_optional.contains(&dir))
    }

    pub(crate) fn liveness_probe_path(&self) -> PathBuf {
        self.roots
            .git_dir
            .join(format!(".magritte-watch-probe-{}", std::process::id()))
    }
}

pub(crate) struct MonitorInstall {
    pub(crate) monitor: RepositoryMonitor,
    pub(crate) wake: async_channel::Receiver<()>,
}

pub(crate) fn install(
    repo: &Repo,
    git_dir: PathBuf,
    common_dir: PathBuf,
    sequence_base: u64,
) -> notify::Result<MonitorInstall> {
    let roots = WatchRoots {
        worktree: canonical_or(repo.workdir()),
        git_dir: canonical_or(&git_dir),
        common_dir: canonical_or(&common_dir),
    };
    let shared = Arc::new(MonitorShared {
        pending: Mutex::new(PendingBatch::default()),
        sequence: AtomicU64::new(sequence_base),
        callback_seen: AtomicBool::new(false),
    });
    let (tx, wake) = async_channel::bounded(1);
    let callback_shared = shared.clone();
    let callback_roots = roots.clone();
    let mut watcher = notify::recommended_watcher(move |result| {
        let seq = callback_shared.next_sequence();
        let batch = match result {
            Ok(event) => {
                callback_shared.note_callback();
                normalize_event(&callback_roots, seq, event)
            }
            Err(_) => Some(RepoChangeBatch {
                sequence: seq,
                native_sequence: Some(seq),
                synthetic: false,
                scope: ChangeScope::RepositoryWide,
                paths: Vec::new(),
                unknown: true,
            }),
        };
        if let Some(batch) = batch {
            callback_shared.merge(batch);
            let _ = tx.try_send(());
        }
    })?;

    let mut watched = HashSet::new();
    watch_once(
        &mut watcher,
        &roots.worktree,
        RecursiveMode::Recursive,
        &mut watched,
    )?;
    watch_once(
        &mut watcher,
        &roots.git_dir,
        RecursiveMode::NonRecursive,
        &mut watched,
    )?;
    watch_once(
        &mut watcher,
        &roots.common_dir,
        RecursiveMode::NonRecursive,
        &mut watched,
    )?;

    let mut watched_optional = HashSet::new();
    for dir in optional_watch_dirs(&roots) {
        if dir.is_dir() {
            watch_once(&mut watcher, &dir, RecursiveMode::Recursive, &mut watched)?;
            watched_optional.insert(dir.clone());
        }
        if let Some(parent) = dir.parent().filter(|parent| parent.is_dir()) {
            watch_once(
                &mut watcher,
                parent,
                RecursiveMode::NonRecursive,
                &mut watched,
            )?;
        }
    }

    Ok(MonitorInstall {
        monitor: RepositoryMonitor {
            _watcher: watcher,
            shared,
            roots,
            watched_optional,
        },
        wake,
    })
}

fn canonical_or(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn optional_watch_dirs(roots: &WatchRoots) -> Vec<PathBuf> {
    let mut dirs = vec![
        roots.common_dir.join("refs"),
        roots.common_dir.join("logs/refs"),
        roots.git_dir.join("rebase-merge"),
        roots.git_dir.join("rebase-apply"),
        roots.git_dir.join("sequencer"),
    ];
    dirs.sort();
    dirs.dedup();
    dirs
}

fn watch_once(
    watcher: &mut notify::RecommendedWatcher,
    path: &Path,
    mode: RecursiveMode,
    watched: &mut HashSet<PathBuf>,
) -> notify::Result<()> {
    if watched.insert(path.to_path_buf()) {
        watcher.watch(path, mode)?;
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MonitorMode {
    Disabled,
    Native,
    Polling,
}

/// Pure event/probe scheduling state. GPUI timers provide the clock; tests use
/// explicit `Instant`s.
pub(crate) struct MonitorSchedule {
    pub(crate) mode: MonitorMode,
    pending: Option<RepoChangeBatch>,
    first_event: Option<Instant>,
    last_event: Option<Instant>,
    last_probe: Option<Instant>,
    blocked_since: Option<Instant>,
    unchanged_ordinal: u8,
    in_flight: bool,
    last_probe_continuous: bool,
    covered_sequence: u64,
    last_status_duration: Duration,
    last_broad_poll: Option<Instant>,
}

impl Default for MonitorSchedule {
    fn default() -> Self {
        Self {
            mode: MonitorMode::Disabled,
            pending: None,
            first_event: None,
            last_event: None,
            last_probe: None,
            blocked_since: None,
            unchanged_ordinal: 0,
            in_flight: false,
            last_probe_continuous: false,
            covered_sequence: 0,
            last_status_duration: Duration::from_millis(200),
            last_broad_poll: None,
        }
    }
}

impl MonitorSchedule {
    pub(crate) fn enable_native(&mut self) {
        self.mode = MonitorMode::Native;
        self.unchanged_ordinal = 0;
        self.blocked_since = None;
    }

    pub(crate) fn enable_polling(&mut self, now: Instant, sequence: u64) {
        self.mode = MonitorMode::Polling;
        self.ingest(RepoChangeBatch::repository_wide(sequence), now);
    }

    pub(crate) fn disable(&mut self) {
        self.mode = MonitorMode::Disabled;
        self.pending = None;
        self.first_event = None;
        self.last_event = None;
        self.blocked_since = None;
        self.in_flight = false;
        self.unchanged_ordinal = 0;
    }

    pub(crate) fn ingest(&mut self, batch: RepoChangeBatch, now: Instant) {
        if self.mode == MonitorMode::Disabled
            || batch
                .native_sequence
                .is_some_and(|sequence| sequence <= self.covered_sequence)
        {
            return;
        }
        match &mut self.pending {
            Some(pending) => pending.merge(batch),
            None => self.pending = Some(batch),
        }
        self.first_event.get_or_insert(now);
        self.last_event = Some(now);
    }

    pub(crate) fn has_pending(&self) -> bool {
        self.pending.is_some()
    }

    fn probe_in_flight(&self) -> bool {
        self.in_flight
    }

    pub(crate) fn pending_due(&self, now: Instant) -> bool {
        if self.in_flight || self.pending.is_none() {
            return false;
        }
        let quiet = self
            .last_event
            .is_none_or(|event| now.duration_since(event) >= QUIET_DELAY);
        let continuous = self
            .first_event
            .is_some_and(|event| now.duration_since(event) >= CONTINUOUS_PROBE);
        let gap = unchanged_gap(self.unchanged_ordinal);
        let gap_met = self
            .last_probe
            .is_none_or(|probe| now.duration_since(probe) >= gap);
        (quiet || continuous) && gap_met
    }

    pub(crate) fn next_event_delay(&self, now: Instant) -> Duration {
        let quiet_at = self.last_event.unwrap_or(now) + QUIET_DELAY;
        let continuous_at = self.first_event.unwrap_or(now) + CONTINUOUS_PROBE;
        let mut due = quiet_at.min(continuous_at);
        if let Some(last) = self.last_probe {
            due = due.max(last + unchanged_gap(self.unchanged_ordinal));
        }
        due.saturating_duration_since(now)
    }

    pub(crate) fn note_blocked(&mut self, now: Instant) {
        self.blocked_since.get_or_insert(now);
    }

    pub(crate) fn clear_blocked(&mut self) {
        self.blocked_since = None;
    }

    pub(crate) fn lock_force_due(&self, now: Instant) -> bool {
        self.blocked_since
            .is_some_and(|start| now.duration_since(start) >= LOCK_FORCE_AFTER)
    }

    pub(crate) fn begin_probe(&mut self, now: Instant) -> Option<RepoChangeBatch> {
        if self.in_flight {
            return None;
        }
        let batch = self.pending.take()?;
        self.last_probe_continuous = self
            .last_event
            .is_some_and(|event| now.duration_since(event) < QUIET_DELAY);
        self.in_flight = true;
        self.first_event = None;
        self.last_event = None;
        self.last_probe = Some(now);
        self.blocked_since = None;
        Some(batch)
    }

    pub(crate) fn finish_probe(
        &mut self,
        now: Instant,
        changed: bool,
        duration: Duration,
        covered_sequence: u64,
    ) {
        self.in_flight = false;
        self.last_probe = Some(now);
        self.last_status_duration = duration.max(Duration::from_millis(1));
        self.covered_sequence = self.covered_sequence.max(covered_sequence);
        if changed {
            self.unchanged_ordinal = 0;
        } else if self.mode == MonitorMode::Polling || self.last_probe_continuous {
            self.unchanged_ordinal = self.unchanged_ordinal.saturating_add(1).min(3);
        } else {
            // A settled native event must remain a sub-second probe even if an
            // earlier, unrelated probe found no status-shape change.
            self.unchanged_ordinal = 0;
        }
        if self.pending.as_ref().is_some_and(|pending| {
            !pending.synthetic
                && pending
                    .native_sequence
                    .is_some_and(|sequence| sequence <= self.covered_sequence)
        }) {
            self.pending = None;
        }
    }

    pub(crate) fn abandon_probe(&mut self) {
        self.in_flight = false;
    }

    pub(crate) fn retry_probe(&mut self, batch: RepoChangeBatch, now: Instant, duration: Duration) {
        self.in_flight = false;
        self.last_probe = Some(now);
        self.last_status_duration = duration.max(Duration::from_millis(1));
        self.unchanged_ordinal = self.unchanged_ordinal.saturating_add(1).min(3);
        self.ingest(batch, now);
    }

    pub(crate) fn cover_without_probe(&mut self, sequence: u64) {
        self.covered_sequence = self.covered_sequence.max(sequence);
        if self.pending.as_ref().is_some_and(|pending| {
            !pending.synthetic
                && pending
                    .native_sequence
                    .is_some_and(|sequence| sequence <= self.covered_sequence)
        }) {
            self.pending = None;
        }
    }

    pub(crate) fn next_poll_delay(&self) -> Duration {
        let duration_floor =
            (self.last_status_duration * 10).clamp(Duration::from_secs(2), Duration::from_secs(60));
        unchanged_gap(self.unchanged_ordinal.max(1)).max(duration_floor)
    }

    pub(crate) fn broad_poll_due(&self, now: Instant) -> bool {
        let interval = (self.last_status_duration * 20)
            .clamp(Duration::from_secs(60), Duration::from_secs(300));
        self.last_broad_poll
            .is_none_or(|last| now.duration_since(last) >= interval)
    }

    pub(crate) fn mark_broad_poll(&mut self, now: Instant) {
        self.last_broad_poll = Some(now);
    }

    #[cfg(test)]
    fn covered_sequence(&self) -> u64 {
        self.covered_sequence
    }
}

fn unchanged_gap(ordinal: u8) -> Duration {
    match ordinal {
        0 => Duration::ZERO,
        1 => Duration::from_secs(2),
        2 => Duration::from_secs(5),
        _ => Duration::from_secs(10),
    }
}

fn lock_under(dir: &Path, depth: usize) -> bool {
    if depth == 0 {
        return false;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    entries.flatten().any(|entry| {
        let path = entry.path();
        path.extension()
            .is_some_and(|extension| extension == "lock")
            || (path.is_dir() && lock_under(&path, depth - 1))
    })
}

impl crate::StatusView {
    pub(crate) fn refresh_initial_repository(&mut self, cx: &mut gpui::Context<Self>) {
        let now = Instant::now();
        let batch = self.monitor_schedule.begin_probe(now);
        if batch.is_some() && self.monitor_schedule.mode == MonitorMode::Polling {
            self.monitor_schedule.mark_broad_poll(now);
            self.monitor_retrying_native = true;
        }
        let scope = batch
            .as_ref()
            .map(|batch| batch.scope)
            .unwrap_or(ChangeScope::RepositoryWide);
        self.pending_refresh_origin = None;
        self.refresh_with_origin(crate::RefreshOrigin::Initial, scope, batch, cx);
    }

    /// Install, drop, or replace the repository monitor to match the live
    /// setting. A failed or partial native setup is discarded as a unit and
    /// replaced by adaptive polling.
    pub(crate) fn configure_repository_monitor(&mut self, cx: &mut gpui::Context<Self>) {
        let retrying_native = self.monitor_retrying_native;
        self.monitor_retrying_native = false;
        self.monitor_listener_gen.bump();
        self.monitor_timer_gen.bump();
        self.monitor_poll_gen.bump();
        self.repository_monitor = None;
        self.pending_refresh_origin = None;

        if !self.config.auto_refresh || self.repo.is_none() {
            self.monitor_schedule.disable();
            return;
        }

        let Some(repo) = self.repo.clone() else {
            return;
        };
        let Some(git_dir) = self.worktree_git_dir.clone() else {
            if retrying_native {
                self.schedule_poll(cx);
            } else {
                self.enter_polling_mode(cx);
            }
            return;
        };
        let common_dir = self
            .repo_scope_dir
            .as_ref()
            .and_then(|scope| scope.parent())
            .map(Path::to_path_buf)
            .or_else(|| repo.git_common_dir().ok());
        let Some(common_dir) = common_dir else {
            if retrying_native {
                self.schedule_poll(cx);
            } else {
                self.enter_polling_mode(cx);
            }
            return;
        };

        self.monitor_sequence = self.monitor_sequence.wrapping_add(1);
        let reconciliation_sequence = self.monitor_sequence;
        match install(&repo, git_dir, common_dir, reconciliation_sequence) {
            Ok(installed) => {
                self.monitor_schedule.enable_native();
                self.monitor_schedule.ingest(
                    RepoChangeBatch::repository_wide(reconciliation_sequence),
                    Instant::now(),
                );
                self.pending_refresh_origin = Some(crate::RefreshOrigin::Monitor);
                let listener_gen = self.monitor_listener_gen.bump();
                let shared = installed.monitor.shared();
                let liveness_shared = shared.clone();
                let liveness_probe = installed.monitor.liveness_probe_path();
                let wake = installed.wake;
                self.repository_monitor = Some(installed.monitor);
                cx.spawn(async move |this, cx| {
                    while wake.recv().await.is_ok() {
                        let Some(batch) = shared.take() else { continue };
                        if this
                            .update(cx, |this, cx| {
                                if !this.monitor_listener_gen.is_current(listener_gen) {
                                    return;
                                }
                                this.receive_repository_batch(batch, cx);
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                })
                .detach();
                self.verify_native_monitor(listener_gen, liveness_shared, liveness_probe, cx);
                self.schedule_repository_refresh(cx);
            }
            Err(_) if retrying_native => self.schedule_poll(cx),
            Err(_) => self.enter_polling_mode(cx),
        }
    }

    fn enter_polling_mode(&mut self, cx: &mut gpui::Context<Self>) {
        self.monitor_listener_gen.bump();
        self.repository_monitor = None;
        self.monitor_retrying_native = false;
        self.monitor_sequence = self.monitor_sequence.wrapping_add(1);
        self.monitor_schedule
            .enable_polling(Instant::now(), self.monitor_sequence);
        self.pending_refresh_origin = Some(crate::RefreshOrigin::Polling);
        if !self.monitor_fallback_notified {
            self.monitor_fallback_notified = true;
            self.set_status(
                "Repository watcher unavailable; using adaptive polling".to_string(),
                true,
                cx,
            );
        }
        self.schedule_repository_refresh(cx);
    }

    fn verify_native_monitor(
        &mut self,
        listener_gen: u64,
        shared: Arc<MonitorShared>,
        probe_path: PathBuf,
        cx: &mut gpui::Context<Self>,
    ) {
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(LIVENESS_SETTLE).await;
            let write_path = probe_path.clone();
            let wrote = cx
                .background_executor()
                .spawn(async move {
                    let mut file = std::fs::OpenOptions::new()
                        .create(true)
                        .truncate(true)
                        .write(true)
                        .open(write_path)?;
                    file.write_all(b"monitor liveness probe\n")?;
                    file.flush()
                })
                .await
                .is_ok();
            cx.background_executor().timer(LIVENESS_TIMEOUT).await;
            let callback_seen = shared.callback_seen();
            cx.background_executor()
                .spawn(async move {
                    let _ = std::fs::remove_file(probe_path);
                })
                .await;
            this.update(cx, |this, cx| {
                if !this.monitor_listener_gen.is_current(listener_gen)
                    || this.monitor_schedule.mode != MonitorMode::Native
                {
                    return;
                }
                if !wrote || !callback_seen {
                    // Registration can succeed even when FSEvents is silent.
                    // Delay the next native retry until a later broad poll so
                    // an unavailable backend cannot cause a reinstall loop.
                    this.monitor_schedule.mark_broad_poll(Instant::now());
                    this.enter_polling_mode(cx);
                }
            })
            .ok();
        })
        .detach();
    }

    fn receive_repository_batch(&mut self, batch: RepoChangeBatch, cx: &mut gpui::Context<Self>) {
        self.monitor_sequence = self.monitor_sequence.max(batch.sequence);
        self.monitor_schedule.ingest(batch, Instant::now());
        if self.monitor_schedule.has_pending() {
            self.pending_refresh_origin
                .get_or_insert(crate::RefreshOrigin::Monitor);
        }
        self.schedule_repository_refresh(cx);
    }

    /// Queue a focus/auto-fetch refresh through the same overlay/lock gate as
    /// native and polling work. One synthetic sequence absorbs its filesystem
    /// echoes when the resulting snapshot lands.
    pub(crate) fn request_automatic_refresh(
        &mut self,
        origin: crate::RefreshOrigin,
        scope: ChangeScope,
        batch: Option<RepoChangeBatch>,
        cx: &mut gpui::Context<Self>,
    ) {
        let batch = batch.unwrap_or_else(|| {
            let sequence = self.next_monitor_sequence();
            RepoChangeBatch {
                sequence,
                native_sequence: None,
                synthetic: true,
                scope,
                paths: Vec::new(),
                unknown: scope == ChangeScope::RepositoryWide,
            }
        });
        // Focus refresh stays available when auto-refresh is disabled.
        if self.monitor_schedule.mode == MonitorMode::Disabled {
            self.monitor_schedule.enable_native();
        }
        self.monitor_schedule.ingest(batch, Instant::now());
        if self.monitor_schedule.has_pending() {
            self.pending_refresh_origin = Some(origin);
        }
        self.schedule_repository_refresh(cx);
    }

    fn schedule_repository_refresh(&mut self, cx: &mut gpui::Context<Self>) {
        if !self.monitor_schedule.has_pending() {
            return;
        }
        let delay = self
            .monitor_schedule
            .next_event_delay(Instant::now())
            .min(CONTINUOUS_PROBE);
        let gen = self.monitor_timer_gen.bump();
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(delay).await;
            this.update(cx, |this, cx| {
                if this.monitor_timer_gen.is_current(gen) {
                    this.try_repository_refresh(cx);
                }
            })
            .ok();
        })
        .detach();
    }

    fn try_repository_refresh(&mut self, cx: &mut gpui::Context<Self>) {
        let now = Instant::now();
        if self.monitor_schedule.probe_in_flight() {
            // The landing schedules any accumulated follow-up. Re-arming a
            // timer whose debounce is already due would otherwise spin with a
            // zero delay until the active Git process exits.
            return;
        }
        if !self.monitor_schedule.pending_due(now) {
            self.schedule_repository_refresh(cx);
            return;
        }
        if self.has_refresh_blocking_overlay() {
            // Overlay time does not count toward the 30-second Git-lock force
            // window; interaction remains protected for as long as it is open.
            self.monitor_schedule.clear_blocked();
            self.schedule_repository_retry(BLOCK_RETRY, cx);
            return;
        }
        let locked = self.job_cancel.is_some() || self.external_git_lock_active();
        if locked && !self.monitor_schedule.lock_force_due(now) {
            self.monitor_schedule.note_blocked(now);
            self.schedule_repository_retry(BLOCK_RETRY, cx);
            return;
        }
        self.monitor_schedule.clear_blocked();
        let Some(mut batch) = self.monitor_schedule.begin_probe(now) else {
            return;
        };
        let default_origin = if self.monitor_schedule.mode == MonitorMode::Polling {
            if self.monitor_schedule.broad_poll_due(now) {
                batch.scope = ChangeScope::RepositoryWide;
                batch.unknown = true;
                self.monitor_schedule.mark_broad_poll(now);
                self.monitor_retrying_native = true;
            }
            crate::RefreshOrigin::Polling
        } else {
            crate::RefreshOrigin::Monitor
        };
        let origin = self.pending_refresh_origin.take().unwrap_or(default_origin);
        self.classify_ignored_and_refresh(batch, origin, cx);
    }

    /// Classify one already-debounced native path batch. Doing this after
    /// `begin_probe` means an FSEvents burst pays for one Git process per
    /// refresh window, rather than one per capacity-one wake-channel drain.
    fn classify_ignored_and_refresh(
        &mut self,
        batch: RepoChangeBatch,
        origin: crate::RefreshOrigin,
        cx: &mut gpui::Context<Self>,
    ) {
        if !batch.needs_ignore_check() {
            self.refresh_with_origin(origin, batch.scope, Some(batch), cx);
            return;
        }
        let Some(repo) = self
            .repo
            .clone()
            .map(|repo| repo.with_timeout(IGNORE_CHECK_TIMEOUT))
        else {
            self.refresh_with_origin(origin, batch.scope, Some(batch), cx);
            return;
        };
        let paths: Vec<String> = batch
            .paths
            .iter()
            .map(|path| crate::path_identity::key(path))
            .collect();
        let listener_gen = self.monitor_listener_gen.current();
        let status_gen = self.status_generation.current();
        cx.spawn(async move |this, cx| {
            let ignored = cx
                .background_executor()
                .spawn(async move { repo.check_ignored(&paths) })
                .await;
            this.update(cx, |this, cx| {
                // A monitor reconfiguration or any intervening status refresh
                // has already abandoned this probe; do not let its classifier
                // cancel the newer read by starting stale work.
                if !this.monitor_listener_gen.is_current(listener_gen)
                    || !this.status_generation.is_current(status_gen)
                {
                    return;
                }
                this.finish_ignore_classification(batch, origin, ignored, cx);
            })
            .ok();
        })
        .detach();
    }

    fn finish_ignore_classification(
        &mut self,
        mut batch: RepoChangeBatch,
        origin: crate::RefreshOrigin,
        ignored: magritte_core::Result<Vec<String>>,
        cx: &mut gpui::Context<Self>,
    ) {
        match ignored {
            Ok(ignored) => {
                if batch.retain_non_ignored(ignored) {
                    self.refresh_with_origin(origin, batch.scope, Some(batch), cx);
                } else {
                    self.monitor_schedule
                        .cover_without_probe(batch.covered_native_sequence());
                    self.monitor_schedule.abandon_probe();
                    if self.monitor_schedule.has_pending() {
                        self.schedule_repository_refresh(cx);
                    } else {
                        self.pending_refresh_origin = None;
                    }
                }
            }
            Err(_) => {
                batch.scope = ChangeScope::RepositoryWide;
                batch.unknown = true;
                self.refresh_with_origin(origin, batch.scope, Some(batch), cx);
            }
        }
    }

    fn schedule_repository_retry(&mut self, delay: Duration, cx: &mut gpui::Context<Self>) {
        let gen = self.monitor_timer_gen.bump();
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(delay).await;
            this.update(cx, |this, cx| {
                if this.monitor_timer_gen.is_current(gen) {
                    this.try_repository_refresh(cx);
                }
            })
            .ok();
        })
        .detach();
    }

    fn schedule_poll(&mut self, cx: &mut gpui::Context<Self>) {
        if self.monitor_schedule.mode != MonitorMode::Polling || !self.config.auto_refresh {
            return;
        }
        let delay = self.monitor_schedule.next_poll_delay();
        let gen = self.monitor_poll_gen.bump();
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(delay).await;
            this.update(cx, |this, cx| {
                if !this.monitor_poll_gen.is_current(gen)
                    || this.monitor_schedule.mode != MonitorMode::Polling
                {
                    return;
                }
                let sequence = this.next_monitor_sequence();
                let batch = RepoChangeBatch {
                    sequence,
                    native_sequence: None,
                    synthetic: true,
                    scope: ChangeScope::StatusOnly,
                    paths: Vec::new(),
                    unknown: true,
                };
                this.monitor_schedule.ingest(batch, Instant::now());
                this.pending_refresh_origin = Some(crate::RefreshOrigin::Polling);
                this.schedule_repository_refresh(cx);
            })
            .ok();
        })
        .detach();
    }

    pub(crate) fn has_refresh_blocking_overlay(&self) -> bool {
        self.popup.is_some() || self.confirm.is_some() || self.ctx_menu_open
    }

    pub(crate) fn refresh_blocker_closed(&mut self, cx: &mut gpui::Context<Self>) {
        if !self.has_refresh_blocking_overlay() && self.monitor_schedule.has_pending() {
            self.schedule_repository_retry(Duration::ZERO, cx);
        }
    }

    fn external_git_lock_active(&self) -> bool {
        let Some(git_dir) = self.worktree_git_dir.as_ref() else {
            return false;
        };
        let common = self
            .repo_scope_dir
            .as_ref()
            .and_then(|scope| scope.parent())
            .unwrap_or(git_dir);
        [
            git_dir.join("index.lock"),
            git_dir.join("HEAD.lock"),
            common.join("packed-refs.lock"),
            common.join("shallow.lock"),
        ]
        .iter()
        .any(|path| path.exists())
            || lock_under(&common.join("refs"), 4)
    }

    fn next_monitor_sequence(&mut self) -> u64 {
        let sequence = self
            .repository_monitor
            .as_ref()
            .map(RepositoryMonitor::next_sequence)
            .unwrap_or_else(|| self.monitor_sequence.wrapping_add(1));
        self.monitor_sequence = self.monitor_sequence.max(sequence);
        sequence
    }

    pub(crate) fn finish_repository_refresh(
        &mut self,
        context: &crate::StatusRefreshContext,
        changed: bool,
        elapsed: Duration,
        success: bool,
        cx: &mut gpui::Context<Self>,
    ) {
        let owns_monitor_probe = context.origin.owns_monitor_probe(context.batch.is_some());
        if success {
            if owns_monitor_probe {
                self.monitor_schedule.finish_probe(
                    Instant::now(),
                    changed,
                    elapsed,
                    context.covered_sequence,
                );
            } else {
                self.monitor_schedule
                    .cover_without_probe(context.covered_sequence);
                self.monitor_schedule.abandon_probe();
            }
        } else if owns_monitor_probe {
            self.monitor_schedule.abandon_probe();
            if let Some(batch) = context.batch.clone() {
                self.monitor_schedule
                    .retry_probe(batch, Instant::now(), elapsed);
                if self.monitor_schedule.has_pending() {
                    self.pending_refresh_origin =
                        Some(if context.origin == crate::RefreshOrigin::Initial {
                            crate::RefreshOrigin::Monitor
                        } else {
                            context.origin
                        });
                }
            }
        } else {
            self.monitor_schedule.abandon_probe();
        }

        if self.monitor_schedule.mode == MonitorMode::Native
            && self
                .repository_monitor
                .as_ref()
                .is_some_and(|monitor| !monitor.mandatory_roots_exist())
        {
            self.enter_polling_mode(cx);
            return;
        }

        if success
            && context.scope == ChangeScope::RepositoryWide
            && self.monitor_schedule.mode == MonitorMode::Native
            && self
                .repository_monitor
                .as_ref()
                .is_some_and(|monitor| !monitor.optional_roots_current())
        {
            self.configure_repository_monitor(cx);
            return;
        }

        if self.monitor_schedule.has_pending() {
            self.schedule_repository_refresh(cx);
        } else if self.monitor_schedule.mode == MonitorMode::Polling {
            if success && self.monitor_retrying_native {
                // A successful broad reconciliation is the retry point for
                // native registration. If it still fails, configuration drops
                // straight back into polling.
                self.configure_repository_monitor(cx);
            } else {
                self.schedule_poll(cx);
            }
        } else {
            self.pending_refresh_origin = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn batch(sequence: u64, path: &str) -> RepoChangeBatch {
        RepoChangeBatch {
            sequence,
            native_sequence: Some(sequence),
            synthetic: false,
            scope: ChangeScope::StatusOnly,
            paths: vec![PathBuf::from(path)],
            unknown: false,
        }
    }

    #[test]
    fn accumulator_is_bounded_and_escalates_scope() {
        let mut pending = PendingBatch::default();
        for sequence in 1..=300 {
            let mut next = batch(sequence, &format!("p{sequence}"));
            if sequence == 200 {
                next.scope = ChangeScope::RepositoryWide;
            }
            pending.merge(next);
        }
        let batch = pending.take().unwrap();
        assert_eq!(batch.paths.len(), PATH_CAP);
        assert!(batch.unknown);
        assert_eq!(batch.scope, ChangeScope::RepositoryWide);
        assert_eq!(batch.sequence, 300);
    }

    #[test]
    fn repeated_hundred_thousand_event_burst_keeps_constant_path_memory() {
        let mut pending = PendingBatch::default();
        for sequence in 1..=100_000 {
            pending.merge(batch(sequence, "same-path"));
        }
        let batch = pending.take().unwrap();
        assert_eq!(batch.sequence, 100_000);
        assert_eq!(batch.paths, vec![PathBuf::from("same-path")]);
        assert!(!batch.unknown);
    }

    #[test]
    fn capacity_one_wake_cannot_lose_merged_data() {
        let shared = Arc::new(MonitorShared {
            pending: Mutex::new(PendingBatch::default()),
            sequence: AtomicU64::new(0),
            callback_seen: AtomicBool::new(false),
        });
        let (tx, rx) = async_channel::bounded(1);
        shared.merge(batch(1, "one"));
        tx.try_send(()).unwrap();
        shared.merge(batch(2, "two"));
        assert!(tx.try_send(()).is_err());
        assert!(rx.try_recv().is_ok());
        let merged = shared.take().unwrap();
        assert_eq!(merged.sequence, 2);
        assert_eq!(
            merged.paths,
            vec![PathBuf::from("one"), PathBuf::from("two")]
        );
    }

    #[test]
    fn callback_liveness_is_recorded_even_without_a_relevant_batch() {
        let shared = MonitorShared {
            pending: Mutex::new(PendingBatch::default()),
            sequence: AtomicU64::new(0),
            callback_seen: AtomicBool::new(false),
        };
        assert!(!shared.callback_seen());
        shared.note_callback();
        assert!(shared.callback_seen());
        assert!(shared.take().is_none());
    }

    #[test]
    fn debounce_and_continuous_cap_coalesce_events() {
        let start = Instant::now();
        let mut schedule = MonitorSchedule::default();
        schedule.enable_native();
        schedule.ingest(batch(1, "a"), start);
        assert!(!schedule.pending_due(start + Duration::from_millis(299)));
        schedule.ingest(batch(2, "b"), start + Duration::from_millis(250));
        assert!(!schedule.pending_due(start + Duration::from_millis(549)));
        assert!(schedule.pending_due(start + Duration::from_secs(1)));
        let merged = schedule
            .begin_probe(start + Duration::from_secs(1))
            .unwrap();
        assert_eq!(merged.paths.len(), 2);
    }

    #[test]
    fn unchanged_probes_back_off_and_changes_reset() {
        let start = Instant::now();
        let mut schedule = MonitorSchedule::default();
        schedule.enable_native();
        schedule.ingest(batch(1, "a"), start);
        schedule.ingest(batch(2, "a"), start + Duration::from_millis(900));
        schedule.begin_probe(start + CONTINUOUS_PROBE).unwrap();
        schedule.finish_probe(
            start + CONTINUOUS_PROBE,
            false,
            Duration::from_millis(100),
            2,
        );
        schedule.ingest(batch(3, "a"), start + Duration::from_millis(1100));
        assert!(!schedule.pending_due(start + Duration::from_millis(2500)));
        assert!(schedule.pending_due(start + Duration::from_secs(3)));
        schedule
            .begin_probe(start + Duration::from_secs(3))
            .unwrap();
        schedule.finish_probe(
            start + Duration::from_millis(3100),
            true,
            Duration::from_millis(100),
            3,
        );
        schedule.ingest(batch(4, "a"), start + Duration::from_millis(3200));
        assert!(schedule.pending_due(start + Duration::from_millis(3500)));
    }

    #[test]
    fn failed_probe_retains_the_batch_with_bounded_backoff() {
        let start = Instant::now();
        let mut schedule = MonitorSchedule::default();
        schedule.enable_native();
        schedule.ingest(batch(1, "a"), start);
        let pending = schedule.begin_probe(start + QUIET_DELAY).unwrap();

        schedule.retry_probe(
            pending,
            start + Duration::from_millis(400),
            Duration::from_millis(100),
        );

        assert!(schedule.has_pending());
        assert!(!schedule.pending_due(start + Duration::from_secs(2)));
        assert!(schedule.pending_due(start + Duration::from_millis(2400)));
    }

    #[test]
    fn sequence_coverage_leaves_exactly_one_late_follow_up() {
        let start = Instant::now();
        let mut schedule = MonitorSchedule::default();
        schedule.enable_native();
        schedule.ingest(batch(1, "a"), start);
        schedule.begin_probe(start + QUIET_DELAY).unwrap();
        schedule.ingest(batch(2, "b"), start + Duration::from_millis(350));
        schedule.finish_probe(
            start + Duration::from_millis(400),
            true,
            Duration::from_millis(100),
            1,
        );
        assert!(schedule.has_pending());
        let follow = schedule
            .begin_probe(start + Duration::from_millis(700))
            .unwrap();
        assert_eq!(follow.sequence, 2);
        schedule.finish_probe(
            start + Duration::from_millis(800),
            false,
            Duration::from_millis(100),
            2,
        );
        assert!(!schedule.has_pending());
        assert_eq!(schedule.covered_sequence(), 2);
    }

    #[test]
    fn synthetic_refresh_cannot_cover_a_future_native_event() {
        let start = Instant::now();
        let mut schedule = MonitorSchedule::default();
        schedule.enable_native();
        schedule.ingest(RepoChangeBatch::repository_wide(50), start);
        let synthetic = schedule.begin_probe(start + QUIET_DELAY).unwrap();
        assert_eq!(synthetic.covered_native_sequence(), 0);
        schedule.finish_probe(
            start + Duration::from_millis(400),
            false,
            Duration::from_millis(100),
            synthetic.covered_native_sequence(),
        );

        schedule.ingest(batch(1, "later"), start + Duration::from_millis(500));
        assert!(schedule.has_pending());
    }

    #[test]
    fn ignored_probe_coverage_does_not_erase_a_later_native_event() {
        let start = Instant::now();
        let mut schedule = MonitorSchedule::default();
        schedule.enable_native();
        schedule.ingest(batch(1, "ignored"), start);
        let ignored = schedule.begin_probe(start + QUIET_DELAY).unwrap();
        schedule.ingest(batch(2, "later"), start + Duration::from_millis(350));

        schedule.cover_without_probe(ignored.covered_native_sequence());
        schedule.abandon_probe();

        assert!(schedule.has_pending());
        let pending = schedule
            .begin_probe(start + Duration::from_millis(650))
            .unwrap();
        assert_eq!(pending.paths, vec![PathBuf::from("later")]);
        assert_eq!(pending.covered_native_sequence(), 2);
    }

    #[test]
    fn ignore_filter_applies_once_to_the_debounced_merged_probe() {
        let start = Instant::now();
        let mut schedule = MonitorSchedule::default();
        schedule.enable_native();
        schedule.ingest(batch(1, "ignored"), start);
        schedule.ingest(batch(2, "kept"), start + Duration::from_millis(100));

        let mut merged = schedule
            .begin_probe(start + Duration::from_millis(400))
            .unwrap();
        assert!(merged.needs_ignore_check());
        assert!(merged.retain_non_ignored(vec!["ignored".into()]));
        assert_eq!(merged.paths, vec![PathBuf::from("kept")]);
        assert_eq!(merged.covered_native_sequence(), 2);
    }

    #[test]
    fn unknown_path_overflow_skips_ignore_classification() {
        let start = Instant::now();
        let mut schedule = MonitorSchedule::default();
        schedule.enable_native();
        for sequence in 1..=PATH_CAP as u64 + 1 {
            schedule.ingest(batch(sequence, &format!("p{sequence}")), start);
        }

        let merged = schedule.begin_probe(start + QUIET_DELAY).unwrap();
        assert!(merged.unknown);
        assert!(!merged.needs_ignore_check());
    }

    #[test]
    fn worktree_root_event_becomes_an_unknown_status_batch() {
        let roots = WatchRoots {
            worktree: PathBuf::from("/repo"),
            git_dir: PathBuf::from("/repo/.git"),
            common_dir: PathBuf::from("/repo/.git"),
        };
        let event = notify::Event::new(notify::EventKind::Modify(notify::event::ModifyKind::Any))
            .add_path(PathBuf::from("/repo"));

        let batch = normalize_event(&roots, 1, event).unwrap();
        assert_eq!(batch.scope, ChangeScope::StatusOnly);
        assert!(batch.paths.is_empty());
        assert!(batch.unknown);
    }

    #[test]
    fn index_event_is_an_unknown_status_batch_instead_of_being_dropped() {
        let roots = WatchRoots {
            worktree: PathBuf::from("/repo"),
            git_dir: PathBuf::from("/repo/.git"),
            common_dir: PathBuf::from("/repo/.git"),
        };
        let event = notify::Event::new(notify::EventKind::Modify(notify::event::ModifyKind::Data(
            notify::event::DataChange::Any,
        )))
        .add_path(PathBuf::from("/repo/.git/index"));

        let batch = normalize_event(&roots, 1, event).unwrap();
        assert_eq!(batch.scope, ChangeScope::StatusOnly);
        assert!(batch.paths.is_empty());
        assert!(batch.unknown);
    }

    #[test]
    fn startup_reconciliation_owns_the_probe_and_retains_one_follow_up() {
        let start = Instant::now();
        let mut schedule = MonitorSchedule::default();
        schedule.enable_native();
        schedule.ingest(RepoChangeBatch::repository_wide(1), start);

        let initial = schedule.begin_probe(start).unwrap();
        assert!(schedule.probe_in_flight());
        assert!(!schedule.has_pending());
        schedule.ingest(batch(2, "changed"), start + Duration::from_millis(10));
        assert!(schedule.has_pending());

        schedule.finish_probe(
            start + Duration::from_millis(100),
            true,
            Duration::from_millis(100),
            initial.covered_native_sequence(),
        );
        assert!(!schedule.probe_in_flight());
        assert!(schedule.has_pending());
    }

    #[test]
    fn probes_never_overlap_and_lock_deferral_is_bounded() {
        let start = Instant::now();
        let mut schedule = MonitorSchedule::default();
        schedule.enable_native();
        schedule.ingest(batch(1, "a"), start);
        assert!(schedule.begin_probe(start + QUIET_DELAY).is_some());
        assert!(schedule.begin_probe(start + QUIET_DELAY).is_none());
        schedule.finish_probe(
            start + Duration::from_millis(400),
            false,
            Duration::from_millis(100),
            1,
        );
        schedule.ingest(batch(2, "b"), start + Duration::from_secs(1));
        schedule.note_blocked(start + Duration::from_secs(3));
        assert!(!schedule.lock_force_due(start + Duration::from_secs(32)));
        assert!(schedule.lock_force_due(start + Duration::from_secs(33)));
    }

    #[test]
    fn polling_scales_with_status_duration_and_broad_reconciliation() {
        let start = Instant::now();
        let mut schedule = MonitorSchedule::default();
        schedule.enable_polling(start, 1);
        schedule.begin_probe(start).unwrap();
        schedule.finish_probe(
            start + Duration::from_secs(1),
            false,
            Duration::from_secs(3),
            1,
        );
        assert_eq!(schedule.next_poll_delay(), Duration::from_secs(30));
        assert!(schedule.broad_poll_due(start + Duration::from_secs(1)));
        schedule.mark_broad_poll(start + Duration::from_secs(1));
        assert!(!schedule.broad_poll_due(start + Duration::from_secs(60)));
        assert!(schedule.broad_poll_due(start + Duration::from_secs(61)));
    }

    #[test]
    fn disable_drops_pending_and_reenable_reconciles() {
        let start = Instant::now();
        let mut schedule = MonitorSchedule::default();
        schedule.enable_native();
        schedule.ingest(batch(1, "a"), start);
        schedule.disable();
        assert!(!schedule.has_pending());
        schedule.enable_polling(start, 2);
        assert!(schedule.has_pending());
    }
}
