//! Small runtime-observable metric primitives shared across the crate.
//!
//! [`Counter`] wraps an [`AtomicU64`] so agent-side counters (single
//! owner) and transport-side counters (shared across [`Arc`] clones
//! and `tokio::spawn` tasks) present the same API. The wrapper picks
//! `Ordering::Relaxed` for every operation — the counters are pure
//! accumulators and have no cross-thread happens-before requirement.
//!
//! The module intentionally starts with just `Counter`; leave room to
//! grow `Gauge` / `Histogram` here if a later feature needs them.
//!
//! [`Arc`]: std::sync::Arc

use std::sync::atomic::{AtomicU64, Ordering};

/// A monotonically-increasing counter backed by an [`AtomicU64`].
///
/// # Example
///
/// ```
/// use attini::Counter;
///
/// let c = Counter::new();
/// assert_eq!(c.get(), 0);
/// c.inc();
/// c.add(4);
/// assert_eq!(c.get(), 5);
/// ```
#[derive(Debug, Default)]
pub struct Counter(AtomicU64);

impl Counter {
    /// Construct a new counter starting at `0`.
    pub const fn new() -> Self {
        Self(AtomicU64::new(0))
    }

    /// Read the current value.
    ///
    /// The read is a `Relaxed` atomic load and returns a plain `u64`
    /// snapshot suitable for `assert_eq!` and arithmetic.
    pub fn get(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }

    /// Increment by one.
    pub fn inc(&self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }

    /// Add `n` to the current value.
    ///
    /// Use for counters that track sums (bytes transferred, items
    /// processed) rather than single events. On overflow the value
    /// wraps per [`AtomicU64::fetch_add`] semantics; in practice
    /// `u64::MAX` is unreachable for the counters this crate publishes.
    pub fn add(&self, n: u64) {
        self.0.fetch_add(n, Ordering::Relaxed);
    }
}

impl Clone for Counter {
    /// Snapshot the current value into an independent counter.
    ///
    /// The returned counter starts at the source's `get()` at the
    /// time of cloning. Later mutations on either counter do not
    /// affect the other; use [`std::sync::Arc<Counter>`] wrapping if
    /// you need shared state.
    fn clone(&self) -> Self {
        Self(AtomicU64::new(self.get()))
    }
}

impl PartialEq for Counter {
    fn eq(&self, other: &Self) -> bool {
        self.get() == other.get()
    }
}

impl Eq for Counter {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_zero() {
        assert_eq!(Counter::default().get(), 0);
    }

    #[test]
    fn inc_bumps_by_one() {
        let c = Counter::new();
        c.inc();
        c.inc();
        c.inc();
        assert_eq!(c.get(), 3);
    }

    #[test]
    fn add_bumps_by_n() {
        let c = Counter::new();
        c.add(10);
        c.add(7);
        assert_eq!(c.get(), 17);
    }

    #[test]
    fn clone_is_independent_snapshot() {
        let a = Counter::new();
        a.inc();
        let b = a.clone();
        assert_eq!(a.get(), 1);
        assert_eq!(b.get(), 1);
        a.inc();
        b.inc();
        b.inc();
        assert_eq!(a.get(), 2);
        assert_eq!(b.get(), 3);
    }

    #[test]
    fn partial_eq_uses_current_values() {
        let a = Counter::new();
        let b = Counter::new();
        assert_eq!(a, b);
        a.inc();
        assert_ne!(a, b);
        b.inc();
        assert_eq!(a, b);
    }
}
