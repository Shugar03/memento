//! Injectable clock for retention-sweep tests (REQ-ML-003, design D5).
//!
//! Application code that computes "now" must accept a clock so tests can
//! advance time without sleeping. [`TestClock`] is that fake: it starts at a
//! fixed instant and only moves when the test says so.

use chrono::{DateTime, Duration, Utc};
use std::sync::{Arc, Mutex};

/// A mutable clock whose `now()` is controlled by the test.
#[derive(Debug, Clone)]
pub struct TestClock {
    inner: Arc<Mutex<DateTime<Utc>>>,
}

impl TestClock {
    /// Start the clock at a fixed instant.
    pub fn new(start: DateTime<Utc>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(start)),
        }
    }

    /// The clock's current time.
    pub fn now(&self) -> DateTime<Utc> {
        *self.inner.lock().expect("test clock poisoned")
    }

    /// Advance the clock by `by` (positive or negative).
    pub fn advance(&self, by: Duration) {
        *self.inner.lock().expect("test clock poisoned") += by;
    }

    /// Jump the clock to an absolute instant.
    pub fn set(&self, at: DateTime<Utc>) {
        *self.inner.lock().expect("test clock poisoned") = at;
    }
}

impl Default for TestClock {
    fn default() -> Self {
        Self::new(Utc::now())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_advances_and_jumps() {
        let start = Utc::now();
        let clock = TestClock::new(start);
        assert_eq!(clock.now(), start);

        clock.advance(Duration::days(30));
        assert_eq!(clock.now(), start + Duration::days(30));

        clock.set(start);
        assert_eq!(clock.now(), start);
    }

    #[test]
    fn cloned_clocks_share_state() {
        let clock = TestClock::new(Utc::now());
        let clone = clock.clone();
        clone.advance(Duration::hours(1));
        assert_eq!(clock.now(), clone.now(), "clone must see the same time");
    }
}
