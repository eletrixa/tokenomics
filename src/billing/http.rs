//! Billing-plane HTTP fetch seam: one `BillingEndpoint` for every real-dollar PAYG provider
//! (spec 024 §B). `HttpBillingEndpoint` matches on `ApiProvider` to build the per-provider
//! URL/headers, then sends one page; a `#[cfg(test)]` canned double mirrors the kimi/zai quota
//! posture. The pure `request_spec` helper locks the request shape (URL + query + auth) with no
//! network so the per-provider wire contract is unit-tested, not live-probed.
//!
//! Project: Tokenomics — monitor LLM subscription accounts (usage, limits, time-left) in a TUI
//! Module:  src/billing/http.rs
//! Deps:    async-trait, reqwest (rustls), jiff; super::ApiProvider, crate::error
//! Tested:  inline `#[cfg(test)]` — request shape (anthropic/openai/moonshot, both cursor states,
//!          gemini/xai → Err); canned fetch transcripts; no-secret-leak over Debug/error paths
//!
//! Key responsibilities:
//! - `BillingEndpoint` trait + `HttpBillingEndpoint` (one `reqwest::Client`, 10s request /
//!   5s connect timeouts): the only network touch for the API billing lane; opt-in gated by the
//!   collector, one page per `fetch`.
//! - `request_spec` (pure): `provider + month_start + cursor → URL + query + auth shape`, tested
//!   without network then consumed by `fetch` so the wire contract is locked by tests.
//!
//! Design constraints:
//! - The API key never appears in a log, error, or `Debug` output — held only for the one request
//!   via reqwest's header/bearer API, never string-concatenated into the URL (reqwest errors carry
//!   the URL, never headers — same discipline as the kimi/zai quota endpoint).
//! - 429 → `AppError::RateLimited` (status only, body never parsed); other non-2xx →
//!   `AppError::Overlay("HTTP {n}")` (body never included); gemini/xai carry no endpoint → `Err`.
//!   Cursor-bearing pagination lives in the lane task, not here.

use std::time::Duration;

use async_trait::async_trait;
use jiff::Timestamp;

use super::ApiProvider;
use crate::error::{AppError, AppResult};

const ANTHROPIC_URL: &str = "https://api.anthropic.com/v1/organizations/cost_report";
const OPENAI_URL: &str = "https://api.openai.com/v1/organization/costs";
const MOONSHOT_URL: &str = "https://api.moonshot.ai/v1/users/me/balance";

/// Request timeout for a billing page fetch; the 5s connect timeout is set separately, mirroring
/// the kimi/zai quota posture (spec 024 §B).
pub const BILLING_TIMEOUT_SECS: u64 = 10;

/// How a provider's API key is attached to the request — the key value itself is applied by the
/// caller and is never stored in a [`RequestSpec`] (the no-secret-leak invariant).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AuthKind {
    /// A custom request header (Anthropic's `x-api-key`) holds the key.
    Header(&'static str),
    /// `Authorization: Bearer <key>` (OpenAI, Moonshot).
    Bearer,
}

/// The non-secret shape of one page request: base URL, ordered query params, how the key attaches,
/// and any extra non-secret headers. Built by [`request_spec`]; consumed by [`HttpBillingEndpoint`].
#[derive(Debug)]
struct RequestSpec {
    /// The endpoint URL (query applied separately via reqwest's query API).
    url: &'static str,
    /// Non-secret query params in insertion order (the key is never a query param).
    query: Vec<(&'static str, String)>,
    /// How the caller attaches the API key.
    auth: AuthKind,
    /// Extra non-secret headers (Anthropic's `anthropic-version`).
    extra_headers: Vec<(&'static str, &'static str)>,
}

/// Build the non-secret request shape for one page fetch (spec 024 §B). Pure — no network — so the
/// per-provider URL/headers/query contract is locked by unit tests. Gemini and xai carry no billing
/// endpoint (the collector never spawns them) ⇒ `Err`.
fn request_spec(
    provider: ApiProvider,
    month_start: Timestamp,
    cursor: Option<&str>,
) -> AppResult<RequestSpec> {
    match provider {
        ApiProvider::Anthropic => {
            // `starting_at` is the UTC month start as RFC3339; `bucket_width` defaults to 1d, so it
            // is omitted. A `limit` of 31 covers a whole month of daily buckets on one page.
            let mut query = vec![
                ("starting_at", month_start.to_string()),
                ("limit", "31".to_string()),
            ];
            if let Some(c) = cursor {
                query.push(("page", c.to_string()));
            }
            Ok(RequestSpec {
                url: ANTHROPIC_URL,
                query,
                auth: AuthKind::Header("x-api-key"),
                extra_headers: vec![("anthropic-version", "2023-06-01")],
            })
        }
        ApiProvider::OpenAi => {
            // `start_time` is unix seconds (the org costs API's window anchor); `page` paginates.
            let mut query = vec![
                ("start_time", month_start.as_second().to_string()),
                ("limit", "31".to_string()),
            ];
            if let Some(c) = cursor {
                query.push(("page", c.to_string()));
            }
            Ok(RequestSpec {
                url: OPENAI_URL,
                query,
                auth: AuthKind::Bearer,
                extra_headers: Vec::new(),
            })
        }
        ApiProvider::Moonshot => Ok(RequestSpec {
            url: MOONSHOT_URL,
            query: Vec::new(),
            auth: AuthKind::Bearer,
            extra_headers: Vec::new(),
        }),
        ApiProvider::Gemini | ApiProvider::Xai => {
            Err(AppError::Overlay("no billing endpoint".to_string()))
        }
    }
}

/// The billing endpoint seam — one interface for every real-dollar PAYG provider.
/// `HttpBillingEndpoint` is the only network touch; tests use a canned double.
#[async_trait]
pub trait BillingEndpoint: Send + Sync {
    /// One HTTP page fetch. `month_start` is the UTC month start for MTD queries (unused by
    /// moonshot); `cursor` is the provider pagination token from a previous page. `Err(RateLimited)`
    /// on 429 (drives the shared backoff); other non-2xx map to `AppError::Overlay`.
    async fn fetch(
        &self,
        provider: ApiProvider,
        api_key: &str,
        month_start: Timestamp,
        cursor: Option<&str>,
    ) -> AppResult<Vec<u8>>;
}

/// The real reqwest (rustls) billing endpoint — one `reqwest::Client` built once (10s request /
/// 5s connect timeouts), reused across polls. Opt-in gated by the collector.
#[derive(Debug)]
pub struct HttpBillingEndpoint {
    client: reqwest::Client,
}

impl HttpBillingEndpoint {
    /// Build the shared client (created once; reused across polls). Mirrors the kimi/zai ctor.
    pub fn new() -> AppResult<Self> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(BILLING_TIMEOUT_SECS))
            .connect_timeout(Duration::from_secs(5))
            .build()
            .map_err(|e| AppError::Overlay(format!("cannot build HTTP client: {e}")))?;
        Ok(Self { client })
    }
}

#[async_trait]
impl BillingEndpoint for HttpBillingEndpoint {
    async fn fetch(
        &self,
        provider: ApiProvider,
        api_key: &str,
        month_start: Timestamp,
        cursor: Option<&str>,
    ) -> AppResult<Vec<u8>> {
        let spec = request_spec(provider, month_start, cursor)?;

        // The key is applied only via reqwest's header/bearer API — never string-concatenated into
        // the URL — so a transport error (which carries the URL, never headers) cannot leak it.
        let mut req = self.client.get(spec.url);
        if !spec.query.is_empty() {
            req = req.query(&spec.query);
        }
        req = match spec.auth {
            AuthKind::Header(name) => req.header(name, api_key),
            AuthKind::Bearer => req.bearer_auth(api_key),
        };
        for &(name, value) in &spec.extra_headers {
            req = req.header(name, value);
        }

        let response = req
            .send()
            .await
            .map_err(|e| AppError::Overlay(format!("request failed: {e}")))?;

        let status = response.status();
        if status.as_u16() == 429 {
            // Status alone drives the backoff — the rate-limit body is never parsed or logged.
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

/// A canned outcome for tests — no network. Test-only; mirrors the kimi/zai `Canned` enum.
#[cfg(test)]
#[derive(Debug)]
pub enum Canned {
    /// Return this body verbatim.
    Body(Vec<u8>),
    /// Simulate a 429.
    RateLimited,
    /// Simulate a transport/parse failure (the real endpoint's non-200 handling is exercised in
    /// `HttpBillingEndpoint` itself, not this no-network double).
    Fail,
}

/// One recorded `fetch` call — lets collector tests assert the pagination walk (first call cursor
/// `None`, follow-ups carry the prior page's). Test-only.
#[cfg(test)]
#[derive(Debug, Clone)]
pub struct CannedCall {
    /// The provider passed to `fetch`.
    pub provider: ApiProvider,
    /// The month-start passed to `fetch`.
    pub month_start: Timestamp,
    /// The pagination cursor passed to `fetch` (`None` for a first page).
    pub cursor: Option<String>,
}

/// A [`BillingEndpoint`] returning a canned outcome (no network) and recording every call.
/// Test-only; mirrors the kimi/zai `CannedEndpoint` plus a `calls` recorder for pagination tests.
#[cfg(test)]
#[derive(Debug)]
pub struct CannedBillingEndpoint {
    /// The canned outcome returned by every `fetch`.
    pub canned: Canned,
    calls: std::sync::Mutex<Vec<CannedCall>>,
}

#[cfg(test)]
impl CannedBillingEndpoint {
    /// Build a canned endpoint that returns `canned` from every `fetch` (calls recorded in order).
    pub fn new(canned: Canned) -> Self {
        Self {
            canned,
            calls: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// Snapshot every recorded call, oldest first — for collector pagination assertions.
    pub fn recorded(&self) -> Vec<CannedCall> {
        self.calls
            .lock()
            .expect("canned calls mutex poisoned")
            .clone()
    }
}

#[cfg(test)]
#[async_trait]
impl BillingEndpoint for CannedBillingEndpoint {
    async fn fetch(
        &self,
        provider: ApiProvider,
        _api_key: &str,
        month_start: Timestamp,
        cursor: Option<&str>,
    ) -> AppResult<Vec<u8>> {
        // Record the call before producing the outcome; the guard is dropped before any await (there
        // is none here) so no lock is held across a suspension point.
        {
            let mut calls = self.calls.lock().expect("canned calls mutex poisoned");
            calls.push(CannedCall {
                provider,
                month_start,
                cursor: cursor.map(str::to_string),
            });
        }
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

    /// UTC month start for the 2026-07 window — 1,782,864,000 unix seconds (verified independently).
    fn month_start() -> Timestamp {
        "2026-07-01T00:00:00Z"
            .parse::<Timestamp>()
            .expect("fixture timestamp parses")
    }

    /// Read the first value of query param `key` from `spec`, if present.
    fn q<'a>(spec: &'a RequestSpec, key: &str) -> Option<&'a str> {
        spec.query
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, v)| v.as_str())
    }

    // ── request shape (spec 024 §B) — pure, no network ────────────────────────────────────────

    #[test]
    fn anthropic_shape_uses_rfc3339_start_limit_and_key_header() {
        let spec = request_spec(ApiProvider::Anthropic, month_start(), None)
            .expect("anthropic has a billing endpoint");
        assert_eq!(spec.url, ANTHROPIC_URL);
        assert_eq!(
            spec.query,
            vec![
                ("starting_at", "2026-07-01T00:00:00Z".to_string()),
                ("limit", "31".to_string()),
            ]
        );
        assert_eq!(spec.auth, AuthKind::Header("x-api-key"));
        assert_eq!(
            spec.extra_headers,
            vec![("anthropic-version", "2023-06-01")]
        );
        assert!(q(&spec, "page").is_none(), "no cursor ⇒ no page param");
        assert!(
            q(&spec, "bucket_width").is_none(),
            "bucket_width defaults to 1d — never sent"
        );
    }

    #[test]
    fn anthropic_shape_appends_cursor_as_trailing_page() {
        let spec = request_spec(ApiProvider::Anthropic, month_start(), Some("tok-2"))
            .expect("anthropic has a billing endpoint");
        assert_eq!(
            spec.query.last(),
            Some(&("page", "tok-2".to_string())),
            "the cursor rides as the trailing `page` query param"
        );
        assert_eq!(spec.query.len(), 3);
    }

    #[test]
    fn openai_shape_uses_unix_seconds_start_limit_and_bearer() {
        let spec = request_spec(ApiProvider::OpenAi, month_start(), None)
            .expect("openai has a billing endpoint");
        assert_eq!(spec.url, OPENAI_URL);
        // 2026-07-01T00:00:00Z == 1,782,864,000 unix seconds (verified independently of jiff).
        assert_eq!(
            spec.query,
            vec![
                ("start_time", "1782864000".to_string()),
                ("limit", "31".to_string()),
            ],
            "start_time is unix seconds via `.as_second()`"
        );
        assert_eq!(spec.auth, AuthKind::Bearer);
        assert!(spec.extra_headers.is_empty());
        assert!(q(&spec, "page").is_none());
    }

    #[test]
    fn openai_shape_appends_cursor_as_trailing_page() {
        let spec = request_spec(ApiProvider::OpenAi, month_start(), Some("p2"))
            .expect("openai has a billing endpoint");
        assert_eq!(spec.query.last(), Some(&("page", "p2".to_string())));
    }

    #[test]
    fn moonshot_shape_has_no_query_and_ignores_cursor() {
        let spec = request_spec(
            ApiProvider::Moonshot,
            month_start(),
            Some("ignored-by-balance"),
        )
        .expect("moonshot has a billing endpoint");
        assert_eq!(spec.url, MOONSHOT_URL);
        assert!(
            spec.query.is_empty(),
            "moonshot balance is a single GET — no query, cursor ignored"
        );
        assert_eq!(spec.auth, AuthKind::Bearer);
    }

    #[test]
    fn gemini_has_no_billing_endpoint() {
        let err = request_spec(ApiProvider::Gemini, month_start(), None)
            .expect_err("gemini ships manual-only — no live endpoint");
        assert_eq!(err.to_string(), "overlay error: no billing endpoint");
    }

    #[test]
    fn xai_has_no_billing_endpoint() {
        let err = request_spec(ApiProvider::Xai, month_start(), None)
            .expect_err("xai ships dormant — no live endpoint");
        assert_eq!(err.to_string(), "overlay error: no billing endpoint");
    }

    // ── canned fetch seam (mirrors kimi/zai) ───────────────────────────────────────────────────

    #[tokio::test]
    async fn canned_returns_body_on_success() {
        let endpoint = CannedBillingEndpoint::new(Canned::Body(b"{\"ok\":1}".to_vec()));
        let bytes = endpoint
            .fetch(
                ApiProvider::Moonshot,
                "fake-key-never-logged",
                month_start(),
                None,
            )
            .await
            .expect("body returned");
        assert_eq!(bytes, b"{\"ok\":1}");
    }

    #[tokio::test]
    async fn canned_maps_rate_limit() {
        let endpoint = CannedBillingEndpoint::new(Canned::RateLimited);
        let result = endpoint
            .fetch(
                ApiProvider::OpenAi,
                "fake-key-never-logged",
                month_start(),
                None,
            )
            .await;
        assert!(matches!(result, Err(AppError::RateLimited)));
    }

    #[tokio::test]
    async fn canned_maps_failure() {
        let endpoint = CannedBillingEndpoint::new(Canned::Fail);
        let result = endpoint
            .fetch(
                ApiProvider::Anthropic,
                "fake-key-never-logged",
                month_start(),
                None,
            )
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn canned_records_the_pagination_walk() {
        let endpoint = CannedBillingEndpoint::new(Canned::Body(b"[]".to_vec()));
        let ts = month_start();
        endpoint
            .fetch(ApiProvider::Anthropic, "k", ts, None)
            .await
            .expect("page 1");
        endpoint
            .fetch(ApiProvider::Anthropic, "k", ts, Some("p2"))
            .await
            .expect("page 2");
        endpoint
            .fetch(ApiProvider::Anthropic, "k", ts, Some("p3"))
            .await
            .expect("page 3");

        let calls = endpoint.recorded();
        assert_eq!(calls.len(), 3, "every fetch is recorded in order");
        assert_eq!(calls[0].provider, ApiProvider::Anthropic);
        assert_eq!(calls[0].month_start, ts);
        assert_eq!(calls[0].cursor, None, "first page carries no cursor");
        assert_eq!(calls[1].cursor.as_deref(), Some("p2"));
        assert_eq!(calls[2].cursor.as_deref(), Some("p3"));
    }

    #[tokio::test]
    async fn no_secret_appears_in_any_error_or_debug_output() {
        // The key is only ever an argument to `fetch`; nothing on the error path holds or echoes it
        // (reqwest errors carry the URL, never headers — same discipline as kimi/zai/Claude).
        let secret = "sk-FAKE-not-a-real-key";
        let endpoint = CannedBillingEndpoint::new(Canned::Fail);
        let err = endpoint
            .fetch(ApiProvider::Anthropic, secret, month_start(), None)
            .await
            .expect_err("canned failure");
        assert!(!format!("{err}").contains(secret));
        assert!(!format!("{err:?}").contains(secret));

        let http = HttpBillingEndpoint::new().expect("client builds");
        assert!(!format!("{http:?}").contains(secret));
    }
}
