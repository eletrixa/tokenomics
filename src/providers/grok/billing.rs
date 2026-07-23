//! Pure parse of Grok Build's `billing: fetched credits config` lines → the weekly quota limit
//! (spec 022 §A).
//!
//! Project: Tokenomics — monitor LLM subscription accounts (usage, limits, time-left) in a TUI
//! Module:  src/providers/grok/billing.rs
//! Deps:    jiff, serde_json; domain (Limit), format (severity_for)
//! Tested:  inline `#[cfg(test)]` below — newest-wins, skip rules, period expiry, verbatim fields
//!
//! Key responsibilities:
//! - `newest_billing_quota`: scan `unified.jsonl` bytes for the newest-by-`ts` billing line that
//!   carries a usable `creditUsagePercent` + `currentPeriod.end` (skip everything else per-line).
//! - `parse_weekly_quota`: turn that into at most one `WeeklyAll` `Authoritative` limit, dropping
//!   a lapsed period (never render a dead week's percent as live).
//!
//! Design constraints:
//! - `creditUsagePercent` is the WEEKLY SUBSCRIPTION quota % (spec 022 evidence) — spec 021 §C's
//!   "on-demand credit" reading was wrong. On-demand fields (`onDemandCap/Used`, `prepaidBalance`)
//!   stay ignored.
//! - A line with no `creditUsagePercent` (early-session fetch) is schema state, never a real 0%.
//! - `resets_at` is the period-end string verbatim; the parsed form is used only for expiry.

use jiff::Timestamp;
use serde::Deserialize;

use crate::domain::{Limit, LimitKind, Provenance, Provider};
use crate::format::severity_for;

/// The `msg` value that marks a credits-config billing line.
const BILLING_MSG: &str = "billing: fetched credits config";

/// The newest usable billing quota found in a log: percent, verbatim period end, parsed period end.
#[derive(Debug, Clone, PartialEq)]
pub struct BillingQuota {
    /// The event's wall-clock time (`ts`) — newest wins across interleaved pids.
    pub timestamp: Timestamp,
    /// The weekly subscription quota utilization, 0–100, verbatim from the line.
    pub percent: f64,
    /// `currentPeriod.end` exactly as logged — becomes `resets_at` verbatim.
    pub period_end_raw: String,
    /// `period_end_raw` parsed, used only for the lapsed-period check.
    pub period_end: Timestamp,
}

/// Serde shape of one `unified.jsonl` line for the billing scan; non-billing lines are filtered by
/// `msg`, and heterogeneous `ctx` shapes parse as `None` fields rather than failing the line.
#[derive(Debug, Deserialize)]
struct LogLine {
    ts: Timestamp,
    msg: String,
    #[serde(default)]
    ctx: Option<Ctx>,
}

/// The billing line's `ctx` — only the `config` block matters here.
#[derive(Debug, Deserialize)]
struct Ctx {
    #[serde(default)]
    config: Option<CreditsConfig>,
}

/// The credits config block: the quota percent and its weekly period.
#[derive(Debug, Deserialize)]
struct CreditsConfig {
    #[serde(default, rename = "creditUsagePercent")]
    credit_usage_percent: Option<f64>,
    #[serde(default, rename = "currentPeriod")]
    current_period: Option<CurrentPeriod>,
}

/// The weekly usage period; only `end` is consumed (`start`/`type` are informational).
#[derive(Debug, Deserialize)]
struct CurrentPeriod {
    #[serde(default)]
    end: Option<String>,
}

/// Scan log bytes for the newest-by-`ts` billing line carrying a finite non-negative
/// `creditUsagePercent` AND a parseable `currentPeriod.end`. Every other line — other `msg`,
/// missing pct (early-session fetch), malformed JSON, negative/non-finite pct, unparseable end —
/// is skipped per-line, never failing the file (spec 022 §A).
pub fn newest_billing_quota(bytes: &[u8]) -> Option<BillingQuota> {
    let text = std::str::from_utf8(bytes).ok()?;
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .filter_map(parse_line)
        // max_by_key on ts; ties keep the later occurrence (max_by returns the last max element).
        .max_by_key(|q| q.timestamp)
}

/// Parse one line into a usable `BillingQuota`, or `None` when any required piece is absent.
fn parse_line(line: &str) -> Option<BillingQuota> {
    let parsed: LogLine = serde_json::from_str(line).ok()?;
    if parsed.msg != BILLING_MSG {
        return None;
    }
    let config = parsed.ctx?.config?;
    let percent = config.credit_usage_percent?;
    if !percent.is_finite() || percent < 0.0 {
        return None;
    }
    let period_end_raw = config.current_period?.end?;
    let period_end: Timestamp = period_end_raw.parse().ok()?;
    Some(BillingQuota {
        timestamp: parsed.ts,
        percent,
        period_end_raw,
        period_end,
    })
}

/// The weekly quota as at most one `WeeklyAll` `Authoritative` limit: the newest usable billing
/// line, dropped entirely when its period has lapsed (`end <= now`) — an expired week's percent is
/// never rendered as live (spec 022 §A). Pure (`now` injected).
pub fn parse_weekly_quota(
    bytes: &[u8],
    account_id: &str,
    now: Timestamp,
    warn_pct: f64,
    crit_pct: f64,
) -> Vec<Limit> {
    let Some(quota) = newest_billing_quota(bytes) else {
        return Vec::new();
    };
    if quota.period_end <= now {
        return Vec::new();
    }
    vec![Limit {
        account_id: account_id.to_string(),
        provider: Provider::Grok,
        kind: LimitKind::WeeklyAll,
        scope: None,
        utilization_pct: quota.percent,
        resets_at: quota.period_end_raw,
        severity: severity_for(quota.percent, warn_pct, crit_pct),
        source: Provenance::Authoritative,
    }]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::Severity;

    fn ts(s: &str) -> Timestamp {
        s.parse().expect("valid timestamp")
    }

    const UNIFIED: &[u8] = include_bytes!("../../../fixtures/grok_unified.jsonl");

    /// One synthetic real-shape billing line — values invented for tests, never real log content.
    fn billing_line(timestamp: &str, percent: f64, end: &str) -> String {
        format!(
            r#"{{"ts":"{timestamp}","src":"shell","pid":1,"lvl":"info","msg":"billing: fetched credits config","ctx":{{"config":{{"creditUsagePercent":{percent},"currentPeriod":{{"type":"USAGE_PERIOD_TYPE_WEEKLY","start":"2026-07-19T00:00:00+00:00","end":"{end}"}},"onDemandCap":{{"val":0}},"onDemandUsed":{{"val":0}},"prepaidBalance":{{"val":0}}}},"subscriptionTier":"SuperGrok Heavy"}}}}"#
        )
    }

    // ── AC1 (spec 022 §A): newest-by-ts wins; skip rules ────────────────────────────────────────

    #[test]
    fn fixture_newest_billing_line_wins_and_skips_pctless_and_drifted_shapes() {
        let quota = newest_billing_quota(UNIFIED).expect("fixture holds usable billing lines");
        // The 12.5% line is newest by ts even though an older-ts 7.5% line sits LATER in the file
        // (interleaved pids) — and the pct-less + config-less billing lines are skipped.
        assert!((quota.percent - 12.5).abs() < f64::EPSILON);
        assert_eq!(quota.timestamp, ts("2026-07-19T15:00:00Z"));
        assert_eq!(quota.period_end_raw, "2026-07-26T00:00:00+00:00");
    }

    #[test]
    fn newest_wins_by_ts_not_file_position() {
        let log = format!(
            "{}\n{}\n",
            billing_line("2026-07-19T15:00:00Z", 12.5, "2026-07-26T00:00:00+00:00"),
            billing_line("2026-07-19T09:00:00Z", 7.5, "2026-07-26T00:00:00+00:00"),
        );
        let quota = newest_billing_quota(log.as_bytes()).expect("usable lines");
        assert!(
            (quota.percent - 12.5).abs() < f64::EPSILON,
            "ts order must beat file position"
        );
    }

    #[test]
    fn negative_nonfinite_and_missing_pieces_are_skipped_per_line() {
        let end = "2026-07-26T00:00:00+00:00";
        let log = format!(
            "{}\n{}\n{}\n{}\n",
            billing_line("2026-07-19T10:00:00Z", -1.0, end), // negative pct
            r#"{"ts":"2026-07-19T11:00:00Z","msg":"billing: fetched credits config","ctx":{"config":{"creditUsagePercent":5.0}}}"#, // no period
            r#"{"ts":"2026-07-19T12:00:00Z","msg":"billing: fetched credits config","ctx":{"config":{"creditUsagePercent":5.0,"currentPeriod":{"end":"not a timestamp"}}}}"#,
            "{malformed",
        );
        assert!(newest_billing_quota(log.as_bytes()).is_none());
    }

    #[test]
    fn empty_and_nonutf8_input_yield_none() {
        assert!(newest_billing_quota(b"").is_none());
        assert!(newest_billing_quota(&[0xff, 0xfe, 0x00]).is_none());
    }

    // ── AC1: parse_weekly_quota — expiry, verbatim fields, severity ─────────────────────────────

    #[test]
    fn live_period_yields_one_authoritative_weekly_limit_with_verbatim_fields() {
        let log = billing_line("2026-07-19T15:00:00Z", 12.5, "2026-07-26T00:00:00+00:00");
        let now = ts("2026-07-20T00:00:00Z");
        let limits = parse_weekly_quota(log.as_bytes(), "grok-heavy", now, 60.0, 85.0);
        assert_eq!(limits.len(), 1);
        let limit = &limits[0];
        assert_eq!(limit.account_id, "grok-heavy");
        assert_eq!(limit.provider, Provider::Grok);
        assert_eq!(limit.kind, LimitKind::WeeklyAll);
        assert_eq!(limit.scope, None);
        assert!((limit.utilization_pct - 12.5).abs() < f64::EPSILON);
        assert_eq!(
            limit.resets_at, "2026-07-26T00:00:00+00:00",
            "resets_at must be the period-end string verbatim, never reformatted"
        );
        assert_eq!(limit.severity, Severity::Ok);
        assert_eq!(limit.source, Provenance::Authoritative);
    }

    #[test]
    fn lapsed_period_yields_no_limit() {
        let log = billing_line("2026-07-19T15:00:00Z", 12.5, "2026-07-26T00:00:00+00:00");
        let at_end = ts("2026-07-26T00:00:00Z");
        let after = ts("2026-07-27T00:00:00Z");
        assert!(parse_weekly_quota(log.as_bytes(), "grok-heavy", at_end, 60.0, 85.0).is_empty());
        assert!(parse_weekly_quota(log.as_bytes(), "grok-heavy", after, 60.0, 85.0).is_empty());
    }

    #[test]
    fn severity_honours_thresholds() {
        let now = ts("2026-07-20T00:00:00Z");
        let end = "2026-07-26T00:00:00+00:00";
        let warn = billing_line("2026-07-19T15:00:00Z", 70.0, end);
        let crit = billing_line("2026-07-19T15:00:00Z", 92.0, end);
        assert_eq!(
            parse_weekly_quota(warn.as_bytes(), "g", now, 60.0, 85.0)[0].severity,
            Severity::Warn
        );
        assert_eq!(
            parse_weekly_quota(crit.as_bytes(), "g", now, 60.0, 85.0)[0].severity,
            Severity::Crit
        );
    }
}
