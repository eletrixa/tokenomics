//! Kimi (Moonshot AI coding plan) quota endpoint: types + parse seam + the HTTP fetch (spec 023 §B).
//!
//! Project: Tokenomics — monitor LLM subscription accounts (usage, limits, time-left) in a TUI
//! Module:  src/providers/kimi/quota.rs
//! Deps:    serde_json, reqwest (rustls), async-trait; domain + format
//! Tested:  inline `#[cfg(test)]` — parse (community-shape fixture, malformed), canned fetch
//!          transcripts (spec 023 §B)
//!
//! Key responsibilities:
//! - `parse_quota_response` (pure): body → `Vec<Limit> { source = Authoritative }`. Maps the
//!   `duration`/`durationUnit`-tagged `usages[]` entries to `Session`/`WeeklyAll` by their
//!   (string-typed) `duration` discriminator, deriving % as `used/limit×100` (never clamped),
//!   erroring when neither expected window survives.
//! - `QuotaEndpoint` trait + `HttpQuotaEndpoint` (reqwest) — the only network touch; real
//!   implementation (mirrors `zai::quota::HttpQuotaEndpoint`), opt-in gated by the caller.
//!
//! Design constraints:
//! - The API key never appears in a log or error (reqwest errors carry the URL, not headers).
//! - The fixture below is community-captured, NOT live-probed — see its doc comment for the exact
//!   unverified surfaces (spec 023 §Live re-probes them against a live key).

use std::time::Duration;

use async_trait::async_trait;
use jiff::Timestamp;
use serde::Deserialize;

use crate::domain::{Limit, LimitKind, Provenance, Provider};
use crate::error::{AppError, AppResult};
use crate::format::severity_for;

const QUOTA_URL: &str = "https://api.kimi.com/coding/v1/usages";
const QUOTA_TIMEOUT_SECS: u64 = 10;

/// `duration`/`durationUnit` pair identifying the rolling 5h session window.
const SESSION_DURATION: (&str, &str) = ("300", "TIME_UNIT_MINUTE");
/// `duration`/`durationUnit` pair identifying the weekly-all window (unverified discriminator,
/// spec 023 §B — re-probe against a live key when one exists).
const WEEKLY_DURATION: (&str, &str) = ("10080", "TIME_UNIT_MINUTE");

/// The top-level quota response envelope (`{ "usages": [...] }`).
#[derive(Debug, Deserialize)]
pub struct UsagesResponse {
    #[serde(default)]
    pub usages: Vec<UsageEntry>,
}

/// One entry of the `usages[]` array. Every numeric field is string-typed on the wire (community-
/// captured shape — see `REAL_SHAPE`'s doc comment in the test module for what's unverified) and
/// optional/defaulted so a missing or reshaped field degrades to a skip, never a parse failure.
#[derive(Debug, Deserialize)]
pub struct UsageEntry {
    #[serde(default)]
    pub limit: Option<String>,
    #[serde(default)]
    pub used: Option<String>,
    #[serde(default, rename = "resetTime")]
    pub reset_time: Option<String>,
    #[serde(default)]
    pub duration: Option<String>,
    #[serde(default, rename = "durationUnit")]
    pub duration_unit: Option<String>,
}

/// Parse a Kimi quota body into authoritative limits (spec 023 §B). Maps the `duration`/
/// `durationUnit`-tagged `usages[]` entries to `Session`/`WeeklyAll`, deriving % as
/// `used/limit×100` (never clamped); any other entry, or one with unusable numerics, is skipped
/// defensively; malformed JSON or neither expected window present ⇒ `Err` (schema drift must
/// degrade, never render a wrong gauge).
pub fn parse_quota_response(
    bytes: &[u8],
    account_id: &str,
    warn_pct: f64,
    crit_pct: f64,
) -> AppResult<Vec<Limit>> {
    let response: UsagesResponse = serde_json::from_slice(bytes)
        .map_err(|e| AppError::Overlay(format!("malformed kimi quota body: {e}")))?;

    let build = |kind: LimitKind, pct: f64, resets_at: String| Limit {
        account_id: account_id.to_string(),
        provider: Provider::Kimi,
        kind,
        scope: None,
        utilization_pct: pct,
        resets_at,
        severity: severity_for(pct, warn_pct, crit_pct),
        source: Provenance::Authoritative,
    };

    let mut limits = Vec::new();
    for entry in response.usages {
        let kind = match (entry.duration.as_deref(), entry.duration_unit.as_deref()) {
            (Some(d), Some(u)) if (d, u) == SESSION_DURATION => LimitKind::Session,
            (Some(d), Some(u)) if (d, u) == WEEKLY_DURATION => LimitKind::WeeklyAll,
            _ => continue, // an unrecognized duration/durationUnit pair — skipped defensively
        };

        // Schema drift (absent/non-numeric/<=0 limit, or absent/non-numeric used) is not a real
        // 0% — skip it rather than render a confident-looking wrong gauge.
        let Some(limit) = entry
            .limit
            .as_deref()
            .and_then(|s| s.trim().parse::<f64>().ok())
            .filter(|v| v.is_finite() && *v > 0.0)
        else {
            continue;
        };
        let Some(used) = entry
            .used
            .as_deref()
            .and_then(|s| s.trim().parse::<f64>().ok())
            .filter(|v| v.is_finite())
        else {
            continue;
        };

        // Kimi provides a real reset instant (unlike z.ai's rolling session window) — use it
        // verbatim; absent or unparsable degrades to "" rather than erroring the whole entry.
        let resets_at = entry
            .reset_time
            .as_deref()
            .and_then(|s| s.parse::<Timestamp>().ok())
            .map(|ts| ts.to_string())
            .unwrap_or_default();

        limits.push(build(kind, used / limit * 100.0, resets_at));
    }

    if limits.is_empty() {
        return Err(AppError::Overlay(format!(
            "kimi quota response for {account_id} carried neither expected usage window"
        )));
    }

    Ok(limits)
}

/// The quota endpoint seam. `HttpQuotaEndpoint` is the only network touch; tests use a canned one.
#[async_trait]
pub trait QuotaEndpoint: Send + Sync {
    /// GET the quota body for `api_key`. `Err(RateLimited)` on 429 (drives the shared backoff).
    async fn fetch(&self, api_key: &str) -> AppResult<Vec<u8>>;
}

/// The real reqwest (rustls) endpoint. Bounded by a request timeout; opt-in gated by the caller.
#[derive(Debug)]
pub struct HttpQuotaEndpoint {
    client: reqwest::Client,
}

impl HttpQuotaEndpoint {
    /// Build the shared client (created once; reused across polls).
    pub fn new() -> AppResult<Self> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(QUOTA_TIMEOUT_SECS))
            .connect_timeout(Duration::from_secs(5))
            .build()
            .map_err(|e| AppError::Overlay(format!("cannot build HTTP client: {e}")))?;
        Ok(Self { client })
    }
}

#[async_trait]
impl QuotaEndpoint for HttpQuotaEndpoint {
    async fn fetch(&self, api_key: &str) -> AppResult<Vec<u8>> {
        let response = self
            .client
            .get(QUOTA_URL)
            .bearer_auth(api_key)
            .header("Accept", "application/json")
            .send()
            .await
            .map_err(|e| AppError::Overlay(format!("request failed: {e}")))?;

        let status = response.status();
        if status.as_u16() == 429 {
            // Known envelope `error.type == "exceeded_current_quota_error"` (spec 023 §B) —
            // documented for future use, never matched on message text; status alone drives this.
            return Err(AppError::RateLimited);
        }
        if !status.is_success() {
            return Err(AppError::Overlay(format!("HTTP {}", status.as_u16())));
        }
        let bytes = response
            .bytes()
            .await
            .map_err(|e| AppError::Overlay(format!("read body failed: {e}")))?;
        Ok(bytes.to_vec())
    }
}

/// A canned endpoint for tests — no network. Test-only.
#[cfg(test)]
#[derive(Debug)]
pub enum Canned {
    /// Return this body verbatim.
    Body(Vec<u8>),
    /// Simulate a 429.
    RateLimited,
    /// Simulate a transport failure (401/403/garbage all surface as a plain fetch failure here —
    /// the real endpoint's non-200 handling is exercised in `HttpQuotaEndpoint` itself, not this
    /// no-network double).
    Fail,
}

/// A [`QuotaEndpoint`] returning a canned outcome (no network). Test-only.
#[cfg(test)]
#[derive(Debug)]
pub struct CannedEndpoint {
    pub canned: Canned,
}

#[cfg(test)]
#[async_trait]
impl QuotaEndpoint for CannedEndpoint {
    async fn fetch(&self, _api_key: &str) -> AppResult<Vec<u8>> {
        match &self.canned {
            Canned::Body(bytes) => Ok(bytes.clone()),
            Canned::RateLimited => Err(AppError::RateLimited),
            Canned::Fail => Err(AppError::Overlay("canned failure".to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{LimitKind, Provenance};
    use jiff::Timestamp;

    /// Community-captured shape (2 corroborating sources), NOT live-probed — Kimi signups
    /// paused ~2026-07-19, no subscription key exists. UNVERIFIED against a live key:
    /// flatness of the duration fields (may nest), the weekly discriminator value "10080",
    /// the role of `scope`. Re-probe via `tok doctor` when a key exists (spec 023 §Live).
    const REAL_SHAPE: &[u8] = br#"{
        "usages": [
            { "scope": "FIXED", "limit": "512", "used": "128", "remaining": "384",
              "resetTime": "2026-07-27T00:00:00Z", "duration": "10080", "durationUnit": "TIME_UNIT_MINUTE" },
            { "scope": "FIXED", "limit": "128", "used": "96", "remaining": "32",
              "resetTime": "2026-07-23T15:30:00Z", "duration": "300", "durationUnit": "TIME_UNIT_MINUTE" }
        ]
    }"#;

    // ── spec 023 §B / AC2 — encodes the CORRECT mapping; red against the unimplemented stub ──────

    #[test]
    fn maps_window_entries_to_session_and_weekly_all() {
        let limits =
            parse_quota_response(REAL_SHAPE, "kimi-k2", 75.0, 90.0).expect("parses (well-formed)");
        assert_eq!(
            limits.len(),
            2,
            "the two usages[] entries map to Session + WeeklyAll: {limits:?}"
        );

        let session = limits
            .iter()
            .find(|l| l.kind == LimitKind::Session)
            .expect("a Session limit");
        assert!((session.utilization_pct - 75.0).abs() < 1e-9);
        assert_eq!(
            session.resets_at,
            "2026-07-23T15:30:00Z"
                .parse::<Timestamp>()
                .expect("fixture value parses")
                .to_string(),
            "Kimi provides a real reset instant for the session window (unlike zai) — use it verbatim"
        );
        assert_eq!(session.source, Provenance::Authoritative);

        let weekly = limits
            .iter()
            .find(|l| l.kind == LimitKind::WeeklyAll)
            .expect("a WeeklyAll limit");
        assert!((weekly.utilization_pct - 25.0).abs() < 1e-9);
        assert_eq!(
            weekly.resets_at,
            "2026-07-27T00:00:00Z"
                .parse::<Timestamp>()
                .expect("fixture value parses")
                .to_string()
        );
        assert_eq!(weekly.source, Provenance::Authoritative);
    }

    #[test]
    fn derived_percentage_survives_uneven_division() {
        let body = br#"{"usages":[
            {"limit":"3","used":"1","duration":"300","durationUnit":"TIME_UNIT_MINUTE"}
        ]}"#;
        let limits = parse_quota_response(body, "kimi-k2", 75.0, 90.0).expect("parses");
        assert_eq!(limits.len(), 1);
        assert!(
            (limits[0].utilization_pct - 100.0 / 3.0).abs() < 1e-9,
            "derived % must not round/truncate: {}",
            limits[0].utilization_pct
        );
    }

    #[test]
    fn non_numeric_limit_entry_is_skipped_not_zeroed() {
        let body = br#"{"usages":[
            {"limit":"unlimited","used":"5","duration":"300","durationUnit":"TIME_UNIT_MINUTE"},
            {"limit":"512","used":"128","duration":"10080","durationUnit":"TIME_UNIT_MINUTE"}
        ]}"#;
        let limits = parse_quota_response(body, "kimi-k2", 75.0, 90.0).expect("parses");
        assert_eq!(
            limits.len(),
            1,
            "the non-numeric session entry must be skipped, never rendered as 0%: {limits:?}"
        );
        assert_eq!(limits[0].kind, LimitKind::WeeklyAll);
    }

    #[test]
    fn zero_limit_entry_is_skipped_not_zeroed() {
        let body = br#"{"usages":[
            {"limit":"0","used":"0","duration":"300","durationUnit":"TIME_UNIT_MINUTE"},
            {"limit":"512","used":"128","duration":"10080","durationUnit":"TIME_UNIT_MINUTE"}
        ]}"#;
        let limits = parse_quota_response(body, "kimi-k2", 75.0, 90.0).expect("parses");
        assert_eq!(
            limits.len(),
            1,
            "a zero-limit entry must be skipped, never rendered as a 0% gauge: {limits:?}"
        );
        assert_eq!(limits[0].kind, LimitKind::WeeklyAll);
    }

    #[test]
    fn infinite_limit_entry_is_skipped_not_zeroed() {
        let body = br#"{"usages":[
            {"limit":"Infinity","used":"5","duration":"300","durationUnit":"TIME_UNIT_MINUTE"},
            {"limit":"512","used":"128","duration":"10080","durationUnit":"TIME_UNIT_MINUTE"}
        ]}"#;
        let limits = parse_quota_response(body, "kimi-k2", 75.0, 90.0).expect("parses");
        assert_eq!(
            limits.len(),
            1,
            "an infinite-limit entry must be skipped, never rendered as a confident 0% gauge: {limits:?}"
        );
        assert_eq!(limits[0].kind, LimitKind::WeeklyAll);
    }

    #[test]
    fn infinite_used_entry_is_skipped() {
        let body = br#"{"usages":[
            {"limit":"128","used":"inf","duration":"300","durationUnit":"TIME_UNIT_MINUTE"}
        ]}"#;
        assert!(
            parse_quota_response(body, "kimi-k2", 75.0, 90.0).is_err(),
            "an infinite `used` must be skipped, never rendered as an inf% gauge"
        );
    }

    #[test]
    fn nan_used_entry_is_skipped() {
        let body = br#"{"usages":[
            {"limit":"128","used":"NaN","duration":"300","durationUnit":"TIME_UNIT_MINUTE"}
        ]}"#;
        assert!(
            parse_quota_response(body, "kimi-k2", 75.0, 90.0).is_err(),
            "a NaN `used` must be skipped, never rendered as a NaN% gauge"
        );
    }

    #[test]
    fn missing_used_entry_is_skipped() {
        let body = br#"{"usages":[
            {"limit":"128","duration":"300","durationUnit":"TIME_UNIT_MINUTE"},
            {"limit":"512","used":"128","duration":"10080","durationUnit":"TIME_UNIT_MINUTE"}
        ]}"#;
        let limits = parse_quota_response(body, "kimi-k2", 75.0, 90.0).expect("parses");
        assert_eq!(
            limits.len(),
            1,
            "a missing `used` must be skipped, never rendered as 0%: {limits:?}"
        );
        assert_eq!(limits[0].kind, LimitKind::WeeklyAll);
    }

    #[test]
    fn missing_reset_time_yields_empty_resets_at_not_error() {
        let body = br#"{"usages":[
            {"limit":"128","used":"96","duration":"300","durationUnit":"TIME_UNIT_MINUTE"}
        ]}"#;
        let limits = parse_quota_response(body, "kimi-k2", 75.0, 90.0).expect("parses");
        assert_eq!(limits.len(), 1);
        assert_eq!(
            limits[0].resets_at, "",
            "an absent resetTime must never be fabricated"
        );
    }

    #[test]
    fn unparsable_reset_time_yields_empty_resets_at_not_error() {
        let body = br#"{"usages":[
            {"limit":"128","used":"96","resetTime":"soon","duration":"300","durationUnit":"TIME_UNIT_MINUTE"}
        ]}"#;
        let limits = parse_quota_response(body, "kimi-k2", 75.0, 90.0).expect("parses");
        assert_eq!(limits.len(), 1);
        assert_eq!(
            limits[0].resets_at, "",
            "an unparsable resetTime must degrade to empty, never error"
        );
    }

    #[test]
    fn unknown_entries_are_skipped_defensively() {
        let body = br#"{"usages":[
            {"limit":"128","used":"96","duration":"300","durationUnit":"TIME_UNIT_MINUTE"},
            {"limit":"512","used":"128","duration":"10080","durationUnit":"TIME_UNIT_MINUTE"},
            {"limit":"9999","used":"1","duration":"43200","durationUnit":"TIME_UNIT_MINUTE"}
        ]}"#;
        let limits = parse_quota_response(body, "kimi-k2", 75.0, 90.0).expect("parses");
        assert_eq!(
            limits.len(),
            2,
            "the unrecognized duration entry must be skipped, not errored: {limits:?}"
        );
    }

    #[test]
    fn both_expected_windows_missing_is_an_error() {
        let body = br#"{"usages":[
            {"limit":"9999","used":"1","duration":"43200","durationUnit":"TIME_UNIT_MINUTE"}
        ]}"#;
        assert!(
            parse_quota_response(body, "kimi-k2", 75.0, 90.0).is_err(),
            "schema drift (neither expected window present) must degrade, never render a wrong gauge"
        );
    }

    #[test]
    fn all_entries_with_unusable_numerics_is_an_error() {
        let body = br#"{"usages":[
            {"limit":"unlimited","used":"5","duration":"300","durationUnit":"TIME_UNIT_MINUTE"},
            {"limit":"0","used":"0","duration":"10080","durationUnit":"TIME_UNIT_MINUTE"}
        ]}"#;
        assert!(parse_quota_response(body, "kimi-k2", 75.0, 90.0).is_err());
    }

    #[test]
    fn malformed_body_is_an_error() {
        assert!(parse_quota_response(b"not json", "kimi-k2", 75.0, 90.0).is_err());
    }

    // ── spec 023 §B / AC3 — fetch seam with canned transcripts ────────────────────────────────

    #[tokio::test]
    async fn canned_endpoint_returns_body_on_success() {
        let endpoint = CannedEndpoint {
            canned: Canned::Body(REAL_SHAPE.to_vec()),
        };
        let bytes = endpoint.fetch("fake-key-never-logged").await.expect("ok");
        assert_eq!(bytes, REAL_SHAPE);
    }

    #[tokio::test]
    async fn canned_endpoint_maps_rate_limit() {
        let endpoint = CannedEndpoint {
            canned: Canned::RateLimited,
        };
        assert!(matches!(
            endpoint.fetch("fake-key-never-logged").await,
            Err(AppError::RateLimited)
        ));
    }

    #[tokio::test]
    async fn canned_endpoint_maps_garbage_to_an_error() {
        let endpoint = CannedEndpoint {
            canned: Canned::Fail,
        };
        assert!(endpoint.fetch("fake-key-never-logged").await.is_err());
    }

    #[tokio::test]
    async fn no_secret_appears_in_any_error_or_debug_output() {
        // The key is only ever an argument to `fetch`; nothing on the error path holds or echoes
        // it back (reqwest errors carry the URL, never headers — same discipline as z.ai/Claude).
        let secret = "sk-kimi-super-secret-do-not-leak";
        let endpoint = CannedEndpoint {
            canned: Canned::Fail,
        };
        let err = endpoint.fetch(secret).await.expect_err("canned failure");
        assert!(!format!("{err}").contains(secret));
        assert!(!format!("{err:?}").contains(secret));

        let http = HttpQuotaEndpoint::new().expect("client builds");
        assert!(!format!("{http:?}").contains(secret));
    }
}
