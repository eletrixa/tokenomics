//! Moonshot (Kimi) balance endpoint: pure parse seam (spec 024 §B).
//!
//! Project: Tokenomics — monitor LLM subscription accounts (usage, limits, time-left) in a TUI
//! Module:  src/billing/moonshot.rs
//! Deps:    serde, serde_json
//! Tested:  inline `#[cfg(test)]` — parse (fixture matrix: ok/negative/malformed) + edge cases
//!
//! Key responsibilities:
//! - `parse_balance` (pure): body → one `ApiFigure { kind: Balance }` from `data.available_balance`
//!   (USD, cents via `dollars_f64_to_cents`). Real money — the fourth plane, never mixed with the
//!   notional proxy.
//!
//! Design constraints:
//! - `available_balance` is the only spendable figure consumed (spec §B); `voucher_balance`/
//!   `cash_balance` are ignored. Negative balances are VALID (debt) and pass through.
//! - A balance endpoint with no usable balance is drift, never a silent $0.00: missing/non-finite
//!   `available_balance`, a non-zero wrapper `code`, malformed JSON, or non-UTF8 ⇒ `Err`.
//! - Never panics; no `unwrap`/`expect`/`panic!` in the runtime path.

use serde::Deserialize;

use crate::billing::{dollars_f64_to_cents, ApiFigure, ApiFigureKind};
use crate::error::{AppError, AppResult};

/// Top-level balance envelope. Every field is optional/defaulted so a reshaped or partial body
/// degrades to a named error at the validation seam below, never a serde failure; unknown fields
/// (`status`, `scode`, …) are tolerated.
#[derive(Debug, Deserialize)]
struct BalanceEnvelope {
    #[serde(default)]
    code: Option<i64>,
    #[serde(default)]
    data: Option<BalanceData>,
}

/// The `data` object; only `available_balance` is consumed (spec §B).
#[derive(Debug, Deserialize)]
struct BalanceData {
    #[serde(default)]
    available_balance: Option<f64>,
}

/// Pure. Extracts `data.available_balance` → one `ApiFigure { kind: Balance, amount_minor:
/// dollars_f64_to_cents(v), currency: "USD" }`. `available_balance` missing or non-finite ⇒ `Err`
/// (`AppError::Overlay`, short reason — a balance endpoint with no balance is drift, never a silent
/// $0.00). Negative balances are VALID (debt) and pass through. `voucher_balance`/`cash_balance`
/// are ignored (spec §B: available only). Wrapper `code != 0` when present ⇒ `Err` naming the code.
/// Malformed JSON / non-UTF8 ⇒ `Err`. Never panics.
pub fn parse_balance(bytes: &[u8]) -> AppResult<ApiFigure> {
    let envelope: BalanceEnvelope = serde_json::from_slice(bytes)
        .map_err(|e| AppError::Overlay(format!("malformed moonshot balance body: {e}")))?;

    // A non-zero wrapper `code` signals an error state regardless of the payload — surface it,
    // naming the code, before trusting any figure.
    if let Some(code) = envelope.code.filter(|&c| c != 0) {
        return Err(AppError::Overlay(format!(
            "moonshot balance endpoint returned non-zero code {code}"
        )));
    }

    // A balance endpoint with no usable balance is drift, never a silent $0.00. serde_json already
    // rejects NaN/Infinity as non-JSON (so a deserialized `f64` is always finite); the `is_finite()`
    // filter is defensive belt-and-braces kept per the spec, and missing + non-finite both collapse
    // to the same named error.
    let Some(value) = envelope
        .data
        .and_then(|d| d.available_balance)
        .filter(|v| v.is_finite())
    else {
        return Err(AppError::Overlay(
            "moonshot balance body carried no usable available_balance".to_string(),
        ));
    };

    let Some(amount_minor) = dollars_f64_to_cents(value) else {
        return Err(AppError::Overlay(
            "moonshot available_balance is out of range".to_string(),
        ));
    };
    Ok(ApiFigure {
        kind: ApiFigureKind::Balance,
        amount_minor,
        currency: "USD".to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::billing::ApiFigureKind;

    const OK: &[u8] = include_bytes!("../../fixtures/billing/moonshot_balance_ok.json");
    const NEGATIVE: &[u8] = include_bytes!("../../fixtures/billing/moonshot_balance_negative.json");
    const MALFORMED: &[u8] =
        include_bytes!("../../fixtures/billing/moonshot_balance_malformed.json");

    // ── fixture matrix (spec 024 §B / AC3) ────────────────────────────────────────────────────

    #[test]
    fn ok_fixture_yields_balance_in_cents() {
        let figure = parse_balance(OK).expect("ok fixture parses");
        assert!(matches!(figure.kind, ApiFigureKind::Balance));
        assert_eq!(figure.currency, "USD");
        assert_eq!(
            figure.amount_minor, 4959,
            "49.58894 USD ×100 (round half-even) = 4959 cents"
        );
    }

    #[test]
    fn negative_balance_is_valid_debt_and_passes_through() {
        let figure = parse_balance(NEGATIVE).expect("negative fixture parses");
        assert!(matches!(figure.kind, ApiFigureKind::Balance));
        assert_eq!(
            figure.amount_minor, -250,
            "-2.50 USD = -250 cents; a negative balance is debt, never an error"
        );
    }

    #[test]
    fn malformed_fixture_is_an_error() {
        assert!(
            parse_balance(MALFORMED).is_err(),
            "truncated JSON must error, never fabricate a balance"
        );
    }

    // ── edge cases ───────────────────────────────────────────────────────────────────────────

    #[test]
    fn missing_data_object_is_an_error() {
        assert!(
            parse_balance(br#"{"code":0}"#).is_err(),
            "no data object ⇒ no available_balance ⇒ drift, never $0.00"
        );
    }

    #[test]
    fn missing_available_balance_is_an_error() {
        assert!(
            parse_balance(br#"{"code":0,"data":{}}"#).is_err(),
            "a balance endpoint with no balance is drift, never a silent $0.00"
        );
    }

    #[test]
    fn non_zero_wrapper_code_is_an_error() {
        let err = parse_balance(br#"{"code":1,"data":{"available_balance":5.0}}"#)
            .expect_err("a non-zero code must error, even with a balance present");
        let msg = format!("{err}");
        assert!(msg.contains("code 1"), "error must name the code: {msg}");
    }

    #[test]
    fn unknown_extra_fields_are_tolerated() {
        let body = br#"{"code":0,"status":true,"scode":"0x0","future":"x","data":{"available_balance":1.0,"voucher_balance":2.0,"cash_balance":3.0,"extra":9}}"#;
        let figure = parse_balance(body).expect("unknown fields must not break parsing");
        assert_eq!(figure.amount_minor, 100, "1.00 USD = 100 cents");
        assert!(matches!(figure.kind, ApiFigureKind::Balance));
        assert_eq!(figure.currency, "USD");
    }

    #[test]
    fn non_finite_literal_is_not_json_so_errors_at_parse() {
        // JSON has no NaN/Infinity literal, so the in-process `is_finite()` guard can never be
        // reached through well-formed JSON (a deserialized `f64` is always finite). serde_json
        // rejects such bodies at parse time → Err; the spec points to the missing-field path
        // (`missing_available_balance_is_an_error` above) as the proxy. These two assertions
        // document the parse-time rejection directly.
        assert!(parse_balance(br#"{"code":0,"data":{"available_balance":NaN}}"#).is_err());
        assert!(parse_balance(br#"{"code":0,"data":{"available_balance":Infinity}}"#).is_err());
    }
}
