//! Where the UI reads the time. `App` holds a [`Clock`], reads it once per
//! action and passes `now` / `wall` down, so everything time-based (toasts,
//! chords, the help timeout, the filter debounce, Pub/Sub rates) can be
//! tested with a [`FixedClock`]. The Redis tasks keep reading the system
//! time for their own I/O timestamps.

use std::time::{Duration, Instant, SystemTime};

use parking_lot::Mutex;

pub trait Clock: Send + Sync {
    /// Monotonic time, for timeouts and rates.
    fn now(&self) -> Instant;
    /// Wall-clock time, for what is shown as a time of day.
    fn wall(&self) -> SystemTime;
}

/// The real time.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }

    fn wall(&self) -> SystemTime {
        SystemTime::now()
    }
}

/// A clock that moves only when told to (tests and golden frames).
#[derive(Debug)]
pub struct FixedClock {
    at: Mutex<(Instant, SystemTime)>,
}

impl FixedClock {
    /// Stopped at `wall`. `Instant`s can't be built from a number, so the
    /// monotonic side starts at the moment of creation; only differences
    /// between its readings matter.
    #[must_use]
    pub fn new(wall: SystemTime) -> Self {
        Self {
            at: Mutex::new((Instant::now(), wall)),
        }
    }

    /// Move both readings forward by `by`.
    pub fn advance(&self, by: Duration) {
        let mut at = self.at.lock();
        at.0 += by;
        at.1 += by;
    }
}

impl Clock for FixedClock {
    fn now(&self) -> Instant {
        self.at.lock().0
    }

    fn wall(&self) -> SystemTime {
        self.at.lock().1
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::UNIX_EPOCH;

    #[test]
    fn fixed_clock_moves_only_when_advanced() {
        let wall = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let clock = FixedClock::new(wall);
        let start = clock.now();
        assert_eq!(clock.now(), start);
        assert_eq!(clock.wall(), wall);

        clock.advance(Duration::from_secs(5));
        assert_eq!(clock.now() - start, Duration::from_secs(5));
        assert_eq!(clock.wall(), wall + Duration::from_secs(5));
    }
}
