//! OpenAI `GET /v1/organization/costs` page parser — f64-dollar daily buckets → cents (spec 024 §B).
//!
//! Project: Tokenomics — monitor LLM subscription accounts (usage, limits, time-left) in a TUI
//! Module:  src/billing/openai.rs
//! Deps:    serde (Deserialize), serde_json; `crate::billing::dollars_f64_to_cents`
//! Tested:  inline `#[cfg(test)]` — fixture matrix (single/paginated/empty/malformed) + parse rules
//!
//! Key responsibilities:
//! - `parse_costs` (pure): one `/v1/organization/costs` page body → `CostPage { cents, next_page }`.
//!   Walks `data[].results[].amount` (OpenAI sends `value` as an f64 in DOLLARS), sums the usd ones
//!   into integer cents via `billing::dollars_f64_to_cents`, and surfaces the pagination cursor.
//! - `CostPage`: the per-page accumulator shape (same shape as the anthropic card's `CostPage`;
//!   declared locally so provider parsers stay decoupled — never imported across providers).
//!
//! Design constraints:
//! - Skip-never-zero: a result whose amount/value is missing or non-finite is skipped, never read
//!   as 0; a page that carried ≥1 unreadable result AND summed no usd amount ⇒ `Err`. An empty page
//!   (no buckets/results) is genuinely `Ok(0)`.
//! - Non-usd results are excluded from the USD MTD sum by a deliberate currency filter — skipped,
//!   not a read failure (an absent currency is drift ⇒ unusable); currency is matched
//!   case-insensitively and never converted, never summed across currencies.
//! - Every modeled field is `Option` + `#[serde(default)]`; unknown fields are tolerated (no
//!   `deny_unknown_fields`) so an API field-addition degrades to a skip, never a parse failure.
//! - No network, no I/O, no panics; malformed JSON / non-UTF8 bytes ⇒ `Err(AppError::Overlay)`.

use serde::Deserialize;

use crate::billing::{dollars_f64_to_micro, parse_dollars_decimal_micro};
use crate::error::{AppError, AppResult};

/// One parsed page of OpenAI cost results: the summed usd spend in exact **micro-cents** (the
/// caller accumulates a figure across pages and rounds ONCE — spec 024 §A) and the pagination
/// cursor to the next page, if any. Same shape as the anthropic card's `CostPage`; declared locally
/// so the provider parsers stay decoupled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CostPage {
    /// Summed usd spend across the page's readable `amount.value` figures, in exact micro-cents.
    pub micro: i128,
    /// Cursor for the next page — `Some` only when the wire carried `has_more == true` AND a
    /// non-empty `next_page`; `None` otherwise (empty page, last page, or empty/absent cursor).
    pub next_page: Option<String>,
}

/// Top-level `/v1/organization/costs` envelope. Fields beyond `data`/`has_more`/`next_page` are
/// ignored; every modeled field defaults so a reshaped envelope degrades to empty, never errors.
#[derive(Debug, Deserialize)]
struct CostPageWire {
    #[serde(default)]
    data: Vec<BucketWire>,
    #[serde(default)]
    has_more: bool,
    #[serde(default)]
    next_page: Option<String>,
}

/// One daily cost bucket. Only `results` is consumed; `start_time`/`end_time` are informational.
#[derive(Debug, Deserialize)]
struct BucketWire {
    #[serde(default)]
    results: Vec<ResultWire>,
}

/// One cost result. Only `amount` is consumed; `line_item`/`project_id`/`api_key_id`/`quantity`
/// (and any future field) are tolerated — ignored — so an API addition never breaks the parse.
#[derive(Debug, Deserialize)]
struct ResultWire {
    #[serde(default)]
    amount: Option<AmountWire>,
}

/// The `amount` object. The reference docs type `value` as a JSON number, but the LIVE wire sends
/// a high-precision decimal STRING in dollars (verified 2026-07-25, e.g.
/// `"15.78_762_621_000_000_000_000_000_000"`) — accept both, preferring the exact string path.
#[derive(Debug, Deserialize)]
struct AmountWire {
    #[serde(default)]
    value: Option<ValueWire>,
    #[serde(default)]
    currency: Option<String>,
}

/// `amount.value`: decimal string (live wire) or JSON number (documented shape).
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum ValueWire {
    /// The live wire's decimal string, parsed exactly.
    Text(String),
    /// The documented number form — dollars as f64 (precision bounded by the wire itself).
    Number(f64),
}

impl ValueWire {
    /// Exact micro-cents for this value, `None` when unreadable (non-finite / malformed).
    fn micro_cents(&self) -> Option<i128> {
        match self {
            Self::Text(s) => parse_dollars_decimal_micro(s),
            Self::Number(v) => dollars_f64_to_micro(*v),
        }
    }
}

/// Parse one OpenAI `/v1/organization/costs` page into a summed-cents [`CostPage`] (spec 024 §B).
///
/// Walks `data[].results[].amount`, summing each **usd** `value` (matched case-insensitively, never
/// converted) into integer cents via [`dollars_f64_to_cents`]. A result with a missing or
/// non-finite amount/value is **skipped, never read as 0**; a page that carried ≥1 unreadable
/// result AND summed no usd amount returns `Err(AppError::Overlay)` (skip-never-zero). An empty
/// page (no buckets/results) is genuinely `Ok(0)`. `next_page` is `Some` only when the wire carried
/// `has_more == true` AND a non-empty cursor. Malformed JSON or non-UTF8 bytes ⇒ `Err`. Never panics.
pub fn parse_costs(bytes: &[u8]) -> AppResult<CostPage> {
    let page: CostPageWire = serde_json::from_slice(bytes)
        .map_err(|e| AppError::Overlay(format!("malformed openai costs body: {e}")))?;

    let mut micro: i128 = 0;
    let mut results_summed: usize = 0;
    let mut results_unusable: usize = 0;

    for bucket in &page.data {
        for result in &bucket.results {
            let Some(amount) = result.amount.as_ref() else {
                // No `amount` object at all — unreadable, never a fabricated 0.
                results_unusable += 1;
                continue;
            };

            // Non-usd results are excluded from the USD MTD sum by a deliberate currency filter —
            // skipped, not a read failure. An ABSENT currency is drift, not a foreign currency:
            // unusable, so the all-unusable guard below fires.
            match amount.currency.as_deref() {
                Some(c) if !c.eq_ignore_ascii_case("usd") => continue,
                Some(_) => {}
                None => {
                    results_unusable += 1;
                    continue;
                }
            }

            let Some(value) = amount.value.as_ref() else {
                results_unusable += 1;
                continue;
            };

            // Unreadable values (non-finite number / malformed string) are skipped, never 0.
            let Some(contribution) = value.micro_cents() else {
                results_unusable += 1;
                continue;
            };

            // A page that overflows i128 micro-cents is drift, never a wrapped sum.
            micro = micro
                .checked_add(contribution)
                .ok_or_else(|| AppError::Overlay("openai costs page sum overflowed".to_string()))?;
            results_summed += 1;
        }
    }

    // Skip-never-zero: a page that carried unreadable results but summed no usd amount must not
    // fabricate a confident $0.00. (results_unusable > 0 ⇒ the page carried ≥1 result; an empty
    // page or an all-non-usd page keeps results_unusable == 0 and is a legitimate `Ok(0)`.)
    if results_summed == 0 && results_unusable > 0 {
        return Err(AppError::Overlay(
            "openai costs page carried results but no readable usd amount".to_string(),
        ));
    }

    // `next_page` is surfaced only when the wire genuinely signals more data AND hands a cursor.
    let next_page = match (page.has_more, page.next_page) {
        (true, Some(cursor)) if !cursor.is_empty() => Some(cursor),
        _ => None,
    };

    Ok(CostPage { micro, next_page })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SINGLE: &[u8] = include_bytes!("../../fixtures/billing/openai_costs_single.json");
    /// The LIVE wire shape (verified 2026-07-25): `amount.value` as a decimal STRING.
    const WIRE_STRINGS: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/fixtures/billing/openai_costs_wire_strings.json"
    ));
    const PAGINATED: &[u8] = include_bytes!("../../fixtures/billing/openai_costs_paginated.json");
    const EMPTY: &[u8] = include_bytes!("../../fixtures/billing/openai_costs_empty.json");
    const MALFORMED: &[u8] = include_bytes!("../../fixtures/billing/openai_costs_malformed.json");

    // ── spec 024 §B / AC3 — fixture matrix ─────────────────────────────────────────────────────

    #[test]
    fn sums_three_usd_buckets_to_exact_cents() {
        // 0.06 → 6, 12.5 → 1250, 1.239 → 124 (half-even) ⇒ 1380 cents.
        let page = parse_costs(SINGLE).expect("well-formed page parses");
        assert_eq!(
            page.micro, 1_379_900_000,
            "0.06 + 12.5 + 1.239 dollars = exactly 1379.9 cents in micro (figure-level rounding lands 1380)"
        );
        assert_eq!(page.next_page, None, "has_more=false ⇒ no cursor");
    }

    #[test]
    fn live_wire_string_values_parse_exactly() {
        // The live wire sends dollars as high-precision decimal strings — parsed exactly, no f64:
        // 15.78_762_621 $ = 1578.762621 ¢ = 1_578_762_621 µ¢; 0.00_000_009 $ = 9 µ¢.
        let page = parse_costs(WIRE_STRINGS).expect("live wire shape parses");
        assert_eq!(
            page,
            CostPage {
                micro: 1_578_762_630,
                next_page: None
            }
        );
    }

    #[test]
    fn surfaces_cursor_on_paginated_page() {
        let page = parse_costs(PAGINATED).expect("parses");
        assert_eq!(page.micro, 250_000_000, "single 2.50 bucket ⇒ 250 cents");
        assert_eq!(
            page.next_page.as_deref(),
            Some("page_fakecursor"),
            "has_more=true + non-empty next_page ⇒ cursor surfaced"
        );
    }

    #[test]
    fn empty_page_is_zero_not_an_error() {
        let page = parse_costs(EMPTY).expect("an empty page is a real zero, not an error");
        assert_eq!(page.micro, 0);
        assert_eq!(page.next_page, None);
    }

    #[test]
    fn malformed_body_is_an_error() {
        assert!(
            parse_costs(MALFORMED).is_err(),
            "truncated JSON must degrade to Err, never panic"
        );
    }

    // ── parse rules: skip-never-zero, currency, cursor, permissivity ───────────────────────────

    #[test]
    fn all_unusable_results_is_an_error_not_zero() {
        // Three results, none with a readable usd amount: amount null, value null, value missing.
        let body = br#"{"data":[{"results":[
            {"amount":null},
            {"amount":{"value":null,"currency":"usd"}},
            {"amount":{"currency":"usd"}}
        ]}],"has_more":false,"next_page":null}"#;
        assert!(
            parse_costs(body).is_err(),
            "a page whose results are ALL unreadable must not fabricate a confident $0.00"
        );
    }

    #[test]
    fn missing_currency_is_unusable_and_errors_not_zero() {
        let body = br#"{"data":[{"results":[{"amount":{"value":5.0}}]}],"has_more":false}"#;
        assert!(parse_costs(body).is_err());
    }

    #[test]
    fn non_usd_results_are_skipped_not_summed() {
        // usd 0.06 (6) + eur 99.0 (skipped) + USD 5.0 (500, case-insensitive) ⇒ 506.
        let body = br#"{"data":[{"results":[
            {"amount":{"value":0.06,"currency":"usd"}},
            {"amount":{"value":99.0,"currency":"eur"}},
            {"amount":{"value":5.0,"currency":"USD"}}
        ]}],"has_more":false,"next_page":null}"#;
        let page = parse_costs(body).expect("parses");
        assert_eq!(
            page.micro, 506_000_000,
            "eur skipped; usd + USD summed case-insensitively"
        );
    }

    #[test]
    fn null_cursor_with_has_more_false_is_none() {
        let body = br#"{"data":[{"results":[
            {"amount":{"value":1.0,"currency":"usd"}}
        ]}],"has_more":false,"next_page":null}"#;
        let page = parse_costs(body).expect("parses");
        assert_eq!(page.micro, 100_000_000);
        assert_eq!(
            page.next_page, None,
            "has_more=false ⇒ next_page None even if a cursor value leaked"
        );
    }

    #[test]
    fn has_more_true_with_empty_or_absent_cursor_is_none() {
        let empty_cursor = br#"{"data":[],"has_more":true,"next_page":""}"#;
        let absent_cursor = br#"{"data":[],"has_more":true}"#;
        assert_eq!(
            parse_costs(empty_cursor).expect("parses").next_page,
            None,
            "an empty cursor string ⇒ None"
        );
        assert_eq!(
            parse_costs(absent_cursor).expect("parses").next_page,
            None,
            "an absent cursor ⇒ None"
        );
    }

    #[test]
    fn unknown_fields_are_tolerated() {
        let body = br#"{"object":"page","data":[{"object":"bucket","results":[
            {"object":"organization.costs.result","amount":{"value":1.0,"currency":"usd"},
             "line_item":"total","project_id":"proj_fake123","some_new_field":42,"quantity":7}
        ],"start_time":1730419200,"end_time":1730505600}],"has_more":false,"next_page":null,"extra_top":"x"}"#;
        let page = parse_costs(body).expect("unknown fields must not break the parse");
        assert_eq!(page.micro, 100_000_000);
    }

    #[test]
    fn non_utf8_bytes_are_an_error() {
        assert!(
            parse_costs(&[0xFF, 0xFE, 0x00]).is_err(),
            "non-UTF8 bytes must degrade to Err, never panic"
        );
    }
}
