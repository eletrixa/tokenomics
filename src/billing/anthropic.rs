//! Anthropic Admin API cost-report page parse (spec 024 §B — the real-money plane).
//!
//! Project: Tokenomics — monitor LLM subscription accounts (usage, limits, time-left) in a TUI
//! Module:  src/billing/anthropic.rs
//! Deps:    serde, serde_json; error (AppError/AppResult); crate::billing::parse_cents_decimal
//! Tested:  inline `#[cfg(test)]` on fixtures/billing/anthropic_cost_report_{single,paginated,
//!          empty,malformed}.json + inline-bytes cases (skip rules, cursor rules, drift guard)
//!
//! Key responsibilities:
//! - `parse_cost_report` (pure): one response body → `CostPage { cents, next_page }`. Sums every
//!   `data[].results[]` entry whose `currency` is "USD" (case-insensitive) via
//!   `parse_cents_decimal` (Anthropic amounts are decimal strings ALREADY in cents — no ×100),
//!   and surfaces the pagination cursor.
//!
//! Design constraints:
//! - This is REAL money (a bill), never the notional proxy. Unusable USD amounts skip, never zero;
//!   a page with ≥1 USD result all-unusable ⇒ `Err` (schema drift degrades, never a fabricated sum).
//!   Zero buckets / zero results ⇒ legitimate `Ok(0)` (empty month).
//! - Permissive structs: `#[serde(default)]` on every field, no `deny_unknown_fields` — absence and
//!   extras degrade, never fail a parse (kimi discipline).
//! - `next_page` is `Some` only when `has_more == true` AND the string is non-empty.
//! - Never panics; no `unwrap`/`expect`/`panic!`.

use serde::Deserialize;

use crate::billing::parse_cents_decimal;
use crate::error::{AppError, AppResult};

/// Sum of one page's cost results in integer cents + the pagination cursor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CostPage {
    /// The summed USD amount of the page, in integer cents (per-amount half-even rounding).
    pub cents: i64,
    /// The next-page cursor, present only when `has_more == true` and the string is non-empty.
    pub next_page: Option<String>,
}

/// The cost-report envelope (`{ data, has_more, next_page }`). `next_page`/`has_more` default so an
/// absent pagination block parses as "no more pages"; `data` defaults to empty (empty month).
#[derive(Debug, Deserialize)]
struct CostReport {
    #[serde(default)]
    data: Vec<Bucket>,
    #[serde(default)]
    has_more: bool,
    #[serde(default)]
    next_page: Option<String>,
}

/// One `data[]` time bucket; only `results` is consumed (`starting_at`/`ending_at` are informational).
#[derive(Debug, Deserialize)]
struct Bucket {
    #[serde(default)]
    results: Vec<CostResult>,
}

/// One `results[]` entry. Only `amount`/`currency` are consumed; every field is optional and
/// defaulted so a missing or reshaped field degrades to a skip, never a parse failure. Unknown
/// fields are ignored (no `deny_unknown_fields`).
#[derive(Debug, Deserialize)]
struct CostResult {
    #[serde(default)]
    amount: Option<String>,
    #[serde(default)]
    currency: Option<String>,
}

/// Pure: parse one Anthropic cost-report response body into the page's USD cent sum + cursor.
///
/// `bytes` is one HTTP response body. Every `results[]` entry whose `currency` is "USD"
/// (case-insensitive) is summed via [`parse_cents_decimal`]. A result with a missing/unparsable
/// amount is skipped; if the page carried ≥1 USD result and all were unusable ⇒ [`AppError::Overlay`]
/// (schema drift must degrade, never a fabricated 0). Zero buckets / zero results ⇒ `Ok(0)` (an
/// empty month). `next_page` is `Some` only when `has_more == true` and the cursor is non-empty.
/// Malformed JSON or non-UTF8 ⇒ `Err`. Never panics.
pub fn parse_cost_report(bytes: &[u8]) -> AppResult<CostPage> {
    let response: CostReport = serde_json::from_slice(bytes)
        .map_err(|e| AppError::Overlay(format!("malformed anthropic cost report body: {e}")))?;

    let mut cents: i64 = 0;
    let mut in_scope: usize = 0; // USD results seen (usable or not)
    let mut unusable: usize = 0; // USD results whose amount was missing/unparsable
    for bucket in response.data {
        for result in bucket.results {
            // Out of scope: a non-USD (or currency-less) result is never summed and never counts as
            // unusable — it is simply not this plane's currency (no cross-currency arithmetic).
            let is_usd = result
                .currency
                .as_deref()
                .is_some_and(|c| c.eq_ignore_ascii_case("USD"));
            if !is_usd {
                continue;
            }
            in_scope += 1;
            match result.amount.as_deref().and_then(parse_cents_decimal) {
                Some(c) => cents += c,
                None => unusable += 1, // missing/unparsable amount — skipped, flagged for the guard
            }
        }
    }

    // Schema drift: ≥1 USD result existed but none parsed ⇒ never fabricate a confident 0.
    if in_scope > 0 && unusable == in_scope {
        return Err(AppError::Overlay(
            "anthropic cost report page carried no usable USD amount".to_string(),
        ));
    }

    // Cursor is real only when the server signals more pages AND hands back a non-empty token.
    let next_page = match (response.has_more, response.next_page) {
        (true, Some(cursor)) if !cursor.is_empty() => Some(cursor),
        _ => None,
    };

    Ok(CostPage { cents, next_page })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SINGLE: &[u8] =
        include_bytes!("../../fixtures/billing/anthropic_cost_report_single.json");
    const PAGINATED: &[u8] =
        include_bytes!("../../fixtures/billing/anthropic_cost_report_paginated.json");
    const EMPTY: &[u8] = include_bytes!("../../fixtures/billing/anthropic_cost_report_empty.json");
    const MALFORMED: &[u8] =
        include_bytes!("../../fixtures/billing/anthropic_cost_report_malformed.json");

    // ── fixture matrix (spec 024 §B / AC3) ────────────────────────────────────────────────────

    #[test]
    fn single_page_sums_per_string_rounded_amounts_to_1131_cents() {
        // parse_cents_decimal rounds EACH amount to integer cents (half-even) before summing:
        //   "123.78912" → 124    "1000" → 1000    "0.5" → 0 (half-even tie → even)    "7.25" → 7
        //   sum = 1131. has_more false ⇒ no cursor.
        let page = parse_cost_report(SINGLE).expect("single fixture parses");
        assert_eq!(
            page,
            CostPage {
                cents: 1131,
                next_page: None
            }
        );
    }

    #[test]
    fn paginated_page_surfaces_the_cursor_alongside_its_sum() {
        let page = parse_cost_report(PAGINATED).expect("paginated fixture parses");
        assert_eq!(
            page,
            CostPage {
                cents: 100,
                next_page: Some("page_faketoken".to_string())
            }
        );
    }

    #[test]
    fn empty_month_is_ok_zero_with_no_cursor() {
        let page = parse_cost_report(EMPTY).expect("empty fixture parses");
        assert_eq!(
            page,
            CostPage {
                cents: 0,
                next_page: None
            },
            "an empty month is a legitimate zero, never an error"
        );
    }

    #[test]
    fn malformed_body_is_an_error() {
        assert!(
            parse_cost_report(MALFORMED).is_err(),
            "truncated JSON must degrade, never fabricate a sum"
        );
    }

    #[test]
    fn non_utf8_body_is_an_error() {
        assert!(
            parse_cost_report(&[0xff, 0xfe, 0x00]).is_err(),
            "non-UTF8 bytes cannot be JSON"
        );
    }

    #[test]
    fn empty_byte_stream_is_an_error_not_ok_zero() {
        // Distinct from `{"data":[]}`: a truly empty body is malformed JSON, not an empty month.
        assert!(parse_cost_report(b"").is_err());
    }

    // ── skip-never-zero + drift guard (spec 024 §B) ───────────────────────────────────────────

    #[test]
    fn all_usd_amounts_unusable_is_an_error() {
        let body = br#"{"data":[{"results":[
            {"amount":"not-a-number","currency":"USD"},
            {"amount":"also-garbage","currency":"USD"}
        ]}],"has_more":false}"#;
        assert!(
            parse_cost_report(body).is_err(),
            "≥1 USD result, all unusable ⇒ Err, never a fabricated 0"
        );
    }

    #[test]
    fn missing_amount_usd_result_is_unusable_and_errors_when_alone() {
        let body = br#"{"data":[{"results":[
            {"currency":"USD"}
        ]}],"has_more":false}"#;
        assert!(
            parse_cost_report(body).is_err(),
            "a lone USD result with no amount is unusable ⇒ Err"
        );
    }

    #[test]
    fn mixed_usd_good_and_bad_sums_the_good_skips_the_bad() {
        let body = br#"{"data":[{"results":[
            {"amount":"100","currency":"USD"},
            {"amount":"bad","currency":"USD"}
        ]}],"has_more":false}"#;
        let page = parse_cost_report(body).expect("the good USD result survives");
        assert_eq!(page.cents, 100);
        assert_eq!(page.next_page, None);
    }

    #[test]
    fn zero_usd_amounts_are_usable_not_an_error() {
        // A genuine zero spend is usable (distinct from a corrupt/unparsable amount): not Err, and
        // not counted toward the drift guard.
        let body = br#"{"data":[{"results":[
            {"amount":"0","currency":"USD"},
            {"amount":"0.0","currency":"USD"}
        ]}],"has_more":false}"#;
        let page = parse_cost_report(body).expect("real zeros are usable");
        assert_eq!(
            page,
            CostPage {
                cents: 0,
                next_page: None
            }
        );
    }

    #[test]
    fn non_usd_result_is_skipped_not_summed() {
        let body = br#"{"data":[{"results":[
            {"amount":"1000","currency":"USD"},
            {"amount":"9999","currency":"EUR"}
        ]}],"has_more":false}"#;
        let page = parse_cost_report(body).expect("parses");
        assert_eq!(
            page.cents, 1000,
            "the EUR result must be skipped; only USD is summed"
        );
    }

    #[test]
    fn only_non_usd_results_is_ok_zero() {
        let body = br#"{"data":[{"results":[
            {"amount":"9999","currency":"EUR"}
        ]}],"has_more":false}"#;
        let page =
            parse_cost_report(body).expect("a non-USD-only page is out of scope, not corrupt");
        assert_eq!(
            page,
            CostPage {
                cents: 0,
                next_page: None
            }
        );
    }

    #[test]
    fn missing_currency_is_skipped_not_counted_as_usd() {
        let body = br#"{"data":[{"results":[
            {"amount":"500"}
        ]}],"has_more":false}"#;
        let page = parse_cost_report(body).expect("parses");
        assert_eq!(
            page,
            CostPage {
                cents: 0,
                next_page: None
            },
            "no currency ⇒ not USD ⇒ skipped, not an unusable-USD result"
        );
    }

    #[test]
    fn extra_unknown_fields_are_tolerated() {
        let body = br#"{"data":[{"results":[
            {"amount":"250","currency":"USD","model":"claude-fable-5","weird_extra_field":true,"nested":{"x":1}}
        ]}],"has_more":false,"unknown_top":42}"#;
        let page = parse_cost_report(body).expect("extras must never fail the parse");
        assert_eq!(page.cents, 250);
    }

    // ── cursor rules: Some only when has_more && non-empty ────────────────────────────────────

    #[test]
    fn has_more_false_yields_no_cursor_even_when_next_page_present() {
        let body = br#"{"data":[{"results":[{"amount":"10","currency":"USD"}]}],"has_more":false,"next_page":"page_x"}"#;
        let page = parse_cost_report(body).expect("parses");
        assert_eq!(page.next_page, None, "has_more false ⇒ never a cursor");
        assert_eq!(page.cents, 10);
    }

    #[test]
    fn has_more_true_with_empty_next_page_yields_no_cursor() {
        let body = br#"{"data":[{"results":[{"amount":"10","currency":"USD"}]}],"has_more":true,"next_page":""}"#;
        let page = parse_cost_report(body).expect("parses");
        assert_eq!(
            page.next_page, None,
            "an empty cursor string is not a cursor"
        );
        assert_eq!(page.cents, 10);
    }

    #[test]
    fn has_more_true_with_missing_next_page_yields_no_cursor() {
        let body = br#"{"data":[{"results":[{"amount":"10","currency":"USD"}]}],"has_more":true}"#;
        let page = parse_cost_report(body).expect("parses");
        assert_eq!(
            page.next_page, None,
            "has_more true but no cursor ⇒ None, not a fabricated token"
        );
        assert_eq!(page.cents, 10);
    }
}
