//! API billing plane contracts: real-dollar balances & month-to-date spend (spec 024 §A — the
//! fourth plane, never mixed with the notional-usage proxy, the limits gauges, or the subscription
//! ledger).
//!
//! Project: Tokenomics — monitor LLM subscription accounts (usage, limits, time-left) in a TUI
//! Module:  src/billing.rs
//! Deps:    serde
//! Tested:  in-file (enum round-trips, `ApiAccount` TOML discipline, exact cents parsing)
//!
//! Key responsibilities:
//! - `ApiProvider` / `ApiFigureKind`: the exhaustive enums for API-key (PAYG) accounts and their
//!   figure kinds — a SEPARATE enum from `domain::Provider` (an `[[api]]` account is not a
//!   subscription `[[account]]`).
//! - `ApiFigure` / `ApiAccount`: one normalized money figure and one `[[api]]` config entry.
//! - `parse_cents_decimal`: Anthropic decimal-cents string → exact `i64` cents (i128 micro-cents,
//!   half-even) — NO f64 anywhere in the path.
//! - `dollars_f64_to_cents`: OpenAI/Moonshot f64 dollars → `i64` cents (×100, half-even).
//!
//! Design constraints:
//! - Money is minor units (cents) as `i64`; wire representations differ and each parser owns its
//!   exact conversion (spec 024 §A). `currency` is a verbatim label — no conversion, no
//!   cross-currency arithmetic anywhere.
//! - `ApiAccount` mirrors `domain::Account`'s serde discipline (`deny_unknown_fields`, `active`
//!   defaults true). `api_key_env` is a NAME, never a value.
//! - Submodule declarations (`anthropic`/`http`/`manual`/`moonshot`/`openai`) are sibling-card
//!   files; they resolve at the wave barrier.

use serde::{Deserialize, Serialize};

// Sibling-card files (src/billing/*.rs) — owned by other cards; present at the wave barrier.
pub mod anthropic;
pub mod http;
pub mod manual;
pub mod moonshot;
pub mod openai;

/// The PAYG API provider behind an `[[api]]` account. A separate enum from `domain::Provider`:
/// an API-key account is billed as real money, never monitored as a subscription. Extend this enum
/// to add a billable provider; the compiler then flags every place that must handle it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ApiProvider {
    /// Anthropic (Admin API cost report — decimal strings already in cents).
    Anthropic,
    /// OpenAI (admin costs endpoint — f64 dollars). `OpenAi` serializes to `"openai"`.
    OpenAi,
    /// Google Gemini (AI Studio — manual-only in this spec, no live billing endpoint).
    Gemini,
    /// Moonshot / Kimi (balance endpoint — f64 dollars).
    Moonshot,
    /// xAI (dormant in this spec — no management key on disk).
    Xai,
}

impl ApiProvider {
    /// The stable lowercase identifier used in config, the store, and display.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::OpenAi => "openai",
            Self::Gemini => "gemini",
            Self::Moonshot => "moonshot",
            Self::Xai => "xai",
        }
    }

    /// Parse the stable identifier back into an `ApiProvider` (e.g. when reading a stored row).
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "anthropic" => Some(Self::Anthropic),
            "openai" => Some(Self::OpenAi),
            "gemini" => Some(Self::Gemini),
            "moonshot" => Some(Self::Moonshot),
            "xai" => Some(Self::Xai),
            _ => None,
        }
    }
}

impl std::fmt::Display for ApiProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Which kind of money figure an `ApiFigure` carries (spec 024 §A).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ApiFigureKind {
    /// A prepaid credit balance (Moonshot `available_balance`, or a manual balance row).
    Balance,
    /// An amount owed / due (manual ledger only — no provider owes endpoint in this spec).
    Owed,
    /// Month-to-date spend (Anthropic cost report, OpenAI costs).
    MtdSpend,
}

impl ApiFigureKind {
    /// The stable snake_case identifier used in config, the store, and the manual TOML.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Balance => "balance",
            Self::Owed => "owed",
            Self::MtdSpend => "mtd_spend",
        }
    }

    /// Parse the stable identifier back into an `ApiFigureKind` (e.g. when reading a stored row).
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "balance" => Some(Self::Balance),
            "owed" => Some(Self::Owed),
            "mtd_spend" => Some(Self::MtdSpend),
            _ => None,
        }
    }
}

/// One normalized money figure: a kind, an amount in minor units (cents), and a verbatim currency
/// label. No conversion, no cross-currency arithmetic — `currency` is a display label only.
#[derive(Debug, Clone, PartialEq)]
pub struct ApiFigure {
    /// What this figure measures.
    pub kind: ApiFigureKind,
    /// The amount in minor units (cents) — never a float, never a bill mixed with notional cost.
    pub amount_minor: i64,
    /// Verbatim currency code from the source (`"USD"`, `"EUR"`; OpenAI's `"usd"` uppercased for
    /// display by the parser). No conversion is ever applied.
    pub currency: String,
}

/// One `[[api]]` config entry (spec 024 §A). Mirrors `domain::Account`'s serde discipline:
/// `deny_unknown_fields`, `active` defaults to true, `api_key_env` is a NAME (never a value).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApiAccount {
    /// Stable, unique key among `[[api]]` entries (e.g. `"anthropic-work"`).
    pub id: String,
    /// Optional human display name; defaults to `id` via [`ApiAccount::display_label`].
    #[serde(default)]
    pub label: Option<String>,
    /// The PAYG provider behind this account.
    pub provider: ApiProvider,
    /// The env-var NAME (never the value) holding the API key. Required for the pollable providers,
    /// rejected for gemini, validated in `config::validate` — never logged, printed, or stored.
    #[serde(default)]
    pub api_key_env: Option<String>,
    /// False marks an inactive API account: unmonitored, never fetched. Absent means active.
    #[serde(default = "active_default")]
    pub active: bool,
}

/// Serde default for [`ApiAccount::active`] — absent means active (existing configs unchanged).
fn active_default() -> bool {
    true
}

impl ApiAccount {
    /// The display label: `label` when set, else `id` (mirrors `domain::Account`'s convention).
    pub fn display_label(&self) -> &str {
        self.label.as_deref().unwrap_or(&self.id)
    }
}

/// One cent is `MICRO_PER_CENT` micro-cents (scale 1e-6) — the exact-integer accumulation unit for
/// [`parse_cents_decimal`].
const MICRO_PER_CENT: i128 = 1_000_000;
/// Half a cent in micro-cents — the half-even rounding boundary.
const HALF_CENT_MICRO: i128 = 500_000;
/// Powers of ten for scaling a ≤6-digit fractional part up to micro-cents (index = trailing zeros).
const POW10_6: [i128; 7] = [1, 10, 100, 1_000, 10_000, 100_000, 1_000_000];

/// Round a non-negative micro-cents value (units of `MICRO_PER_CENT` per cent) to whole cents,
/// half-even (banker's rounding): exact halves go to the nearer even cent.
fn round_half_even_micro(micro: i128) -> i128 {
    let q = micro / MICRO_PER_CENT;
    let r = micro % MICRO_PER_CENT;
    // Round up past the half boundary, and on the exact half only when the quotient is odd
    // (half-even: exact halves go to the nearer even cent).
    if r > HALF_CENT_MICRO || (r == HALF_CENT_MICRO && q % 2 != 0) {
        q + 1
    } else {
        q
    }
}

/// Parse a decimal string ALREADY IN CENTS into whole `i64` cents, half-even rounding, with exact
/// integer accumulation and NO f64 anywhere.
///
/// The Anthropic cost report gives decimal strings already denominated in cents (e.g. `"123.78912"`
/// = 123.78912¢ ≈ $1.24). The value is accumulated exactly as i128 micro-cents (scale 1e-6) and
/// rounded half-even to cents once.
///
/// Returns `None` on: empty input; a sign other than exactly one leading `'-'`; non-digits; more
/// than one `'.'`; a fractional part longer than 6 digits (cannot be exact at 1e-6 scale); or
/// overflow of `i64` cents. Accepts `"0"`, `"123"`, `"123.45"`, `"-0.5"`, `"123.78912"` (→ 124).
pub fn parse_cents_decimal(s: &str) -> Option<i64> {
    if s.is_empty() {
        return None;
    }
    // Exactly one optional leading '-'; anything else (including '+') fails the digit check below.
    let (neg, rest) = if let Some(stripped) = s.strip_prefix('-') {
        (true, stripped)
    } else {
        (false, s)
    };
    if rest.is_empty() {
        return None; // "-" alone
    }

    // At most one '.'.
    let (int_str, frac_str) = match rest.split_once('.') {
        Some((int_part, frac_part)) => {
            if frac_part.contains('.') {
                return None; // a second dot
            }
            (int_part, frac_part)
        }
        None => (rest, ""),
    };
    if int_str.is_empty() && frac_str.is_empty() {
        return None; // "." alone
    }
    if !int_str.bytes().all(|b| b.is_ascii_digit()) || !frac_str.bytes().all(|b| b.is_ascii_digit())
    {
        return None; // non-digit: other signs, whitespace, unicode digits, etc.
    }

    // Integer part → cents (i128). Astronomically long inputs overflow i128 here → None.
    let int_cents: i128 = if int_str.is_empty() {
        0
    } else {
        int_str.parse().ok()?
    };

    // Fractional part → micro-cents at scale 1e-6. More than 6 digits can't be exact → None.
    if frac_str.len() > 6 {
        return None;
    }
    let frac_micro: i128 = if frac_str.is_empty() {
        0
    } else {
        let digits: i128 = frac_str.parse().ok()?;
        digits * POW10_6[6 - frac_str.len()]
    };

    let total_micro = int_cents
        .checked_mul(MICRO_PER_CENT)?
        .checked_add(frac_micro)?;

    // Round half-even on the magnitude, then reapply the sign (matches ties-to-even on negatives).
    let cents = round_half_even_micro(total_micro);
    let cents = if neg { -cents } else { cents };
    i64::try_from(cents).ok()
}

/// Convert f64 DOLLARS into whole `i64` cents by ×100 then half-even (banker's) rounding.
///
/// OpenAI (`amount.value`) and Moonshot (`available_balance`) report f64 dollars; precision is
/// bounded by the wire itself, so f64 is acceptable here (unlike [`parse_cents_decimal`]). Returns
/// `None` on a non-finite value (NaN/∞) or `|v| > 9e15/100` (keeps the result within `i64` and
/// within f64's exact-integer range, so the bounded cast is exact).
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub fn dollars_f64_to_cents(v: f64) -> Option<i64> {
    if !v.is_finite() {
        return None;
    }
    if v.abs() > MAX_ABS_DOLLARS {
        return None;
    }
    // `round_ties_even` is the stdlib's half-even; the bound above keeps the result within i64 and
    // within f64's exact-integer range, so the `as i64` is exact — matching the ccusage cast policy.
    let cents = (v * 100.0).round_ties_even();
    Some(cents as i64)
}

/// `9e15 / 100` — the largest dollar magnitude [`dollars_f64_to_cents`] accepts. Keeps cents within
/// `i64::MAX` and within f64's exact-integer range (≤ 2^53).
const MAX_ABS_DOLLARS: f64 = 9e13;

#[cfg(test)]
mod tests {
    use super::*;

    // ---- ApiProvider ----

    #[test]
    fn api_provider_as_str_and_parse_round_trip_all_variants() {
        for original in [
            ApiProvider::Anthropic,
            ApiProvider::OpenAi,
            ApiProvider::Gemini,
            ApiProvider::Moonshot,
            ApiProvider::Xai,
        ] {
            assert_eq!(ApiProvider::parse(original.as_str()), Some(original));
        }
    }

    #[test]
    fn api_provider_openai_is_the_string_openai() {
        // OpenAi lowercased = "openai" (NOT "open_ai"); the config string round-trips.
        assert_eq!(ApiProvider::OpenAi.as_str(), "openai");
        assert_eq!(ApiProvider::parse("openai"), Some(ApiProvider::OpenAi));
    }

    #[test]
    fn api_provider_strings_match_the_contract() {
        assert_eq!(ApiProvider::Anthropic.as_str(), "anthropic");
        assert_eq!(ApiProvider::OpenAi.as_str(), "openai");
        assert_eq!(ApiProvider::Gemini.as_str(), "gemini");
        assert_eq!(ApiProvider::Moonshot.as_str(), "moonshot");
        assert_eq!(ApiProvider::Xai.as_str(), "xai");
    }

    #[test]
    fn api_provider_parse_rejects_unknown_and_wrong_case() {
        assert_eq!(ApiProvider::parse("claude"), None); // subscription provider, not api
        assert_eq!(ApiProvider::parse(""), None);
        assert_eq!(ApiProvider::parse("OpenAI"), None); // case-sensitive
        assert_eq!(ApiProvider::parse("open_ai"), None);
    }

    #[test]
    fn api_provider_display_uses_as_str() {
        assert_eq!(format!("{}", ApiProvider::Anthropic), "anthropic");
        assert_eq!(format!("{}", ApiProvider::OpenAi), "openai");
        assert_eq!(format!("{}", ApiProvider::Xai), "xai");
    }

    #[test]
    fn api_provider_serde_round_trips_all_variants() {
        // Proves the serde lowercase mapping (config-string round-trip) for all five — including
        // that OpenAi ↔ "openai", not "open_ai".
        for original in [
            ApiProvider::Anthropic,
            ApiProvider::OpenAi,
            ApiProvider::Gemini,
            ApiProvider::Moonshot,
            ApiProvider::Xai,
        ] {
            let json = serde_json::to_string(&original).expect("serialize");
            assert_eq!(json, format!("\"{}\"", original.as_str()));
            let back: ApiProvider = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(back, original);
        }
    }

    // ---- ApiFigureKind ----

    #[test]
    fn api_figure_kind_as_str_and_parse_round_trip_all_variants() {
        for original in [
            ApiFigureKind::Balance,
            ApiFigureKind::Owed,
            ApiFigureKind::MtdSpend,
        ] {
            assert_eq!(ApiFigureKind::parse(original.as_str()), Some(original));
        }
    }

    #[test]
    fn api_figure_kind_strings_match_the_contract() {
        assert_eq!(ApiFigureKind::Balance.as_str(), "balance");
        assert_eq!(ApiFigureKind::Owed.as_str(), "owed");
        assert_eq!(ApiFigureKind::MtdSpend.as_str(), "mtd_spend"); // snake_case of MtdSpend
    }

    #[test]
    fn api_figure_kind_parse_rejects_unknown() {
        assert_eq!(ApiFigureKind::parse(""), None);
        assert_eq!(ApiFigureKind::parse("mtd-spend"), None);
        assert_eq!(ApiFigureKind::parse("MTD_SPEND"), None); // case-sensitive
    }

    #[test]
    fn api_figure_kind_serde_round_trips_all_variants() {
        for original in [
            ApiFigureKind::Balance,
            ApiFigureKind::Owed,
            ApiFigureKind::MtdSpend,
        ] {
            let json = serde_json::to_string(&original).expect("serialize");
            assert_eq!(json, format!("\"{}\"", original.as_str()));
            let back: ApiFigureKind = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(back, original);
        }
    }

    // ---- ApiFigure ----

    #[test]
    fn api_figure_constructs_with_minor_units() {
        let fig = ApiFigure {
            kind: ApiFigureKind::MtdSpend,
            amount_minor: 1_234, // $12.34
            currency: "USD".to_string(),
        };
        assert_eq!(fig.kind, ApiFigureKind::MtdSpend);
        assert_eq!(fig.amount_minor, 1_234);
        assert_eq!(fig.currency, "USD");
    }

    // ---- ApiAccount (TOML discipline, mirroring domain::Account) ----

    #[test]
    fn api_account_minimal_parses_with_defaults() {
        let toml = "id = \"anthropic-work\"\nprovider = \"anthropic\"\n";
        let acct: ApiAccount = toml::from_str(toml).expect("parses");
        assert_eq!(acct.id, "anthropic-work");
        assert_eq!(acct.provider, ApiProvider::Anthropic);
        assert_eq!(acct.label, None);
        assert_eq!(acct.api_key_env, None);
        assert!(acct.active); // defaults to true
    }

    #[test]
    fn api_account_full_parses_with_all_fields() {
        let toml = "\
id = \"openai-prod\"\n\
label = \"OpenAI prod\"\n\
provider = \"openai\"\n\
api_key_env = \"OPENAI_ADMIN_API_KEY\"\n\
active = false\n\
";
        let acct: ApiAccount = toml::from_str(toml).expect("parses");
        assert_eq!(acct.id, "openai-prod");
        assert_eq!(acct.label.as_deref(), Some("OpenAI prod"));
        assert_eq!(acct.provider, ApiProvider::OpenAi);
        assert_eq!(acct.api_key_env.as_deref(), Some("OPENAI_ADMIN_API_KEY"));
        assert!(!acct.active);
    }

    #[test]
    fn api_account_rejects_unknown_field() {
        let toml = "id = \"x\"\nprovider = \"anthropic\"\nbogus = 1\n";
        let err = toml::from_str::<ApiAccount>(toml);
        assert!(err.is_err(), "deny_unknown_fields must reject `bogus`");
    }

    #[test]
    fn api_account_parses_every_provider_string() {
        // Config-string round-trip for ALL five variants via TOML (the real config surface).
        for (s, expected) in [
            ("anthropic", ApiProvider::Anthropic),
            ("openai", ApiProvider::OpenAi),
            ("gemini", ApiProvider::Gemini),
            ("moonshot", ApiProvider::Moonshot),
            ("xai", ApiProvider::Xai),
        ] {
            let toml = format!("id = \"x\"\nprovider = \"{s}\"\n");
            let acct: ApiAccount =
                toml::from_str(&toml).unwrap_or_else(|_| panic!("provider {s} should parse"));
            assert_eq!(acct.provider, expected);
        }
    }

    #[test]
    fn api_account_display_label_prefers_label_then_falls_back_to_id() {
        let with_label = ApiAccount {
            id: "x".to_string(),
            label: Some("Pretty".to_string()),
            provider: ApiProvider::Anthropic,
            api_key_env: None,
            active: true,
        };
        assert_eq!(with_label.display_label(), "Pretty");

        let without_label = ApiAccount {
            id: "anthropic-work".to_string(),
            label: None,
            provider: ApiProvider::Anthropic,
            api_key_env: None,
            active: true,
        };
        assert_eq!(without_label.display_label(), "anthropic-work");
    }

    // ---- parse_cents_decimal (exact, no f64) ----

    #[test]
    fn parse_cents_decimal_rounds_half_even_on_the_magnitude() {
        // Listed contract table.
        assert_eq!(parse_cents_decimal("0"), Some(0));
        assert_eq!(parse_cents_decimal("123"), Some(123));
        assert_eq!(parse_cents_decimal("123.45"), Some(123));
        assert_eq!(parse_cents_decimal("123.5"), Some(124)); // half-even: 123 odd → 124
        assert_eq!(parse_cents_decimal("122.5"), Some(122)); // half-even: 122 even → 122
        assert_eq!(parse_cents_decimal("123.78912"), Some(124)); // .78912 > .5 → 124
        assert_eq!(parse_cents_decimal("-0.5"), Some(0)); // half-even: 0 even → 0
        assert_eq!(parse_cents_decimal("-1.5"), Some(-2)); // half-even: 1 odd → 2, neg → -2
    }

    #[test]
    fn parse_cents_decimal_accepts_extra_valid_forms() {
        assert_eq!(parse_cents_decimal("0.5"), Some(0)); // half-even: 0 even
        assert_eq!(parse_cents_decimal(".5"), Some(0));
        assert_eq!(parse_cents_decimal("5."), Some(5));
        assert_eq!(parse_cents_decimal("-123.45"), Some(-123));
        assert_eq!(parse_cents_decimal("0.123456"), Some(0)); // 0.123456¢ rounds to 0
        assert_eq!(parse_cents_decimal("0.500000"), Some(0)); // exactly half → 0
        assert_eq!(parse_cents_decimal("1.500000"), Some(2)); // exactly half → 2
    }

    #[test]
    fn parse_cents_decimal_rejects_malformed() {
        // Listed None cases.
        assert_eq!(parse_cents_decimal(""), None);
        assert_eq!(parse_cents_decimal("1.2.3"), None); // >1 dot
        assert_eq!(parse_cents_decimal("abc"), None); // non-digit
                                                      // Additional exactness/format guards.
        assert_eq!(parse_cents_decimal("-"), None);
        assert_eq!(parse_cents_decimal("."), None);
        assert_eq!(parse_cents_decimal("+5"), None); // sign other than one leading '-'
        assert_eq!(parse_cents_decimal("5-3"), None);
        assert_eq!(parse_cents_decimal("1 2"), None); // interior whitespace
        assert_eq!(parse_cents_decimal("0.1234567"), None); // >6 fractional digits (1e-6 scale)
    }

    #[test]
    fn parse_cents_decimal_rejects_overflow() {
        // ~1e20 cents overflows i64 (max ~9.2e18) → None, with no panic.
        assert_eq!(parse_cents_decimal("99999999999999999999"), None);
        // A stratospheric integer that overflows i128 parsing → None.
        let huge = "9".repeat(40);
        assert_eq!(parse_cents_decimal(&huge), None);
    }

    // ---- dollars_f64_to_cents (f64 dollars → cents, half-even) ----

    #[test]
    fn dollars_f64_to_cents_table() {
        assert_eq!(dollars_f64_to_cents(0.06), Some(6));
        assert_eq!(dollars_f64_to_cents(49.58894), Some(4959)); // .894 > .5 → up
        assert_eq!(dollars_f64_to_cents(0.005), Some(0)); // 0.005*100 = 0.5 → ties to 0
        assert_eq!(dollars_f64_to_cents(0.015), Some(2)); // 0.015*100 = 1.5 → ties to 2
        assert_eq!(dollars_f64_to_cents(-3.005), Some(-300)); // -3.005*100 = -300.5 → ties to -300
        assert_eq!(dollars_f64_to_cents(0.0), Some(0));
        assert_eq!(dollars_f64_to_cents(-0.06), Some(-6));
    }

    #[test]
    fn dollars_f64_to_cents_rejects_non_finite() {
        assert_eq!(dollars_f64_to_cents(f64::NAN), None);
        assert_eq!(dollars_f64_to_cents(f64::INFINITY), None);
        assert_eq!(dollars_f64_to_cents(f64::NEG_INFINITY), None);
    }

    #[test]
    fn dollars_f64_to_cents_rejects_out_of_range() {
        // |v| > 9e15/100 (= 9e13) → None; the boundary itself is accepted.
        assert_eq!(dollars_f64_to_cents(9e13), Some(9_000_000_000_000_000));
        assert_eq!(dollars_f64_to_cents(-9e13), Some(-9_000_000_000_000_000));
        assert_eq!(dollars_f64_to_cents(9e13 + 1.0), None);
        assert_eq!(dollars_f64_to_cents(-(9e13 + 1.0)), None);
    }

    // ---- half-even helper sanity ----

    #[test]
    fn round_half_even_micro_ties_to_even() {
        assert_eq!(round_half_even_micro(0), 0);
        assert_eq!(round_half_even_micro(500_000), 0); // 0.5¢ → 0 (even)
        assert_eq!(round_half_even_micro(1_500_000), 2); // 1.5¢ → 2 (even)
        assert_eq!(round_half_even_micro(2_500_000), 2); // 2.5¢ → 2 (even)
        assert_eq!(round_half_even_micro(123_789_120), 124); // 123.78912¢ → 124
        assert_eq!(round_half_even_micro(499_999), 0); // < half → down
        assert_eq!(round_half_even_micro(500_001), 1); // > half → up
    }
}
