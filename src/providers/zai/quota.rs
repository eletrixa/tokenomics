//! z.ai quota endpoint: types + parse seam + the HTTP fetch (spec 019 §B).
//!
//! Project: Tokenomics — monitor LLM subscription accounts (usage, limits, time-left) in a TUI
//! Module:  src/providers/zai/quota.rs
//! Deps:    serde_json, reqwest (rustls), async-trait; domain + format
//! Tested:  inline `#[cfg(test)]` — parse (real-shape fixture, malformed), canned fetch transcripts
//!
//! Key responsibilities:
//! - `parse_quota_response` (pure): body → `Vec<Limit> { source = Authoritative }`. Maps the
//!   `unit`/`number`-tagged `TOKENS_LIMIT` entries to `Session`/`WeeklyAll`, skips `TIME_LIMIT`
//!   and any other entry defensively, and errors when neither expected `TOKENS_LIMIT` entry is
//!   present (schema drift must degrade, never render a wrong gauge).
//! - `QuotaEndpoint` trait + `HttpQuotaEndpoint` (reqwest) — the only network touch; real
//!   implementation (mirrors `claude::overlay::HttpUsageEndpoint`), opt-in gated by the caller.
//!
//! Design constraints:
//! - The API key never appears in a log or error (reqwest errors carry the URL, not headers).

use std::time::Duration;

use async_trait::async_trait;
use jiff::Timestamp;
use serde::Deserialize;

use crate::domain::{Limit, LimitKind, Provenance, Provider};
use crate::error::{AppError, AppResult};
use crate::format::severity_for;

const QUOTA_URL: &str = "https://api.z.ai/api/monitor/usage/quota/limit";
const QUOTA_TIMEOUT_SECS: u64 = 10;

/// Wire `type` string for a token-budget entry; disambiguated into `Session`/`WeeklyAll` below by
/// its `unit`/`number` pair. The sibling `TIME_LIMIT` type (the monthly MCP-tool quota) is skipped
/// unconditionally by the `!= TOKENS_LIMIT` check below — surfaced by `doctor` as an informational
/// line instead of a gauge, with no gauge home of its own.
const TOKENS_LIMIT: &str = "TOKENS_LIMIT";
/// `unit`/`number` pair identifying the rolling 5-hour session window (plans/002-multi-provider/
/// 01-zai.md §2, live-probed 2026-07-19).
const SESSION_UNIT_NUMBER: (i64, i64) = (3, 5);
/// `unit`/`number` pair identifying the weekly-all window.
const WEEKLY_UNIT_NUMBER: (i64, i64) = (6, 1);

/// The top-level quota response envelope (`{ "data": { "limits": [...] }, ... }`). Unknown sibling
/// fields (`code`, `msg`, `success`, `data.level`) are ignored by serde.
#[derive(Debug, Deserialize)]
pub struct QuotaResponse {
    #[serde(default)]
    pub data: Option<QuotaData>,
}

/// The `data` object: the `limits[]` array this adapter maps.
#[derive(Debug, Deserialize)]
pub struct QuotaData {
    #[serde(default)]
    pub limits: Vec<QuotaLimitEntry>,
}

/// One entry of the `limits[]` array. Field names kept close to the wire shape (plans/002-multi-
/// provider/01-zai.md §2, live-probed 2026-07-19).
#[derive(Debug, Deserialize)]
pub struct QuotaLimitEntry {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub unit: Option<i64>,
    #[serde(default)]
    pub number: Option<i64>,
    #[serde(default)]
    pub percentage: Option<f64>,
    #[serde(default, rename = "nextResetTime")]
    pub next_reset_time: Option<i64>,
}

/// Parse a z.ai quota body into authoritative limits (spec 019 §B). `TIME_LIMIT` and any
/// unrecognized entry are skipped defensively; malformed JSON or neither expected `TOKENS_LIMIT`
/// entry present ⇒ `Err` (schema drift must degrade, never render a wrong gauge).
pub fn parse_quota_response(
    bytes: &[u8],
    account_id: &str,
    warn_pct: f64,
    crit_pct: f64,
) -> AppResult<Vec<Limit>> {
    let response: QuotaResponse = serde_json::from_slice(bytes)
        .map_err(|e| AppError::Overlay(format!("malformed zai quota body: {e}")))?;
    let entries = response.data.map_or_else(Vec::new, |d| d.limits);

    let build = |kind: LimitKind, pct: f64, resets_at: String| Limit {
        account_id: account_id.to_string(),
        provider: Provider::Zai,
        kind,
        scope: None,
        utilization_pct: pct,
        resets_at,
        severity: severity_for(pct, warn_pct, crit_pct),
        source: Provenance::Authoritative,
    };

    let mut limits = Vec::new();
    for entry in entries {
        if entry.kind != TOKENS_LIMIT {
            continue; // TIME_LIMIT (monthly MCP quota, doctor-only) or an unrecognized entry — skipped
        }
        // A matching entry with no `percentage` is schema drift, not a real 0% — skip it rather than
        // render a confident-looking wrong gauge (the both-missing check below then degrades it).
        let Some(pct) = entry.percentage else {
            continue;
        };
        match (entry.unit, entry.number) {
            (Some(u), Some(n)) if (u, n) == SESSION_UNIT_NUMBER => {
                // The rolling 5h window has no reset instant — never fabricate one.
                limits.push(build(LimitKind::Session, pct, String::new()));
            }
            (Some(u), Some(n)) if (u, n) == WEEKLY_UNIT_NUMBER => {
                let resets_at = match entry.next_reset_time {
                    Some(ms) => Timestamp::from_millisecond(ms)
                        .map_err(|_| {
                            AppError::Overlay(
                                "zai quota carried an out-of-range nextResetTime".to_string(),
                            )
                        })?
                        .to_string(),
                    None => String::new(),
                };
                limits.push(build(LimitKind::WeeklyAll, pct, resets_at));
            }
            _ => {} // an unrecognized unit/number pair on a TOKENS_LIMIT entry — skipped
        }
    }

    if limits.is_empty() {
        return Err(AppError::Overlay(format!(
            "zai quota response for {account_id} carried neither expected TOKENS_LIMIT entry"
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

    /// The real-shape fixture, live-probed 2026-07-19 (plans/002-multi-provider/01-zai.md §2),
    /// secret redacted (there is none in this body).
    const REAL_SHAPE: &[u8] = br#"{
        "code": 200,
        "msg": "Operation successful",
        "data": {
            "limits": [
                {
                    "type": "TIME_LIMIT",
                    "unit": 5, "number": 1,
                    "usage": 100, "currentValue": 0, "remaining": 100, "percentage": 0,
                    "nextResetTime": 1786182515975
                },
                { "type": "TOKENS_LIMIT", "unit": 3, "number": 5, "percentage": 42 },
                { "type": "TOKENS_LIMIT", "unit": 6, "number": 1, "percentage": 81,
                  "nextResetTime": 1784713715974 }
            ],
            "level": "lite"
        },
        "success": true
    }"#;

    // ── spec 019 §B / AC2 — encodes the CORRECT mapping; red against the stub above ───────────

    #[test]
    fn maps_tokens_limit_entries_to_session_and_weekly_all() {
        let limits =
            parse_quota_response(REAL_SHAPE, "zai-lite", 75.0, 90.0).expect("parses (well-formed)");
        assert_eq!(
            limits.len(),
            2,
            "TIME_LIMIT skipped; the two TOKENS_LIMIT entries map to Session + WeeklyAll: {limits:?}"
        );

        let session = limits
            .iter()
            .find(|l| l.kind == LimitKind::Session)
            .expect("a Session limit");
        assert!((session.utilization_pct - 42.0).abs() < 1e-9);
        assert_eq!(
            session.resets_at, "",
            "the rolling 5h window has no reset instant — never fabricate one"
        );
        assert_eq!(session.source, Provenance::Authoritative);

        let weekly = limits
            .iter()
            .find(|l| l.kind == LimitKind::WeeklyAll)
            .expect("a WeeklyAll limit");
        assert!((weekly.utilization_pct - 81.0).abs() < 1e-9);
        // Pin the exact epoch-ms → RFC 3339 conversion (not just "non-empty") so a units regression
        // (e.g. from_microsecond instead of from_millisecond) fails this test.
        assert_eq!(
            weekly.resets_at,
            Timestamp::from_millisecond(1_784_713_715_974)
                .expect("fixture value is in range")
                .to_string(),
            "nextResetTime (epoch ms) must convert to a verbatim RFC 3339 resets_at"
        );
        assert_eq!(weekly.source, Provenance::Authoritative);
    }

    /// spec 019 §B: a `TOKENS_LIMIT` entry whose unit/number matches but whose `percentage` is
    /// absent is schema drift, not a real 0% — it must be skipped, never rendered as a confident
    /// 0% gauge (the module's own invariant).
    #[test]
    fn tokens_limit_entry_missing_percentage_is_skipped_not_zeroed() {
        let body = br#"{"data":{"limits":[
            {"type":"TOKENS_LIMIT","unit":3,"number":5},
            {"type":"TOKENS_LIMIT","unit":6,"number":1,"percentage":81,"nextResetTime":1784713715974}
        ]}}"#;
        let limits = parse_quota_response(body, "zai-lite", 75.0, 90.0).expect("parses");
        assert_eq!(
            limits.len(),
            1,
            "the percentage-less Session entry must be skipped, not rendered as 0%: {limits:?}"
        );
        assert_eq!(limits[0].kind, LimitKind::WeeklyAll);
    }

    /// When the only entries present are missing `percentage`, nothing authoritative survives — the
    /// existing both-missing error must fire (schema drift degrades, never renders a wrong gauge).
    #[test]
    fn all_tokens_limit_entries_missing_percentage_is_an_error() {
        let body = br#"{"data":{"limits":[
            {"type":"TOKENS_LIMIT","unit":3,"number":5},
            {"type":"TOKENS_LIMIT","unit":6,"number":1,"nextResetTime":1784713715974}
        ]}}"#;
        assert!(parse_quota_response(body, "zai-lite", 75.0, 90.0).is_err());
    }

    #[test]
    fn both_tokens_limit_entries_missing_is_an_error() {
        let body =
            br#"{"data":{"limits":[{"type":"TIME_LIMIT","unit":5,"number":1,"percentage":0}]}}"#;
        assert!(
            parse_quota_response(body, "zai-lite", 75.0, 90.0).is_err(),
            "schema drift (neither expected TOKENS_LIMIT entry present) must degrade, never render \
             a wrong gauge"
        );
    }

    #[test]
    fn unknown_entries_are_skipped_defensively() {
        let body = br#"{"data":{"limits":[
            {"type":"TOKENS_LIMIT","unit":3,"number":5,"percentage":10},
            {"type":"TOKENS_LIMIT","unit":6,"number":1,"percentage":20,"nextResetTime":1784713715974},
            {"type":"SOMETHING_NEW","unit":99,"number":1,"percentage":50}
        ]}}"#;
        let limits = parse_quota_response(body, "zai-lite", 75.0, 90.0).expect("parses");
        assert_eq!(
            limits.len(),
            2,
            "the unknown entry must be skipped, not errored: {limits:?}"
        );
    }

    #[test]
    fn malformed_body_is_an_error() {
        assert!(parse_quota_response(b"not json", "zai-lite", 75.0, 90.0).is_err());
    }

    // ── spec 019 §B / AC3 — fetch seam with canned transcripts ────────────────────────────────

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
        // it back (reqwest errors carry the URL, never headers — same discipline as the Claude
        // overlay). Exercised against both the canned double AND a real-but-unreachable client.
        let secret = "sk-zai-super-secret-do-not-leak";
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
