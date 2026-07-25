//! Pure presentation: severity classification + human formatting for limits, tokens, and time.
//!
//! Project: Tokenomics — monitor LLM subscription accounts (usage, limits, time-left) in a TUI
//! Module:  src/format.rs
//! Deps:    jiff (reset countdowns); domain (Severity)
//! Tested:  inline `#[cfg(test)]` below
//!
//! Key responsibilities:
//! - `severity_for`: the canonical threshold classifier (used by the collector and the TUI).
//! - `format_pct` / `format_tokens` / `format_cost` / `format_money` / `format_reset`: human strings for the board.
//!
//! Design constraints:
//! - Everything here is pure and deterministic (time is injected); no I/O.
//! - `resets_at` is rendered verbatim when it does not parse — a reset time is never fabricated.
//! - Cost always carries the "notional" label — it is a usage proxy, never a bill.
//! - `format_money` is the ONLY real-money (billing) formatter — its `$`/`€`/code + 2-dp + sign
//!   grammar is deliberately disjoint from the notional proxy; never route notional values through it.

use std::collections::HashMap;

use jiff::Timestamp;

use crate::domain::{Limit, LimitKind, Provenance, Severity};

/// Provenance precedence: `Authoritative` (2) > `Derived` (1) > `Estimate` (0).
fn provenance_rank(source: Provenance) -> u8 {
    match source {
        Provenance::Authoritative => 2,
        Provenance::Derived => 1,
        Provenance::Estimate => 0,
    }
}

/// Merge two limit sets by `(kind, scope)`, keeping the higher-provenance source
/// (Authoritative > Derived > Estimate); at equal provenance the `incoming` (newer) wins. The
/// result is sorted deterministically (session, then weekly-all, then scoped by label).
pub fn merge_limits(existing: Vec<Limit>, incoming: Vec<Limit>) -> Vec<Limit> {
    let mut by_key: HashMap<(LimitKind, Option<String>), Limit> = HashMap::new();
    for limit in existing.into_iter().chain(incoming) {
        let key = (limit.kind, limit.scope.clone());
        let replace = by_key
            .get(&key)
            .is_none_or(|current| provenance_rank(limit.source) >= provenance_rank(current.source));
        if replace {
            by_key.insert(key, limit);
        }
    }
    let mut merged: Vec<Limit> = by_key.into_values().collect();
    merged.sort_by(|a, b| {
        kind_rank(a.kind)
            .cmp(&kind_rank(b.kind))
            .then(a.scope.cmp(&b.scope))
    });
    merged
}

/// Demote stored `Authoritative` limits to `Estimate` when the overlay has gone stale, so the fresh
/// `Derived` session wins the subsequent [`merge_limits`] while the last-known weekly/scoped values
/// stay on the board — frozen percent, countdown still computed live from the stored `resets_at` —
/// instead of collapsing to an `n/a` hint. Honours the "degrade silently to derived on any
/// 429/failure" invariant: a demoted row is out-ranked by any live derived one, and a recovered
/// overlay's authoritative rows win it back. When a demoted row's reset passes, the spec 012 §A
/// expiry machinery renders it dormant ("waiting for reset"), so nothing alarms forever.
/// Authoritative is fresh iff the last overlay success is present AND within `ttl_secs` of `now_ms`.
/// Pure (`now_ms` injected). Non-authoritative rows are never touched.
pub fn demote_stale_authoritative(
    current: Vec<Limit>,
    overlay_success_ms: Option<i64>,
    now_ms: i64,
    ttl_secs: u64,
) -> Vec<Limit> {
    let ttl_ms = i64::try_from(ttl_secs.saturating_mul(1000)).unwrap_or(i64::MAX);
    let fresh = overlay_success_ms.is_some_and(|then| now_ms.saturating_sub(then) <= ttl_ms);
    if fresh {
        return current;
    }
    current
        .into_iter()
        .map(|mut l| {
            if l.source == Provenance::Authoritative {
                l.source = Provenance::Estimate;
            }
            l
        })
        .collect()
}

/// Stable display order for limit kinds.
fn kind_rank(kind: LimitKind) -> u8 {
    match kind {
        LimitKind::Session => 0,
        LimitKind::WeeklyAll => 1,
        LimitKind::WeeklyScoped => 2,
    }
}

/// The lowercase severity label (`ok` / `warn` / `crit`).
pub fn severity_label(severity: Severity) -> &'static str {
    severity.as_str()
}

/// The lowercase provenance badge (`authoritative` / `derived` / `estimate`).
pub fn provenance_label(source: Provenance) -> &'static str {
    source.as_str()
}

/// The 3–4 char provenance abbreviation for tight tiers (`auth` / `drv` / `est`).
pub fn provenance_short(source: Provenance) -> &'static str {
    match source {
        Provenance::Authoritative => "auth",
        Provenance::Derived => "drv",
        Provenance::Estimate => "est",
    }
}

/// Classify a utilization % against `warn`/`crit` thresholds (percentages). `NaN`/negative ⇒ `Ok`.
/// The single source of truth for severity; the collector and the TUI both call this.
pub fn severity_for(utilization_pct: f64, warn: f64, crit: f64) -> Severity {
    if utilization_pct >= crit {
        Severity::Crit
    } else if utilization_pct >= warn {
        Severity::Warn
    } else {
        Severity::Ok
    }
}

/// Format a utilization % with no decimals, clamped at a lower bound of 0 (`37.4` ⇒ `"37%"`).
pub fn format_pct(utilization_pct: f64) -> String {
    let clamped = if utilization_pct.is_nan() {
        0.0
    } else {
        utilization_pct.max(0.0)
    };
    format!("{clamped:.0}%")
}

/// Format a token count compactly: `1_234_567` ⇒ `"1.23M"`, `12_345` ⇒ `"12.3K"`, `999` ⇒ `"999"`.
/// Uses integer math (no float cast) so the digits are exact.
pub fn format_tokens(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        let whole = tokens / 1_000_000;
        let hundredths = (tokens % 1_000_000) / 10_000;
        format!("{whole}.{hundredths:02}M")
    } else if tokens >= 1_000 {
        let whole = tokens / 1_000;
        let tenths = (tokens % 1_000) / 100;
        format!("{whole}.{tenths}K")
    } else {
        tokens.to_string()
    }
}

/// Format a notional cost, always labeled (`1.7` ⇒ `"$1.70 (notional)"`).
pub fn format_cost(cost_notional: f64) -> String {
    format!("${cost_notional:.2} (notional)")
}

/// Format a notional cost as whole dollars for the tightest tiers (`335.95` ⇒ `"$335"`). The
/// "(notional)" label is dropped here to save cells; the dashboard title carries a standing
/// `$ = notional` legend so the proxy meaning is never lost. Truncates (never rounds up a bill).
pub fn format_dollars(cost_notional: f64) -> String {
    format!("${:.0}", cost_notional.max(0.0).floor())
}

/// Real-money formatter for the API billing plane (spec 025): minor units → "$12.34".
/// USD → "$", EUR → "€", anything else → "<CODE> " prefix (uppercased, e.g. "CZK 1.00").
/// Always exactly 2 decimals. Negative → leading "-" before the glyph: "-$0.52".
/// This grammar is deliberately disjoint from the notional-cost formatting — never add
/// an "n"/"(notional)" suffix here, and never route notional values through this fn.
#[allow(dead_code)] // consumed by the spec 025 TUI wave; allow dies with the wiring
pub fn format_money(amount_minor: i64, currency: &str) -> String {
    let negative = amount_minor < 0;
    let abs = amount_minor.unsigned_abs();
    let whole = abs / 100;
    let cents = abs % 100;
    let sign = if negative { "-" } else { "" };
    match currency {
        "USD" => format!("{sign}${whole}.{cents:02}"),
        "EUR" => format!("{sign}€{whole}.{cents:02}"),
        other => format!("{sign}{} {whole}.{cents:02}", other.to_uppercase()),
    }
}

/// The past-reset sentinel: the stored reset time has passed, so the window has reset and the stored
/// utilization is history, not state — we're waiting for fresh evidence (a new collect, or an overlay
/// success after the user logs in / opens Claude) to bring the new countdown. Read verbatim by the
/// label composers to drop the `"resets "` verb (never "resets waiting for reset").
pub const RESET_DONE: &str = "waiting for reset";

/// Whether `resets_at` parses and is already at or past `now`. Unparseable ⇒ `false` — an unknown
/// time is never treated as expired (verbatim rule: we don't fabricate a state we can't compute).
pub fn reset_expired(resets_at: &str, now: Timestamp) -> bool {
    resets_at
        .parse::<Timestamp>()
        .is_ok_and(|t| t.as_millisecond() <= now.as_millisecond())
}

/// Format the time until `resets_at` as a countdown from `now` (`"in 2h 41m"`). Already past ⇒
/// [`RESET_DONE`]. Unparseable ⇒ the raw string, verbatim (never fabricated).
pub fn format_reset(resets_at: &str, now: Timestamp) -> String {
    let Ok(target) = resets_at.parse::<Timestamp>() else {
        return resets_at.to_string();
    };
    let diff_ms = target.as_millisecond() - now.as_millisecond();
    if diff_ms <= 0 {
        return RESET_DONE.to_string();
    }
    let total_minutes = diff_ms / 1000 / 60;
    let (days, hours, minutes) = (
        total_minutes / (24 * 60),
        (total_minutes / 60) % 24,
        total_minutes % 60,
    );
    if days > 0 {
        format!("in {days}d {hours}h")
    } else if hours > 0 {
        format!("in {hours}h {minutes}m")
    } else {
        format!("in {minutes}m")
    }
}

/// Format how long ago `then_ms` (epoch-millis) was, relative to `now` — see [`format_ago_ms`].
pub fn format_ago(then_ms: i64, now: Timestamp) -> String {
    format_ago_ms(now.as_millisecond() - then_ms)
}

/// Humanize an elapsed duration (ms) as a staleness signal: `"just now"` (< 5s or in the future),
/// then `"12s ago"` / `"5m ago"` / `"3h ago"` / `"2d ago"`. Seconds resolution below a minute so a
/// fast poll cadence reads as live rather than collapsing to "just now" for a whole minute. Coarse
/// above a minute by design — it signals staleness, not precision.
pub fn format_ago_ms(diff_ms: i64) -> String {
    let seconds = diff_ms / 1000;
    if seconds < 5 {
        return "just now".to_string();
    }
    if seconds < 60 {
        return format!("{seconds}s ago");
    }
    let minutes = seconds / 60;
    if minutes < 60 {
        format!("{minutes}m ago")
    } else if minutes < 24 * 60 {
        format!("{}h ago", minutes / 60)
    } else {
        format!("{}d ago", minutes / (24 * 60))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(s: &str) -> Timestamp {
        s.parse().expect("valid timestamp")
    }

    #[test]
    fn ago_scales_and_clamps_future_to_just_now() {
        let now = ts("2026-07-04T12:00:00Z");
        let ms = |s: &str| ts(s).as_millisecond();
        assert_eq!(format_ago(ms("2026-07-04T11:58:00Z"), now), "2m ago");
        assert_eq!(format_ago(ms("2026-07-04T09:00:00Z"), now), "3h ago");
        assert_eq!(format_ago(ms("2026-07-01T12:00:00Z"), now), "3d ago");
        // Sub-minute now resolves to seconds (so a ~10-20s cadence reads as live, not "just now").
        assert_eq!(format_ago(ms("2026-07-04T11:59:30Z"), now), "30s ago");
        assert_eq!(format_ago(ms("2026-07-04T11:59:48Z"), now), "12s ago");
        // Within a few seconds (or a future timestamp from clock skew) still reads "just now".
        assert_eq!(format_ago(ms("2026-07-04T11:59:59Z"), now), "just now");
        assert_eq!(format_ago(ms("2026-07-04T12:05:00Z"), now), "just now");
    }

    #[test]
    fn ago_ms_humanizes_elapsed_duration_directly() {
        assert_eq!(format_ago_ms(0), "just now");
        assert_eq!(format_ago_ms(12_000), "12s ago");
        assert_eq!(format_ago_ms(90_000), "1m ago");
        assert_eq!(format_ago_ms(3 * 60 * 60 * 1000), "3h ago");
        assert_eq!(format_ago_ms(-5000), "just now"); // negative (skew) never shows
    }

    #[test]
    fn severity_thresholds_and_edges() {
        assert_eq!(severity_for(50.0, 75.0, 90.0), Severity::Ok);
        assert_eq!(severity_for(75.0, 75.0, 90.0), Severity::Warn);
        assert_eq!(severity_for(89.9, 75.0, 90.0), Severity::Warn);
        assert_eq!(severity_for(90.0, 75.0, 90.0), Severity::Crit);
        // NaN and negative both fall to Ok.
        assert_eq!(severity_for(f64::NAN, 75.0, 90.0), Severity::Ok);
        assert_eq!(severity_for(-5.0, 75.0, 90.0), Severity::Ok);
    }

    #[test]
    fn pct_rounds_and_clamps() {
        assert_eq!(format_pct(37.4), "37%");
        assert_eq!(format_pct(37.6), "38%");
        assert_eq!(format_pct(-1.0), "0%");
        assert_eq!(format_pct(f64::NAN), "0%");
    }

    #[test]
    fn tokens_scale_to_k_and_m() {
        assert_eq!(format_tokens(1_234_567), "1.23M");
        assert_eq!(format_tokens(12_345), "12.3K");
        assert_eq!(format_tokens(999), "999");
        assert_eq!(format_tokens(1_000), "1.0K");
        assert_eq!(format_tokens(244_820_890), "244.82M");
    }

    #[test]
    fn cost_is_always_labeled_notional() {
        assert_eq!(format_cost(1.7), "$1.70 (notional)");
        assert_eq!(format_cost(295.648), "$295.65 (notional)");
    }

    #[test]
    fn dollars_truncate_and_clamp() {
        // Whole dollars only, truncated (never rounds a notional proxy up).
        assert_eq!(format_dollars(335.95), "$335");
        assert_eq!(format_dollars(1.0), "$1");
        assert_eq!(format_dollars(0.0), "$0");
        assert_eq!(format_dollars(-5.0), "$0");
    }

    #[test]
    fn money_formats_cents_with_currency_glyph_and_sign() {
        // Real-money table (spec 025 §C): minor units → 2-dp string. USD/EUR get a glyph,
        // anything else an uppercased "<CODE> " prefix; negatives carry a leading "-".
        assert_eq!(format_money(1234, "USD"), "$12.34");
        assert_eq!(format_money(350, "EUR"), "€3.50");
        assert_eq!(format_money(100, "czk"), "CZK 1.00"); // lowercase → uppercased code
        assert_eq!(format_money(0, "USD"), "$0.00");
        assert_eq!(format_money(-52, "USD"), "-$0.52");
        assert_eq!(format_money(5, "USD"), "$0.05"); // leading-zero cents
                                                     // Large value keeps full precision (integer math; no float cast to round).
        assert_eq!(format_money(12_345_678, "USD"), "$123456.78");
        // A negative code-prefixed amount leads the "-" before the code too.
        assert_eq!(format_money(-52, "czk"), "-CZK 0.52");
    }

    #[test]
    fn money_handles_i64_min_without_overflow() {
        // i64::MIN must not panic: unsigned_abs() sidesteps abs()'s overflow, and the exact
        // string pins the integer math at the extreme (no f64 rounding creeps in).
        assert_eq!(format_money(i64::MIN, "USD"), "-$92233720368547758.08");
    }

    #[test]
    fn provenance_short_abbreviates() {
        assert_eq!(provenance_short(Provenance::Authoritative), "auth");
        assert_eq!(provenance_short(Provenance::Derived), "drv");
        assert_eq!(provenance_short(Provenance::Estimate), "est");
    }

    fn limit(kind: LimitKind, scope: Option<&str>, pct: f64, source: Provenance) -> Limit {
        Limit {
            account_id: "a".to_string(),
            provider: crate::domain::Provider::Claude,
            kind,
            scope: scope.map(str::to_string),
            utilization_pct: pct,
            resets_at: "2026-07-04T12:00:00Z".to_string(),
            severity: Severity::Ok,
            source,
        }
    }

    #[test]
    fn merge_prefers_authoritative_then_newer() {
        // Authoritative session beats derived session regardless of order.
        let merged = merge_limits(
            vec![limit(LimitKind::Session, None, 60.0, Provenance::Derived)],
            vec![limit(
                LimitKind::Session,
                None,
                19.0,
                Provenance::Authoritative,
            )],
        );
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].source, Provenance::Authoritative);
        assert!((merged[0].utilization_pct - 19.0).abs() < 1e-9);

        // Derived does NOT clobber an existing authoritative.
        let merged = merge_limits(
            vec![limit(
                LimitKind::Session,
                None,
                19.0,
                Provenance::Authoritative,
            )],
            vec![limit(LimitKind::Session, None, 62.0, Provenance::Derived)],
        );
        assert_eq!(merged[0].source, Provenance::Authoritative);

        // Equal provenance ⇒ incoming (newer) wins.
        let merged = merge_limits(
            vec![limit(LimitKind::Session, None, 60.0, Provenance::Derived)],
            vec![limit(LimitKind::Session, None, 62.0, Provenance::Derived)],
        );
        assert!((merged[0].utilization_pct - 62.0).abs() < 1e-9);
    }

    #[test]
    fn demote_stale_authoritative_demotes_to_estimate_past_ttl_only() {
        let now_ms = ts("2026-07-04T12:00:00Z").as_millisecond();
        let ttl = 600; // 2× a 300s overlay cadence
        let current = vec![
            limit(LimitKind::Session, None, 29.0, Provenance::Authoritative),
            limit(LimitKind::WeeklyAll, None, 91.0, Provenance::Authoritative),
            limit(
                LimitKind::WeeklyScoped,
                Some("Fable"),
                99.0,
                Provenance::Authoritative,
            ),
        ];

        // Fresh overlay (2m ago ≤ ttl) ⇒ unchanged.
        let fresh_ms = ts("2026-07-04T11:58:00Z").as_millisecond();
        let kept = demote_stale_authoritative(current.clone(), Some(fresh_ms), now_ms, ttl);
        assert_eq!(kept.len(), 3);
        assert!(kept.iter().all(|l| l.source == Provenance::Authoritative));

        // Stale overlay (20m ago > ttl) ⇒ every row KEPT, demoted to Estimate — the last-known
        // values (and their resets_at countdowns) stay on the board, only the rank drops.
        let stale_ms = ts("2026-07-04T11:40:00Z").as_millisecond();
        let demoted = demote_stale_authoritative(current.clone(), Some(stale_ms), now_ms, ttl);
        assert_eq!(
            demoted.len(),
            3,
            "values must survive demotion: {demoted:?}"
        );
        assert!(demoted.iter().all(|l| l.source == Provenance::Estimate));
        assert!((demoted[1].utilization_pct - 91.0).abs() < 1e-9);

        // Never succeeded (None) ⇒ treated as stale, demoted.
        let never = demote_stale_authoritative(current.clone(), None, now_ms, ttl);
        assert!(never.iter().all(|l| l.source == Provenance::Estimate));

        // A derived row present alongside is untouched regardless of overlay age.
        let mixed = vec![
            limit(LimitKind::Session, None, 60.0, Provenance::Derived),
            limit(LimitKind::WeeklyAll, None, 91.0, Provenance::Authoritative),
        ];
        let out = demote_stale_authoritative(mixed, None, now_ms, ttl);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].source, Provenance::Derived);
        assert_eq!(out[1].source, Provenance::Estimate);
    }

    #[test]
    fn merge_keeps_distinct_kinds_and_scopes_sorted() {
        let merged = merge_limits(
            vec![
                limit(
                    LimitKind::WeeklyScoped,
                    Some("opus"),
                    40.0,
                    Provenance::Authoritative,
                ),
                limit(LimitKind::Session, None, 19.0, Provenance::Authoritative),
            ],
            vec![limit(
                LimitKind::WeeklyAll,
                None,
                91.0,
                Provenance::Authoritative,
            )],
        );
        let kinds: Vec<LimitKind> = merged.iter().map(|l| l.kind).collect();
        assert_eq!(
            kinds,
            vec![
                LimitKind::Session,
                LimitKind::WeeklyAll,
                LimitKind::WeeklyScoped
            ]
        );
    }

    #[test]
    fn reset_countdown_past_and_verbatim() {
        let now = ts("2026-07-04T10:00:00Z");
        assert_eq!(format_reset("2026-07-04T12:41:00Z", now), "in 2h 41m");
        assert_eq!(format_reset("2026-07-04T10:30:00Z", now), "in 30m");
        assert_eq!(format_reset("2026-07-06T12:00:00Z", now), "in 2d 2h");
        // Already past ⇒ the window reset; we're waiting for fresh evidence to bring the new time.
        assert_eq!(format_reset("2026-07-04T09:00:00Z", now), RESET_DONE);
        assert_eq!(RESET_DONE, "waiting for reset");
        // Unparseable ⇒ verbatim, never fabricated.
        assert_eq!(format_reset("whenever", now), "whenever");
    }

    #[test]
    fn reset_expired_past_future_and_unparseable() {
        let now = ts("2026-07-04T10:00:00Z");
        assert!(reset_expired("2026-07-04T09:00:00Z", now)); // past
        assert!(reset_expired("2026-07-04T10:00:00Z", now)); // exactly now
        assert!(!reset_expired("2026-07-04T10:00:01Z", now)); // future
        assert!(!reset_expired("whenever", now)); // unparseable is never expired
    }
}
