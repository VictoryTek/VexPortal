//! Idle shutdown.
//!
//! The daemon is a root process on the system bus; there is no reason for it to stay
//! resident while nobody is using the portal. systemd starts it again on the next
//! call, so exiting when idle costs a few hundred milliseconds and removes a
//! long-lived privileged process from the machine.

use std::time::{Duration, Instant};

pub struct IdleTracker {
    timeout: Duration,
    last_activity: Instant,
}

impl IdleTracker {
    pub fn new(timeout: Duration) -> Self {
        Self {
            timeout,
            last_activity: Instant::now(),
        }
    }

    pub fn mark_active(&mut self) {
        self.last_activity = Instant::now();
    }

    pub fn is_idle(&self) -> bool {
        self.last_activity.elapsed() >= self.timeout
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_tracker_is_not_idle() {
        assert!(!IdleTracker::new(Duration::from_secs(60)).is_idle());
    }

    #[test]
    fn a_zero_timeout_is_immediately_idle() {
        assert!(IdleTracker::new(Duration::ZERO).is_idle());
    }

    #[test]
    fn activity_resets_the_clock() {
        let mut tracker = IdleTracker::new(Duration::from_secs(60));
        tracker.last_activity = Instant::now() - Duration::from_secs(120);
        assert!(tracker.is_idle());
        tracker.mark_active();
        assert!(!tracker.is_idle());
    }
}
