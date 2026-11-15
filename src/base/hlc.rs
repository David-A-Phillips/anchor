//! Hybrid logical clock.
//!
//! Units are versioned with an [`Hlc`] so that "last writer wins" is well defined across devices
//! whose wall clocks disagree. A timestamp packs 48 bits of milliseconds since the Unix epoch and a
//! 16-bit logical counter into one `u64`, so it sorts correctly as a plain integer (including as a
//! SQLite `INTEGER` column).

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use super::time::now_ms;

const COUNTER_BITS: u32 = 16;
const COUNTER_MASK: u64 = (1 << COUNTER_BITS) - 1;

/// A hybrid logical timestamp. Totally ordered; ties between devices are broken by the caller with
/// the origin [`DeviceId`](super::DeviceId).
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct Hlc(u64);

impl Hlc {
    /// The smallest timestamp; older than anything a clock produces.
    pub const ZERO: Hlc = Hlc(0);

    /// Builds a timestamp from its parts. `wall_ms` is truncated to 48 bits.
    pub const fn new(wall_ms: u64, counter: u16) -> Self {
        Self((wall_ms << COUNTER_BITS) | counter as u64)
    }

    /// Reconstructs a timestamp from its packed representation.
    pub const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    /// Packed representation.
    pub const fn raw(self) -> u64 {
        self.0
    }

    /// Packed representation as `i64`, for SQLite. Order-preserving for every realistic timestamp
    /// (the sign bit is only reached in the year 6429).
    pub const fn as_i64(self) -> i64 {
        self.0 as i64
    }

    /// Inverse of [`Self::as_i64`].
    pub const fn from_i64(raw: i64) -> Self {
        Self(raw as u64)
    }

    /// Milliseconds since the Unix epoch.
    pub const fn wall_ms(self) -> u64 {
        self.0 >> COUNTER_BITS
    }

    /// Logical counter within the millisecond.
    pub const fn counter(self) -> u16 {
        (self.0 & COUNTER_MASK) as u16
    }

    /// The next timestamp strictly greater than `self`.
    pub const fn successor(self) -> Self {
        Self(self.0.saturating_add(1))
    }
}

impl std::fmt::Display for Hlc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}", self.wall_ms(), self.counter())
    }
}

/// A monotonic source of [`Hlc`] timestamps for one device.
#[derive(Debug, Default)]
pub struct HlcClock {
    last: Mutex<Hlc>,
}

impl HlcClock {
    /// Creates a clock that has not observed anything yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a clock that will never issue a timestamp at or below `floor`
    /// (use the greatest timestamp persisted locally when restarting).
    pub fn starting_after(floor: Hlc) -> Self {
        Self {
            last: Mutex::new(floor),
        }
    }

    /// Issues a timestamp strictly greater than every timestamp issued or observed so far.
    pub fn now(&self) -> Hlc {
        self.tick(now_ms().max(0) as u64)
    }

    /// Like [`Self::now`] with an explicit wall-clock reading; deterministic, for tests.
    pub fn tick(&self, wall_ms: u64) -> Hlc {
        let mut last = self.last.lock();
        let candidate = Hlc::new(wall_ms, 0);
        let next = if candidate > *last {
            candidate
        } else {
            last.successor()
        };
        *last = next;
        next
    }

    /// Records a timestamp received from another device so later local timestamps exceed it.
    pub fn observe(&self, remote: Hlc) {
        let mut last = self.last.lock();
        if remote > *last {
            *last = remote;
        }
    }

    /// The greatest timestamp issued or observed so far.
    pub fn latest(&self) -> Hlc {
        *self.last.lock()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packs_and_orders() {
        let a = Hlc::new(1_700_000_000_000, 0);
        let b = Hlc::new(1_700_000_000_000, 1);
        let c = Hlc::new(1_700_000_000_001, 0);
        assert!(a < b && b < c);
        assert_eq!(b.wall_ms(), 1_700_000_000_000);
        assert_eq!(b.counter(), 1);
        assert_eq!(Hlc::from_i64(c.as_i64()), c);
        assert!(a.as_i64() < c.as_i64());
    }

    #[test]
    fn clock_is_strictly_monotonic_even_when_wall_clock_stalls_or_regresses() {
        let clock = HlcClock::new();
        let first = clock.tick(1_000);
        let second = clock.tick(1_000);
        let third = clock.tick(900);
        assert!(first < second && second < third);
        assert_eq!(third.wall_ms(), 1_000);
        assert_eq!(third.counter(), 2);
    }

    #[test]
    fn counter_overflow_rolls_into_wall_time() {
        let clock = HlcClock::starting_after(Hlc::new(5, u16::MAX));
        let next = clock.tick(5);
        assert_eq!((next.wall_ms(), next.counter()), (6, 0));
    }

    #[test]
    fn observe_pushes_clock_forward() {
        let clock = HlcClock::new();
        let remote = Hlc::new(9_999_999_999_999, 7);
        clock.observe(remote);
        assert!(clock.tick(1) > remote);
        assert!(clock.now() > remote);
    }
}
