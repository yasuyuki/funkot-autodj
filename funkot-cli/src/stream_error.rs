//! Bounded reporting for cpal audio-stream errors.
//!
//! cpal's ALSA worker retries a failing device with **no backoff**: a generic
//! (non-xrun, non-disconnect) error such as `snd_pcm_avail_delay` returning
//! `EIO` lands in the catch-all arm of `output_stream_worker`, which calls the
//! error callback and immediately loops round to fail again. Measured on this
//! project's WSL2 + WSLg host with the failure injected at the libasound level:
//! **~90,000 error-callback invocations per second** (1,790,033 lines in ~20 s).
//! One `eprintln!` per invocation buries the `--label-sections` terminal UI
//! within a fraction of a second and burns a whole CPU core.
//!
//! The device does not have to be at fault for this to happen. WSLg's
//! PulseAudio wedges on its own when its RDP audio endpoint goes away (its log
//! shows `module-rdp-sink.c: data_send: send failed`), after which the ALSA
//! `pulse` plugin returns `EIO` for every call. So the fix cannot be "hold the
//! device for less time"; the reporting itself has to be bounded.
//!
//! [`StreamErrorThrottle`] collapses a burst into at most one line per
//! interval and always reports how many copies it swallowed, so the count is
//! never silently lost. [`looks_wedged`] tells a caller when the error rate is
//! so far above any real glitch that the stream should be torn down rather
//! than nursed. Both are free of cpal and terminal dependencies so the policy
//! can be unit-tested against an injected clock.

use std::time::{Duration, Instant};

/// Default gap between printed lines while an error keeps repeating.
pub const DEFAULT_SUMMARY_INTERVAL: Duration = Duration::from_secs(2);

/// Error rate at or above which a stream counts as wedged rather than
/// glitching. Real xruns arrive at single digits per second; the retry spin
/// measured above is four orders of magnitude faster, so anything in between
/// works. 100/s is comfortably above legitimate glitch rates.
pub const WEDGED_ERRORS_PER_SEC: f64 = 100.0;

/// Never call a stream wedged on fewer errors than this, however short the
/// sampling window was. Guards against a single error observed 1 ms after the
/// previous sample reading as a 1000/s rate.
pub const WEDGED_MIN_ERRORS: u64 = 20;

/// One line the caller should print.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamErrorReport {
    /// Text of the error being reported.
    pub message: String,
    /// Errors swallowed since the previously printed line (0 for the first).
    pub suppressed: u64,
    /// Wall time those `suppressed` errors span.
    pub window: Duration,
}

impl StreamErrorReport {
    /// Render as a single line, without terminal framing (the caller adds the
    /// `\r` framing raw mode needs).
    pub fn to_line(&self) -> String {
        if self.suppressed == 0 {
            format!("audio stream error: {}", self.message)
        } else {
            // A wedged device fills the window in tens of milliseconds, so
            // seconds-with-one-decimal would print a misleading "0.0s".
            let window = if self.window < Duration::from_secs(1) {
                format!("{}ms", self.window.as_millis())
            } else {
                format!("{:.1}s", self.window.as_secs_f64())
            };
            format!(
                "audio stream error: {} (+{} more in the last {window})",
                self.message, self.suppressed
            )
        }
    }
}

/// Rate limiter for a cpal error callback.
///
/// Throttling is purely time-based: at most one report per `interval`,
/// regardless of whether the message changed. Exempting "a different message"
/// would be nicer to read but would reopen the flood for any device that
/// alternates between two errors, which is exactly the property this type
/// exists to guarantee against.
#[derive(Debug)]
pub struct StreamErrorThrottle {
    interval: Duration,
    last_emit: Option<Instant>,
    /// Text of the most recently *printed* error, reused by [`Self::flush`].
    /// Only updated on emit, so the suppressed fast path stays allocation-free.
    last_message: Option<String>,
    suppressed: u64,
    total: u64,
}

impl StreamErrorThrottle {
    /// Emit at most one report per `interval`.
    pub fn new(interval: Duration) -> Self {
        Self {
            interval,
            last_emit: None,
            last_message: None,
            suppressed: 0,
            total: 0,
        }
    }

    /// Record one error, materialising its text only when a line is actually
    /// due. cpal can invoke the error callback tens of thousands of times a
    /// second, so formatting every one of them is pure waste.
    pub fn record_with<F>(&mut self, now: Instant, message: F) -> Option<StreamErrorReport>
    where
        F: FnOnce() -> String,
    {
        self.total = self.total.saturating_add(1);
        if let Some(last) = self.last_emit {
            if now.duration_since(last) < self.interval {
                self.suppressed = self.suppressed.saturating_add(1);
                return None;
            }
        }
        let window = self
            .last_emit
            .map_or(Duration::ZERO, |last| now.duration_since(last));
        let report = StreamErrorReport {
            message: message(),
            suppressed: self.suppressed,
            window,
        };
        self.suppressed = 0;
        self.last_emit = Some(now);
        self.last_message = Some(report.message.clone());
        Some(report)
    }

    /// Convenience wrapper over [`Self::record_with`] for callers that already
    /// hold the message.
    pub fn record(&mut self, now: Instant, message: &str) -> Option<StreamErrorReport> {
        self.record_with(now, || message.to_string())
    }

    /// Release the tail of a collapsed burst, e.g. when the stream is being
    /// torn down and no further `record` calls will arrive. Reports the last
    /// printed message, since that is what the swallowed copies were.
    pub fn flush(&mut self, now: Instant) -> Option<StreamErrorReport> {
        if self.suppressed == 0 {
            return None;
        }
        let window = self
            .last_emit
            .map_or(Duration::ZERO, |last| now.duration_since(last));
        let report = StreamErrorReport {
            message: self.last_message.clone().unwrap_or_default(),
            suppressed: self.suppressed,
            window,
        };
        self.suppressed = 0;
        self.last_emit = Some(now);
        Some(report)
    }

    /// Errors seen since construction or the last [`Self::reset`].
    pub fn total(&self) -> u64 {
        self.total
    }

    /// Forget everything, for reuse across a rebuilt stream.
    pub fn reset(&mut self) {
        self.last_emit = None;
        self.last_message = None;
        self.suppressed = 0;
        self.total = 0;
    }
}

/// Whether `errors` observed over `elapsed` means the stream is wedged in
/// cpal's zero-backoff retry loop rather than recovering from a glitch.
pub fn looks_wedged(errors: u64, elapsed: Duration) -> bool {
    if errors < WEDGED_MIN_ERRORS {
        return false;
    }
    let secs = elapsed.as_secs_f64();
    if secs <= 0.0 {
        return true;
    }
    errors as f64 / secs >= WEDGED_ERRORS_PER_SEC
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(base: Instant, ms: u64) -> Instant {
        base + Duration::from_millis(ms)
    }

    #[test]
    fn first_error_is_reported_immediately() {
        let t0 = Instant::now();
        let mut t = StreamErrorThrottle::new(Duration::from_secs(2));
        let report = t.record(t0, "EIO").expect("first error must be reported");
        assert_eq!(report.suppressed, 0);
        assert_eq!(report.to_line(), "audio stream error: EIO");
    }

    #[test]
    fn a_flood_collapses_to_one_line_per_interval() {
        // The regression: a wedged ALSA device drives the error callback at
        // ~90k/s. Whatever the rate, the printed line count must be bounded by
        // elapsed time / interval.
        let t0 = Instant::now();
        let mut t = StreamErrorThrottle::new(Duration::from_secs(2));
        let mut lines = 0u32;
        // 10 simulated seconds at 90 kHz.
        for i in 0..900_000u64 {
            let now = at(t0, i / 90); // 90 records per millisecond
            if t.record(now, "EIO").is_some() {
                lines += 1;
            }
        }
        // 0 s, 2 s, 4 s, 6 s, 8 s (10 s is one tick past the last record).
        assert_eq!(lines, 5, "expected one line per 2 s over ~10 s");
        assert_eq!(t.total(), 900_000);
    }

    #[test]
    fn the_summary_line_carries_the_swallowed_count() {
        let t0 = Instant::now();
        let mut t = StreamErrorThrottle::new(Duration::from_secs(2));
        assert!(t.record(t0, "EIO").is_some());
        for i in 1..=1000u64 {
            assert!(t.record(at(t0, i), "EIO").is_none(), "i={i}");
        }
        let report = t
            .record(at(t0, 2_000), "EIO")
            .expect("interval elapsed, must report");
        assert_eq!(report.suppressed, 1000);
        assert_eq!(
            report.to_line(),
            "audio stream error: EIO (+1000 more in the last 2.0s)"
        );
    }

    #[test]
    fn a_changed_message_does_not_bypass_the_throttle() {
        // Alternating messages must not reopen the flood.
        let t0 = Instant::now();
        let mut t = StreamErrorThrottle::new(Duration::from_secs(2));
        assert!(t.record(t0, "EIO").is_some());
        let mut lines = 0u32;
        for i in 1..10_000u64 {
            let msg = if i % 2 == 0 { "EIO" } else { "broken pipe" };
            if t.record(at(t0, i / 10), msg).is_some() {
                lines += 1;
            }
        }
        assert!(lines <= 1, "alternating messages leaked {lines} lines");
    }

    #[test]
    fn record_with_does_not_format_suppressed_errors() {
        let t0 = Instant::now();
        let mut t = StreamErrorThrottle::new(Duration::from_secs(2));
        let mut formatted = 0u32;
        for i in 0..1000u64 {
            t.record_with(at(t0, i), || {
                formatted += 1;
                "EIO".to_string()
            });
        }
        assert_eq!(formatted, 1, "only the reported error should be formatted");
    }

    #[test]
    fn flush_releases_the_tail_of_a_burst_and_only_once() {
        let t0 = Instant::now();
        let mut t = StreamErrorThrottle::new(Duration::from_secs(2));
        assert!(t.record(t0, "EIO").is_some());
        for i in 1..=50u64 {
            t.record(at(t0, i), "EIO");
        }
        let report = t.flush(at(t0, 100)).expect("tail must be released");
        assert_eq!(report.suppressed, 50);
        assert_eq!(report.message, "EIO", "flush reuses the last printed text");
        assert!(t.flush(at(t0, 200)).is_none(), "nothing left to flush");
    }

    #[test]
    fn flush_on_a_quiet_throttle_reports_nothing() {
        let t0 = Instant::now();
        let mut t = StreamErrorThrottle::new(Duration::from_secs(2));
        assert!(t.flush(t0).is_none(), "no errors seen at all");
        assert!(t.record(t0, "EIO").is_some());
        assert!(
            t.flush(at(t0, 10)).is_none(),
            "the single error was already printed"
        );
    }

    #[test]
    fn reset_lets_a_rebuilt_stream_report_again() {
        let t0 = Instant::now();
        let mut t = StreamErrorThrottle::new(Duration::from_secs(2));
        assert!(t.record(t0, "EIO").is_some());
        assert!(t.record(at(t0, 1), "EIO").is_none());
        t.reset();
        assert_eq!(t.total(), 0);
        assert!(
            t.record(at(t0, 2), "EIO").is_some(),
            "after reset the next error is news again"
        );
    }

    #[test]
    fn wedged_detection_ignores_ordinary_glitches() {
        // A handful of xruns per second is normal on a loaded machine.
        assert!(!looks_wedged(5, Duration::from_secs(1)));
        assert!(!looks_wedged(50, Duration::from_secs(1)));
        // Below the minimum count, even a tiny window is not enough evidence.
        assert!(!looks_wedged(19, Duration::from_millis(1)));
    }

    #[test]
    fn wedged_detection_fires_on_the_retry_spin() {
        // 100 ms UI poll tick at the measured ~90k/s.
        assert!(looks_wedged(9_000, Duration::from_millis(100)));
        assert!(looks_wedged(100, Duration::from_secs(1)));
        assert!(looks_wedged(20, Duration::ZERO));
    }
}
