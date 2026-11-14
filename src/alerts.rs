//! Edge-triggered severity alerts: fire once on an upward crossing, with a per-key cooldown.
//!
//! Project: Tokenomics — monitor LLM subscription accounts (usage, limits, time-left) in a TUI
//! Module:  src/alerts.rs
//! Deps:    notify-rust (best-effort desktop notify, offloaded via tokio); domain (Severity)
//! Tested:  inline `#[cfg(test)]` — the pure transition table + tracker cooldown (injected clock)
//!
//! Key responsibilities:
//! - `evaluate_alerts` (pure): map a `(prev, curr)` severity transition to fire / recover / nothing.
//! - `AlertTracker`: apply a per-`(account, kind, scope)` cooldown so re-crossings don't spam.
//! - `notify_desktop`: best-effort desktop notification; any failure is swallowed (never fatal).
//!
//! Design constraints:
//! - The in-TUI banner is the source of truth (driven by model state); this only adds notifications.
//! - Alerts key off `utilization_pct`-derived severity, never cost.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::domain::{LimitKind, Severity};

/// The outcome of a severity transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlertEvent {
    /// Crossed upward into a worse tier — fire at this severity.
    Fired(Severity),
    /// Crossed downward — a recovery.
    Recovered,
    /// No tier change.
    Unchanged,
}

/// Severity ordering (`Ok` < `Warn` < `Crit`).
fn rank(severity: Severity) -> u8 {
    match severity {
        Severity::Ok => 0,
        Severity::Warn => 1,
        Severity::Crit => 2,
    }
}

/// Classify a `(prev, curr)` severity transition. Pure and edge-triggered.
pub fn evaluate_alerts(prev: Severity, curr: Severity) -> AlertEvent {
    use std::cmp::Ordering;
    match rank(curr).cmp(&rank(prev)) {
        Ordering::Greater => AlertEvent::Fired(curr),
        Ordering::Less => AlertEvent::Recovered,
        Ordering::Equal => AlertEvent::Unchanged,
    }
}

/// The cooldown key: one alert stream per account limit window.
type AlertKey = (String, LimitKind, Option<String>);

/// Applies a per-key cooldown on top of the edge-triggered evaluator, so a flapping limit does not
/// spam. The clock is injected so the cooldown is testable.
#[derive(Debug)]
pub struct AlertTracker {
    cooldown: Duration,
    last_fired: HashMap<AlertKey, (Severity, Instant)>,
}

impl AlertTracker {
    /// Build a tracker with the given per-key cooldown.
    pub fn new(cooldown: Duration) -> Self {
        Self {
            cooldown,
            last_fired: HashMap::new(),
        }
    }

    /// Evaluate a transition; return `Some(severity)` to fire (subject to the cooldown), else `None`.
    pub fn on_transition(
        &mut self,
        key: AlertKey,
        prev: Severity,
        curr: Severity,
        now: Instant,
    ) -> Option<Severity> {
        let AlertEvent::Fired(severity) = evaluate_alerts(prev, curr) else {
            return None;
        };
        let ready = self
            .last_fired
            .get(&key)
            // An escalation (warn → crit) is new information, never a flap: it beats the cooldown.
            .is_none_or(|(last_sev, last)| {
                rank(severity) > rank(*last_sev) || now.duration_since(*last) >= self.cooldown
            });
        if ready {
            self.last_fired.insert(key, (severity, now));
            Some(severity)
        } else {
            None
        }
    }
}

/// Best-effort desktop notification. Any failure (no notification daemon, etc.) is swallowed.
///
/// `notify-rust`'s `show()` is a **blocking, unbounded** D-Bus round trip, and the collector runs on
/// a current-thread runtime — calling it inline would stall the whole loop (and shutdown) if the
/// notification daemon is slow/unreachable. So it is offloaded to the blocking pool, fire-and-forget;
/// only the notification outcome is best-effort. Must be called from within a Tokio runtime.
pub fn notify_desktop(summary: &str, body: &str) {
    let summary = summary.to_string();
    let body = body.to_string();
    drop(tokio::task::spawn_blocking(move || {
        if let Err(e) = notify_rust::Notification::new()
            .summary(&summary)
            .body(&body)
            .show()
        {
            eprintln!("alerts: desktop notify failed (non-fatal): {e}");
        }
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upward_crossings_fire_at_the_new_severity() {
        assert_eq!(
            evaluate_alerts(Severity::Ok, Severity::Warn),
            AlertEvent::Fired(Severity::Warn)
        );
        assert_eq!(
            evaluate_alerts(Severity::Warn, Severity::Crit),
            AlertEvent::Fired(Severity::Crit)
        );
        assert_eq!(
            evaluate_alerts(Severity::Ok, Severity::Crit),
            AlertEvent::Fired(Severity::Crit)
        );
    }

    #[test]
    fn downward_is_recovery_and_equal_is_unchanged() {
        assert_eq!(
            evaluate_alerts(Severity::Crit, Severity::Warn),
            AlertEvent::Recovered
        );
        assert_eq!(
            evaluate_alerts(Severity::Warn, Severity::Ok),
            AlertEvent::Recovered
        );
        assert_eq!(
            evaluate_alerts(Severity::Warn, Severity::Warn),
            AlertEvent::Unchanged
        );
        assert_eq!(
            evaluate_alerts(Severity::Ok, Severity::Ok),
            AlertEvent::Unchanged
        );
    }

    #[test]
    fn tracker_fires_once_then_cools_down() {
        let mut tracker = AlertTracker::new(Duration::from_secs(200));
        let key = || ("acct".to_string(), LimitKind::Session, None);
        let t0 = Instant::now();

        // First upward crossing fires.
        assert_eq!(
            tracker.on_transition(key(), Severity::Ok, Severity::Warn, t0),
            Some(Severity::Warn)
        );
        // A re-crossing within the cooldown is suppressed.
        assert_eq!(
            tracker.on_transition(
                key(),
                Severity::Ok,
                Severity::Warn,
                t0 + Duration::from_secs(10)
            ),
            None
        );
        // After the cooldown, it fires again.
        assert_eq!(
            tracker.on_transition(
                key(),
                Severity::Ok,
                Severity::Warn,
                t0 + Duration::from_secs(201)
            ),
            Some(Severity::Warn)
        );
        // An escalation inside the cooldown still fires — the more urgent alert must not be lost.
        assert_eq!(
            tracker.on_transition(
                key(),
                Severity::Warn,
                Severity::Crit,
                t0 + Duration::from_secs(210)
            ),
            Some(Severity::Crit)
        );
        // A non-crossing never fires.
        assert_eq!(
            tracker.on_transition(
                key(),
                Severity::Warn,
                Severity::Warn,
                t0 + Duration::from_secs(999)
            ),
            None
        );
    }
}
