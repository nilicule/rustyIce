//! Exponential backoff used by the relay reconnect loop. Starts at 1 s,
//! doubles on each failed attempt, capped at 30 s. Reset after a successful
//! connect so a flaky upstream that reconnects within seconds doesn't slowly
//! drift into the cap.

use std::time::Duration;

pub struct Backoff {
    current: Duration,
    max: Duration,
}

impl Backoff {
    #[must_use]
    pub fn new() -> Self {
        Self {
            current: Duration::from_secs(1),
            max: Duration::from_secs(30),
        }
    }

    /// Returns the current delay, then doubles the internal value for the
    /// next call (clamped to `max`).
    pub fn next_delay(&mut self) -> Duration {
        let d = self.current;
        self.current = (self.current * 2).min(self.max);
        d
    }

    /// Reset to the starting delay. Call this after a successful connect.
    pub fn reset(&mut self) {
        self.current = Duration::from_secs(1);
    }
}

impl Default for Backoff {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_delay_doubles_and_caps_at_30s() {
        let mut b = Backoff::new();
        assert_eq!(b.next_delay(), Duration::from_secs(1));
        assert_eq!(b.next_delay(), Duration::from_secs(2));
        assert_eq!(b.next_delay(), Duration::from_secs(4));
        assert_eq!(b.next_delay(), Duration::from_secs(8));
        assert_eq!(b.next_delay(), Duration::from_secs(16));
        assert_eq!(b.next_delay(), Duration::from_secs(30));
        assert_eq!(b.next_delay(), Duration::from_secs(30));
    }

    #[test]
    fn reset_returns_to_1s() {
        let mut b = Backoff::new();
        let _ = b.next_delay();
        let _ = b.next_delay();
        let _ = b.next_delay();
        b.reset();
        assert_eq!(b.next_delay(), Duration::from_secs(1));
    }
}
