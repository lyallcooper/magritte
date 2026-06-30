//! A monotonic stamp for discarding stale async work.
//!
//! Several bits of [`StatusView`](crate::StatusView) spawn background work
//! (status/diff reads, screen loads, picker candidate fetches) or schedule a
//! delayed action (a prefix-sequence timeout, a status-message auto-dismiss)
//! whose result must be ignored if something newer superseded it in the
//! meantime. The pattern is always the same: [`bump`](Generation::bump) the
//! counter before starting and capture the returned stamp; when the work
//! lands, keep it only if the stamp is still [`current`](Generation::current).

/// A monotonic counter handed out as opaque `u64` stamps. `bump` before
/// starting async work and capture the stamp; check [`is_current`] (or compare
/// against [`current`]) when it lands to drop superseded results.
///
/// [`is_current`]: Generation::is_current
/// [`current`]: Generation::current
#[derive(Default)]
pub(crate) struct Generation(u64);

impl Generation {
    /// Advance the counter and return the new current stamp.
    pub(crate) fn bump(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(1);
        self.0
    }

    /// The current stamp, for capturing or comparing against.
    pub(crate) fn current(&self) -> u64 {
        self.0
    }

    /// Whether `stamp` is still the current one (nothing has bumped since).
    pub(crate) fn is_current(&self, stamp: u64) -> bool {
        self.0 == stamp
    }
}
