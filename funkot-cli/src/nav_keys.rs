//! Left/right cursor multi-press coalescing for skip / rewind.
//!
//! Window is 500ms (common double-click / multi-tap interval). Same-direction
//! taps accumulate; a direction change or timeout flushes the prior group.

use std::time::{Duration, Instant};

use funkot_core::engine::NavAction;

/// Standard multi-tap / double-click coalescing window.
pub const MULTI_PRESS_WINDOW: Duration = Duration::from_millis(500);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavDir {
    Left,
    Right,
}

/// Coalesce rapid left/right taps into a single [`NavAction`].
#[derive(Debug, Default)]
pub struct MultiPressAggregator {
    dir: Option<NavDir>,
    count: u32,
    last: Option<Instant>,
}

impl MultiPressAggregator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a tap. Returns a completed action when the direction changes
    /// (flushing the previous group before starting the new one).
    pub fn press(&mut self, dir: NavDir, now: Instant) -> Option<NavAction> {
        match self.dir {
            None => {
                self.dir = Some(dir);
                self.count = 1;
                self.last = Some(now);
                None
            }
            Some(prev) if prev == dir => {
                if let Some(last) = self.last {
                    if now.duration_since(last) > MULTI_PRESS_WINDOW {
                        let action = self.take_action();
                        self.dir = Some(dir);
                        self.count = 1;
                        self.last = Some(now);
                        return action;
                    }
                }
                self.count = self.count.saturating_add(1);
                self.last = Some(now);
                None
            }
            Some(_) => {
                let action = self.take_action();
                self.dir = Some(dir);
                self.count = 1;
                self.last = Some(now);
                action
            }
        }
    }

    /// If the window has elapsed since the last tap, flush the pending group.
    pub fn poll_timeout(&mut self, now: Instant) -> Option<NavAction> {
        let last = self.last?;
        if self.dir.is_none() || self.count == 0 {
            return None;
        }
        if now.duration_since(last) >= MULTI_PRESS_WINDOW {
            self.take_action()
        } else {
            None
        }
    }

    /// Force-flush any pending taps (e.g. on shutdown).
    pub fn flush(&mut self) -> Option<NavAction> {
        self.take_action()
    }

    fn take_action(&mut self) -> Option<NavAction> {
        let dir = self.dir.take()?;
        let count = self.count;
        self.count = 0;
        self.last = None;
        if count == 0 {
            return None;
        }
        Some(action_for(dir, count))
    }
}

/// Map a coalesced tap count to a nav action (left ≥3 → jump prev, right ≥2 → jump next).
pub fn action_for(dir: NavDir, count: u32) -> NavAction {
    match dir {
        NavDir::Left => match count {
            0 => NavAction::RestartCurrent, // unreachable via take_action
            1 => NavAction::RestartCurrent,
            2 => NavAction::TransitionToPrev,
            _ => NavAction::JumpToPrevIntro,
        },
        NavDir::Right => match count {
            0 | 1 => NavAction::TransitionToNext,
            _ => NavAction::JumpToNextIntro,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t0() -> Instant {
        Instant::now()
    }

    #[test]
    fn left_counts_map() {
        assert_eq!(action_for(NavDir::Left, 1), NavAction::RestartCurrent);
        assert_eq!(action_for(NavDir::Left, 2), NavAction::TransitionToPrev);
        assert_eq!(action_for(NavDir::Left, 3), NavAction::JumpToPrevIntro);
        assert_eq!(action_for(NavDir::Left, 9), NavAction::JumpToPrevIntro);
    }

    #[test]
    fn right_counts_map() {
        assert_eq!(action_for(NavDir::Right, 1), NavAction::TransitionToNext);
        assert_eq!(action_for(NavDir::Right, 2), NavAction::JumpToNextIntro);
        assert_eq!(action_for(NavDir::Right, 5), NavAction::JumpToNextIntro);
    }

    #[test]
    fn coalesces_within_window() {
        let mut a = MultiPressAggregator::new();
        let t = t0();
        assert!(a.press(NavDir::Left, t).is_none());
        assert!(a.press(NavDir::Left, t + Duration::from_millis(100)).is_none());
        assert!(a.press(NavDir::Left, t + Duration::from_millis(200)).is_none());
        assert_eq!(
            a.poll_timeout(t + Duration::from_millis(700)),
            Some(NavAction::JumpToPrevIntro)
        );
    }

    #[test]
    fn direction_change_flushes() {
        let mut a = MultiPressAggregator::new();
        let t = t0();
        assert!(a.press(NavDir::Left, t).is_none());
        assert!(a.press(NavDir::Left, t + Duration::from_millis(50)).is_none());
        assert_eq!(
            a.press(NavDir::Right, t + Duration::from_millis(100)),
            Some(NavAction::TransitionToPrev)
        );
        assert_eq!(
            a.poll_timeout(t + Duration::from_millis(700)),
            Some(NavAction::TransitionToNext)
        );
    }

    #[test]
    fn gap_beyond_window_splits_groups() {
        let mut a = MultiPressAggregator::new();
        let t = t0();
        assert!(a.press(NavDir::Right, t).is_none());
        assert_eq!(
            a.press(NavDir::Right, t + Duration::from_millis(600)),
            Some(NavAction::TransitionToNext)
        );
        assert_eq!(
            a.poll_timeout(t + Duration::from_millis(1200)),
            Some(NavAction::TransitionToNext)
        );
    }
}
