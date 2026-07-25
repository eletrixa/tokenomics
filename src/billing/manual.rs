//! The manual API-billing lane: a read-through-only TOML source of human-entered dollar figures
//! (balance / owed / month-to-date spend) for API accounts that have no live billing endpoint —
//! the fourth data plane, never conflated with ccusage usage, the opt-in overlay, or the ledger.
//!
//! Project: Tokenomics — monitor LLM subscription accounts (usage, limits, time-left) in a TUI
//! Module:  src/billing/manual.rs
//! Deps:    jiff (civil::Date), toml + serde (parsing), crate::billing::{ApiFigureKind,
//!          dollars_f64_to_cents} (pinned sibling contract, spec 024 §A)
//! Tested:  inline `#[cfg(test)]` below — parse() row/file degradation (spec 024 §C acceptance 4),
//!          path resolution, hot reload via the injectable `ApiManualSource` seam, content-hash
//!          no-op, the 35-day staleness boundary, exhaustive-destructure PII guarantee
//!
//! Key responsibilities:
//! - `ApiManualRow`: one manual figure — `id` (joins `[[api]].id` exact + case-sensitive), `figure`,
//!   `amount_minor` (cents, i64), `currency` (verbatim label), `as_of` (ISO date, verbatim),
//!   `notes` (optional). There is deliberately NO email/account-name field: any PII the external
//!   schema might carry simply has no place to land here — the exhaustive-destructure test enforces
//!   it (ledger discipline, spec 024 §F).
//! - `parse`: pure `&str` → `ParseOutcome`. Per-row degradation (one malformed row drops only that
//!   row, reported via `ParseOutcome::errors`); `Err` is reserved for a WHOLLY unparseable file.
//! - `ApiManual`: the stateful, keep-last-good loader behind the injectable `ApiManualSource` trait,
//!   so hot reload never touches the filesystem in tests (mirrors `ledger::Ledger` 1:1).
//! - `resolve_path`: `TOKENOMICS_API_MANUAL` env > `[settings] api_manual_path` > off. Pure.
//! - `row_is_stale`: pure `as_of` + `today` → older than `MANUAL_STALE_DAYS` (35) ⇒ stale. `as_of`
//!   is parse-validated at `parse()` time, so an unparsable date degrades its row (never a silent
//!   fresh); this helper assumes a validated `as_of`.
//!
//! Design constraints:
//! - Read-through only: TUI-side, never persisted to SQLite, never fetched over the network. The
//!   collector never reads this file (spec 024 §C). Mirrors the ledger's third-plane discipline.
//! - No panic/unwrap/expect on any content — a mid-edit (or hand-authored, malformed) file must
//!   never crash or blank the board, only degrade to `Stale`/`Missing` + a doctor-visible reason.
//! - Money is minor units (cents) as `i64`; the float→cents conversion is the pinned sibling
//!   `dollars_f64_to_cents` (spec 024 §A). No cross-currency arithmetic anywhere.

use std::path::PathBuf;

use jiff::civil::Date;
use serde::Deserialize;

use crate::billing::{dollars_f64_to_cents, ApiFigureKind};

/// Env var overriding the manual path (empty ⇒ ignored) — mirrors `TOKENOMICS_LEDGER`.
pub const API_MANUAL_ENV: &str = "TOKENOMICS_API_MANUAL";

/// A manual row whose `as_of` is strictly older than this many days is stale (spec 024 §C). Pinned
/// boundary: `as_of + 35 days == today` is still fresh; day 36 is the first stale day (see
/// `row_is_stale`).
pub const MANUAL_STALE_DAYS: i64 = 35;

/// One clean manual row. `as_of` is the verbatim ISO-date string from the file (rendered verbatim,
/// never re-formatted); `amount_minor` is cents. There is deliberately no email/account-name field:
/// the external schema's PII never gets a place to land (spec 024 §C/§F).
#[derive(Debug, Clone, PartialEq)]
pub struct ApiManualRow {
    /// Stable join key, matched exactly + case-sensitively against `[[api]].id` (spec 024 §C).
    pub id: String,
    /// Which dollar figure this row is: balance / owed / month-to-date spend.
    pub figure: ApiFigureKind,
    /// The amount in minor units (cents). Negative is valid (e.g. an owed balance).
    pub amount_minor: i64,
    /// Verbatim currency label (`"USD"`, `"EUR"`, …); never converted or summed across currencies.
    pub currency: String,
    /// The verbatim ISO date (`YYYY-MM-DD`) the figure was recorded; drives staleness.
    pub as_of: String,
    /// Optional free-text note, rendered verbatim.
    pub notes: Option<String>,
}

/// One row that failed to parse, kept ONLY for `tok doctor` — never for rendering (spec 024 §C/§E).
/// `id` is best-effort: present when the row at least had a readable `id`, so a bad `figure`/amount/
/// date on an otherwise-identifiable row is still diagnosable by which account it belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowError {
    /// The row's `id`, when present even though some other field was malformed.
    pub id: Option<String>,
    /// A human-readable reason (e.g. `"unknown figure 'bogus'"`, `"missing amount"`).
    pub reason: String,
}

/// The result of one `parse()` call: rows that parsed clean, plus rows that didn't (with reason).
/// Not `Eq`: `ApiManualRow` carries an `ApiFigureKind` the pinned contract only guarantees is
/// `PartialEq` (mirrors `ledger::ParseOutcome`, minus `Eq`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ParseOutcome {
    /// Every row that parsed cleanly.
    pub rows: Vec<ApiManualRow>,
    /// Every row that degraded (dropped), with its reason — surfaced by `tok doctor`, never rendered.
    pub errors: Vec<RowError>,
}

/// Where the manual lane's current read came from — carried once per read, not per row (spec 024
/// §C). `Off` renders nothing anywhere in the TUI; the other three degrade visibly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiManualProvenance {
    /// Not configured (`TOKENOMICS_API_MANUAL` and `[settings] api_manual_path` both unset). The
    /// state of an `ApiManual` that is never `poll`ed.
    Off,
    /// The last read parsed (the file may still hold rows that failed per-row — see `errors`).
    Fresh,
    /// A configured path's last successful parse is being retained after a failed re-parse (a
    /// mid-edit file), or nothing has ever parsed yet.
    Stale,
    /// A configured path could not be read at all (absent, permissions, …).
    Missing,
}

/// Raw `[[api_manual]]` shape for structural (whole-document) deserialization only — every field
/// is loose/optional so a single bad row never fails the whole document; per-row semantic
/// validation (id/figure/amount/currency/as_of present and well-formed) happens in `parse` after
/// this succeeds. No `deny_unknown_fields`: unrecognized keys are silently ignored by serde's
/// default struct behaviour — never an error (and crucially, any PII key has no field to land in).
#[derive(Debug, Deserialize, Default)]
struct RawApiManualFile {
    #[serde(default)]
    api_manual: Vec<RawApiManualRow>,
}

#[derive(Debug, Deserialize, Default)]
struct RawApiManualRow {
    id: Option<String>,
    figure: Option<String>,
    /// Read permissively so a present-but-non-numeric amount degrades its ROW (per-row "unparsable
    /// amount"), not the whole document. Only TOML floats are accepted dollars (spec 024 §C).
    amount: Option<toml::Value>,
    currency: Option<String>,
    as_of: Option<String>,
    notes: Option<String>,
}

/// Parse a manual-row `figure` string into an `ApiFigureKind`. Depends on the pinned sibling
/// contract exposing `parse(&str) -> Option<Self>` — the `as_str`/`parse` "like Provider" shape
/// spec 024 §A pins for this enum, and the shape `ledger` uses for `SubStatus`. `None` ⇒ a per-row
/// "bad figure" error (never silently coerced).
fn parse_figure(s: &str) -> Option<ApiFigureKind> {
    ApiFigureKind::parse(s)
}

/// Coerce a raw TOML amount value to f64 dollars. The manual amount is a TOML **float** (dollars)
/// per spec 024 §C / the file format; any other present value (integer, string, bool, …) is an
/// "unparsable amount" — `None`, which `parse` turns into a per-row error. Accepting only floats
/// keeps the int→float cast (and its `cast_precision_loss` lint) out of the path entirely.
fn amount_dollars(v: &toml::Value) -> Option<f64> {
    if let toml::Value::Float(d) = v {
        Some(*d)
    } else {
        None
    }
}

/// Parse an `as_of` string as a strict `YYYY-MM-DD` calendar date. Hand-rolled (no jiff format
/// dependency) so a precise, id-naming reason comes back for a malformed date. Mirrors
/// `ledger::to_date`'s validate-then-degrade discipline.
fn parse_as_of_date(s: &str) -> Result<Date, String> {
    let mut parts = s.split('-');
    let y = parts.next();
    let mo = parts.next();
    let d = parts.next();
    let extra = parts.next();
    let (Some(year_str), Some(month_str), Some(day_str), None) = (y, mo, d, extra) else {
        return Err(format!("as_of '{s}' is not a YYYY-MM-DD date"));
    };
    let year = year_str
        .parse::<i16>()
        .map_err(|_| format!("as_of '{s}' has a bad year"))?;
    let month = month_str
        .parse::<i8>()
        .map_err(|_| format!("as_of '{s}' has a bad month"))?;
    let day = day_str
        .parse::<i8>()
        .map_err(|_| format!("as_of '{s}' has a bad day"))?;
    Date::new(year, month, day).map_err(|_| format!("as_of '{s}' is not a valid calendar date"))
}

/// Pure parse of manual TOML text. `Ok` even when individual rows are malformed — those degrade
/// into `ParseOutcome::errors` and are dropped from `rows`, never rendered, never causing the whole
/// read to fail. `Err` is reserved for a WHOLLY unparseable file (bad TOML syntax, or an
/// `[[api_manual]]` shape that doesn't even deserialize) — the caller (`ApiManual::poll`) keeps its
/// last-good rows and marks the plane `Stale` rather than blanking the dashboard.
pub fn parse(text: &str) -> Result<ParseOutcome, String> {
    let raw: RawApiManualFile = toml::from_str(text).map_err(|e| e.to_string())?;
    let mut rows = Vec::new();
    let mut errors = Vec::new();
    for row in raw.api_manual {
        let Some(id) = row.id else {
            errors.push(RowError {
                id: None,
                reason: "missing id".to_string(),
            });
            continue;
        };
        let Some(figure_str) = row.figure else {
            errors.push(RowError {
                id: Some(id.clone()),
                reason: "missing figure".to_string(),
            });
            continue;
        };
        let Some(figure) = parse_figure(&figure_str) else {
            errors.push(RowError {
                id: Some(id.clone()),
                reason: format!("unknown figure '{figure_str}'"),
            });
            continue;
        };
        let amount_minor = match row.amount {
            None => {
                errors.push(RowError {
                    id: Some(id.clone()),
                    reason: "missing amount".to_string(),
                });
                continue;
            }
            Some(v) => {
                let Some(cents) = amount_dollars(&v).and_then(dollars_f64_to_cents) else {
                    errors.push(RowError {
                        id: Some(id.clone()),
                        reason: "amount is not a usable decimal number".to_string(),
                    });
                    continue;
                };
                cents
            }
        };
        let Some(currency) = row.currency else {
            errors.push(RowError {
                id: Some(id.clone()),
                reason: "missing currency".to_string(),
            });
            continue;
        };
        let Some(as_of) = row.as_of else {
            errors.push(RowError {
                id: Some(id.clone()),
                reason: "missing as_of".to_string(),
            });
            continue;
        };
        if parse_as_of_date(&as_of).is_err() {
            errors.push(RowError {
                id: Some(id.clone()),
                reason: format!("as_of '{as_of}' is not a YYYY-MM-DD date"),
            });
            continue;
        }
        rows.push(ApiManualRow {
            id,
            figure,
            amount_minor,
            currency,
            as_of,
            notes: row.notes,
        });
    }
    Ok(ParseOutcome { rows, errors })
}

/// Resolve the manual path per spec 024 §C: `TOKENOMICS_API_MANUAL` (non-empty) beats a non-empty
/// `[settings] api_manual_path` beats off. Pure (values passed in, mirrors `ledger::resolve_path`)
/// so it's table-testable without touching the environment; no hardcoded default path exists.
pub fn resolve_path(
    env_val: Option<&str>,
    settings_path: Option<&std::path::Path>,
) -> Option<PathBuf> {
    if let Some(env) = env_val {
        if !env.is_empty() {
            return Some(PathBuf::from(env));
        }
    }
    if let Some(settings) = settings_path {
        if !settings.as_os_str().is_empty() {
            return Some(settings.to_path_buf());
        }
    }
    None
}

/// Whether a manual row's `as_of` is stale: strictly older than `MANUAL_STALE_DAYS` (35). Pinned
/// boundary — `as_of + 35 days < today ⇒ stale`, so day 36 is the first stale day and day 35
/// exactly is fresh. A future `as_of` reads as fresh (it isn't stale). Pure, `today` injected.
///
/// `as_of` reaching here is always parse-validated at `parse()` time (an unparsable date degrades
/// its row there, never a silent fresh); the unparsable branch below is defensive only.
pub fn row_is_stale(as_of: &str, today: Date) -> bool {
    let Ok(date) = parse_as_of_date(as_of) else {
        return false;
    };
    today
        .since(date)
        .is_ok_and(|span| i64::from(span.get_days()) > MANUAL_STALE_DAYS)
}

/// A source of manual-lane text, injectable so tests never touch the filesystem — mirrors
/// `ledger::LedgerSource`. `None` means unreadable/absent (drives `ApiManualProvenance::Missing`).
pub trait ApiManualSource {
    /// Read the current manual content, or `None` when the path can't be read right now.
    fn read(&mut self) -> Option<String>;
}

/// Production `ApiManualSource`: a fresh `read_to_string` of `path` each poll. The content-hash /
/// keep-last-good discipline lives in `ApiManual::poll`, not here — this stays a thin file read
/// (mirrors `ledger::FileLedgerSource`).
#[derive(Debug)]
pub struct FileApiManualSource {
    path: PathBuf,
}

impl FileApiManualSource {
    /// Watch `path` for manual content.
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

impl ApiManualSource for FileApiManualSource {
    fn read(&mut self) -> Option<String> {
        std::fs::read_to_string(&self.path).ok()
    }
}

/// The stateful, keep-last-good manual reader the TUI polls once per tick (spec 024 §C). Starts
/// `Off` and only ever changes state via `poll` (mirrors `ledger::Ledger` 1:1).
#[derive(Debug)]
pub struct ApiManual {
    rows: Vec<ApiManualRow>,
    errors: Vec<RowError>,
    provenance: ApiManualProvenance,
    /// Content-hash of the last `read()` this loader acted on, so an unchanged read (including a
    /// persistently-missing file) is a cheap no-op rather than a re-parse (mirrors
    /// `ledger::Ledger`'s content-hash discipline).
    last_seen: LastSeen,
    /// The whole-file parse error from the last failed re-parse (the `Stale`-causing `Err(reason)`
    /// from `parse`), so a wholly unparseable file is still diagnosable by `tok doctor`. `None`
    /// once a read succeeds again.
    stale_reason: Option<String>,
}

/// `ApiManual::last_seen`'s three states — never polled, last content unreadable/absent, or last
/// content's hash — kept as a named enum rather than `Option<Option<u64>>` (clippy pedantic).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum LastSeen {
    #[default]
    Never,
    Missing,
    Hash(u64),
}

impl ApiManual {
    /// A fresh, unpolled loader (`Off`, zero rows) — the caller only calls `poll` once a path has
    /// been resolved via `resolve_path`; an unconfigured plane is never polled at all.
    pub fn new() -> Self {
        Self {
            rows: Vec::new(),
            errors: Vec::new(),
            provenance: ApiManualProvenance::Off,
            last_seen: LastSeen::Never,
            stale_reason: None,
        }
    }

    /// The last-good parsed rows (kept across a failed re-parse — see `ApiManualProvenance::Stale`).
    pub fn rows(&self) -> &[ApiManualRow] {
        &self.rows
    }

    /// Rows that failed to parse on the last successful read (for `tok doctor`; never rendered).
    pub fn errors(&self) -> &[RowError] {
        &self.errors
    }

    /// The plane's current provenance.
    pub fn provenance(&self) -> ApiManualProvenance {
        self.provenance
    }

    /// The whole-file parse error behind the current `Stale` provenance, when there is one — so a
    /// wholly unparseable file is diagnosable by `tok doctor`, not just "stale" with no reason.
    /// `None` when `provenance()` isn't `Stale` for a parse-error reason.
    pub fn stale_reason(&self) -> Option<&str> {
        self.stale_reason.as_deref()
    }

    /// Poll `source` for the current content and fold the result in: a readable + parseable file
    /// becomes `Fresh` (rows replaced); a readable-but-unparseable file keeps the last-good rows
    /// and becomes `Stale`; an unreadable/absent file becomes `Missing` (last-good rows also kept
    /// — a transient read failure must never blank the dashboard). Mirrors `ledger::Ledger::poll`'s
    /// keep-last-good discipline, but for display data only. A no-op when the content hash is
    /// unchanged since the last poll.
    pub fn poll<S: ApiManualSource>(&mut self, source: &mut S) {
        let content = source.read();
        let seen = match &content {
            Some(text) => LastSeen::Hash(hash_bytes(text)),
            None => LastSeen::Missing,
        };
        if self.last_seen == seen {
            return; // unchanged since the last poll — nothing to re-parse
        }
        self.last_seen = seen;
        let Some(text) = content else {
            self.provenance = ApiManualProvenance::Missing;
            return;
        };
        match parse(&text) {
            Ok(outcome) => {
                self.rows = outcome.rows;
                self.errors = outcome.errors;
                self.provenance = ApiManualProvenance::Fresh;
                self.stale_reason = None;
            }
            Err(reason) => {
                self.provenance = ApiManualProvenance::Stale;
                self.stale_reason = Some(reason);
            }
        }
    }
}

/// Hash manual text with `std`'s `DefaultHasher` — the content-change trigger for
/// `ApiManual::poll`, mirroring `ledger::hash_bytes`. In-process only, never persisted.
fn hash_bytes(text: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    // Fixture bodies are `include_str!`'d at compile time — no filesystem touch at runtime, per
    // the card (the files exist for doctor/integration reuse later).
    const GOOD: &str = include_str!("../../fixtures/billing/api_manual_good.toml");
    const PARTIAL: &str = include_str!("../../fixtures/billing/api_manual_partial.toml");
    const BROKEN: &str = include_str!("../../fixtures/billing/api_manual_broken.toml");

    // ── AC4: parse() — good / partial / broken fixtures ────────────────────────────────────────

    #[test]
    fn parse_good_fixture_yields_three_rows_across_figures_and_currencies() {
        let outcome = parse(GOOD).expect("a well-formed manual file parses");
        assert!(
            outcome.errors.is_empty(),
            "no malformed rows in the good fixture: {:?}",
            outcome.errors
        );
        assert_eq!(outcome.rows.len(), 3, "rows: {:?}", outcome.rows);

        let anthropic = find(&outcome.rows, "anthropic-api").expect("anthropic-api row");
        assert_eq!(anthropic.figure, ApiFigureKind::Balance);
        assert_eq!(anthropic.amount_minor, 4012);
        assert_eq!(anthropic.currency, "USD");
        assert_eq!(anthropic.as_of, "2026-07-20");
        assert_eq!(anthropic.notes.as_deref(), Some("console credit"));

        let openai = find(&outcome.rows, "openai-api").expect("openai-api row");
        assert_eq!(openai.figure, ApiFigureKind::MtdSpend);
        assert_eq!(openai.amount_minor, 1250);
        assert_eq!(
            openai.notes, None,
            "a row without notes parses with notes=None"
        );

        let moonshot = find(&outcome.rows, "moonshot-api").expect("moonshot-api row");
        assert_eq!(moonshot.figure, ApiFigureKind::Owed);
        assert_eq!(moonshot.amount_minor, 888);
        assert_eq!(
            moonshot.currency, "EUR",
            "the EUR row must round-trip its currency"
        );
    }

    #[test]
    fn parse_partial_fixture_keeps_good_rows_and_reports_each_bad_one() {
        let outcome = parse(PARTIAL).expect("the file itself is well-formed TOML");
        assert_eq!(
            outcome
                .rows
                .iter()
                .map(|r| r.id.as_str())
                .collect::<Vec<_>>(),
            vec!["anthropic-api", "gemini-api"],
            "only the two clean rows survive"
        );
        assert_eq!(outcome.errors.len(), 2, "errors: {:?}", outcome.errors);

        let bad_figure = outcome
            .errors
            .iter()
            .find(|e| e.id.as_deref() == Some("moonshot-api"))
            .expect("an error naming moonshot-api");
        assert!(
            bad_figure.reason.contains("figure") || bad_figure.reason.contains("bogus"),
            "reason should name the bad figure: {bad_figure:?}"
        );

        let missing_as_of = outcome
            .errors
            .iter()
            .find(|e| e.id.as_deref() == Some("openai-api"))
            .expect("an error naming openai-api");
        assert!(
            missing_as_of.reason.contains("as_of"),
            "reason should name the missing as_of: {missing_as_of:?}"
        );
    }

    #[test]
    fn parse_broken_fixture_is_a_whole_file_error() {
        // Not valid TOML — the whole-file failure path (ApiManual::poll turns this into
        // keep-last-good + Stale, tested below).
        assert!(parse(BROKEN).is_err());
    }

    #[test]
    fn empty_manual_parses_to_zero_rows_no_errors() {
        let outcome = parse("").expect("an empty file is valid (empty) TOML");
        assert!(outcome.rows.is_empty());
        assert!(outcome.errors.is_empty());
    }

    // ── per-row degradation: every failure mode names the id, rest survive ─────────────────────

    #[test]
    fn unknown_figure_degrades_only_that_row() {
        let toml = "\
[[api_manual]]
id = \"anthropic-api\"
figure = \"balance\"
amount = 1.00
currency = \"USD\"
as_of = \"2026-07-20\"

[[api_manual]]
id = \"openai-api\"
figure = \"nonsense\"
amount = 2.00
currency = \"USD\"
as_of = \"2026-07-21\"
";
        let outcome = parse(toml).expect("well-formed TOML");
        assert_eq!(
            outcome
                .rows
                .iter()
                .map(|r| r.id.as_str())
                .collect::<Vec<_>>(),
            vec!["anthropic-api"]
        );
        assert_eq!(outcome.errors.len(), 1);
        assert_eq!(outcome.errors[0].id.as_deref(), Some("openai-api"));
        assert!(
            outcome.errors[0].reason.contains("nonsense")
                || outcome.errors[0].reason.contains("figure"),
            "{:?}",
            outcome.errors[0]
        );
    }

    #[test]
    fn unparsable_amount_degrades_only_that_row() {
        // A present-but-non-float amount is a per-row "unparsable amount" — not a whole-file failure.
        let toml = "\
[[api_manual]]
id = \"anthropic-api\"
figure = \"balance\"
amount = \"lots\"
currency = \"USD\"
as_of = \"2026-07-20\"

[[api_manual]]
id = \"openai-api\"
figure = \"owed\"
amount = 2.00
currency = \"USD\"
as_of = \"2026-07-21\"
";
        let outcome = parse(toml).expect("the file itself is well-formed TOML");
        assert_eq!(
            outcome
                .rows
                .iter()
                .map(|r| r.id.as_str())
                .collect::<Vec<_>>(),
            vec!["openai-api"],
            "only the non-numeric-amount row drops"
        );
        assert_eq!(outcome.errors.len(), 1);
        assert_eq!(outcome.errors[0].id.as_deref(), Some("anthropic-api"));
        assert!(
            outcome.errors[0].reason.contains("amount"),
            "{:?}",
            outcome.errors[0]
        );
    }

    #[test]
    fn unparsable_as_of_degrades_only_that_row() {
        let toml = "\
[[api_manual]]
id = \"anthropic-api\"
figure = \"balance\"
amount = 1.00
currency = \"USD\"
as_of = \"not-a-date\"

[[api_manual]]
id = \"openai-api\"
figure = \"owed\"
amount = 2.00
currency = \"USD\"
as_of = \"2026-07-21\"
";
        let outcome = parse(toml).expect("the file itself is well-formed TOML");
        assert_eq!(
            outcome
                .rows
                .iter()
                .map(|r| r.id.as_str())
                .collect::<Vec<_>>(),
            vec!["openai-api"],
            "only the bad-as_of row drops — never a silent fresh"
        );
        assert_eq!(outcome.errors.len(), 1);
        assert_eq!(outcome.errors[0].id.as_deref(), Some("anthropic-api"));
        assert!(
            outcome.errors[0].reason.contains("as_of"),
            "{:?}",
            outcome.errors[0]
        );
    }

    #[test]
    fn missing_required_fields_each_degrade_their_row() {
        let toml = "\
[[api_manual]]
figure = \"balance\"
amount = 1.00
currency = \"USD\"
as_of = \"2026-07-20\"

[[api_manual]]
id = \"no-figure\"
amount = 1.00
currency = \"USD\"
as_of = \"2026-07-20\"

[[api_manual]]
id = \"no-amount\"
figure = \"balance\"
currency = \"USD\"
as_of = \"2026-07-20\"

[[api_manual]]
id = \"no-currency\"
figure = \"balance\"
amount = 1.00
as_of = \"2026-07-20\"
";
        let outcome = parse(toml).expect("well-formed TOML");
        assert!(
            outcome.rows.is_empty(),
            "every row is missing a required field"
        );
        assert_eq!(outcome.errors.len(), 4, "errors: {:?}", outcome.errors);
        // The missing-id row carries no id; the rest carry theirs.
        assert_eq!(
            outcome.errors[0].id, None,
            "the missing-id row has no readable id"
        );
        assert!(outcome
            .errors
            .iter()
            .any(|e| e.id.as_deref() == Some("no-figure")));
        assert!(outcome
            .errors
            .iter()
            .any(|e| e.id.as_deref() == Some("no-amount")));
        assert!(outcome
            .errors
            .iter()
            .any(|e| e.id.as_deref() == Some("no-currency")));
    }

    #[test]
    fn negative_amount_is_valid() {
        let toml = "\
[[api_manual]]
id = \"anthropic-api\"
figure = \"owed\"
amount = -5.00
currency = \"USD\"
as_of = \"2026-07-20\"
";
        let outcome = parse(toml).expect("well-formed TOML");
        assert_eq!(outcome.rows.len(), 1);
        assert_eq!(
            outcome.rows[0].amount_minor, -500,
            "a negative dollar amount is a valid negative cents value"
        );
    }

    #[test]
    fn api_manual_row_has_exactly_six_fields() {
        // Structural guarantee (spec 024 §C/§F): no email/account-name field exists — an
        // exhaustive destructure means a future edit adding one must touch this test. The made-up
        // id/note carry no PII.
        let row = ApiManualRow {
            id: "anthropic-api".to_string(),
            figure: ApiFigureKind::Balance,
            amount_minor: 4012,
            currency: "USD".to_string(),
            as_of: "2026-07-20".to_string(),
            notes: None,
        };
        let ApiManualRow {
            id,
            figure,
            amount_minor,
            currency,
            as_of,
            notes,
        } = row;
        let _ = (id, figure, amount_minor, currency, as_of, notes);
    }

    // ── path resolution (×3 precedence cases) ─────────────────────────────────────────────────

    #[test]
    fn env_override_beats_settings_path() {
        assert_eq!(
            resolve_path(
                Some("/env/api_manual.toml"),
                Some(Path::new("/settings/api_manual.toml"))
            ),
            Some(PathBuf::from("/env/api_manual.toml"))
        );
    }

    #[test]
    fn settings_path_used_when_env_absent_or_empty() {
        assert_eq!(
            resolve_path(None, Some(Path::new("/settings/api_manual.toml"))),
            Some(PathBuf::from("/settings/api_manual.toml"))
        );
        assert_eq!(
            resolve_path(Some(""), Some(Path::new("/settings/api_manual.toml"))),
            Some(PathBuf::from("/settings/api_manual.toml")),
            "an empty env override must be ignored"
        );
    }

    #[test]
    fn both_unset_resolves_to_off() {
        assert_eq!(resolve_path(None, None), None);
        assert_eq!(
            resolve_path(Some(""), Some(Path::new(""))),
            None,
            "empty env AND empty settings path ⇒ off"
        );
    }

    // ── the stateful loader: Missing / Stale / Fresh, keep-last-good, no-op ────────────────────

    /// An in-memory `ApiManualSource` for tests: each poll delivers the next queued entry. `None`
    /// means the file couldn't be read this poll (drives `Missing`). ZERO filesystem.
    struct TestSource(std::collections::VecDeque<Option<String>>);

    impl ApiManualSource for TestSource {
        fn read(&mut self) -> Option<String> {
            self.0.pop_front().flatten()
        }
    }

    #[test]
    fn fresh_loader_never_polled_stays_off() {
        let manual = ApiManual::new();
        assert_eq!(manual.provenance(), ApiManualProvenance::Off);
        assert!(manual.rows().is_empty());
    }

    #[test]
    fn configured_but_missing_file_is_missing_not_stale() {
        let mut manual = ApiManual::new();
        let mut source = TestSource(vec![None].into());
        manual.poll(&mut source);
        assert_eq!(manual.provenance(), ApiManualProvenance::Missing);
        assert!(manual.rows().is_empty());
    }

    #[test]
    fn first_load_of_unparseable_content_is_stale_with_zero_rows() {
        let mut manual = ApiManual::new();
        let mut source = TestSource(vec![Some("not [[[ valid toml".to_string())].into());
        manual.poll(&mut source);
        assert_eq!(
            manual.provenance(),
            ApiManualProvenance::Stale,
            "first load, nothing parseable yet ⇒ Stale"
        );
        assert!(manual.rows().is_empty());
        assert!(
            manual.stale_reason().is_some(),
            "a wholly unparseable file must be diagnosable, not just 'stale'"
        );
    }

    #[test]
    fn good_load_then_broken_reparse_keeps_last_good_rows_and_marks_stale() {
        let mut manual = ApiManual::new();
        let mut source = TestSource(vec![Some(GOOD.to_string())].into());
        manual.poll(&mut source);
        assert_eq!(manual.provenance(), ApiManualProvenance::Fresh);
        assert_eq!(manual.rows().len(), 3);

        // A mid-edit breaks the file (the broken fixture). The previous good rows must survive.
        let mut source = TestSource(vec![Some(BROKEN.to_string())].into());
        manual.poll(&mut source);
        assert_eq!(manual.provenance(), ApiManualProvenance::Stale);
        assert_eq!(
            manual.rows().len(),
            3,
            "the previous good rows must survive a failed re-parse"
        );
        assert!(
            manual.stale_reason().is_some(),
            "the whole-file parse error must be retained for tok doctor"
        );
    }

    #[test]
    fn unchanged_content_is_a_noop_poll() {
        // Content-hash no-op: polling the same bytes twice does not re-parse (provenance stays
        // Fresh, no spurious state churn).
        let mut manual = ApiManual::new();
        let bytes = GOOD.to_string();
        let mut source = TestSource(vec![Some(bytes.clone()), Some(bytes)].into());
        manual.poll(&mut source);
        assert_eq!(manual.provenance(), ApiManualProvenance::Fresh);
        let rows_after_first = manual.rows().len();
        manual.poll(&mut source);
        assert_eq!(
            manual.provenance(),
            ApiManualProvenance::Fresh,
            "unchanged content must not change provenance"
        );
        assert_eq!(manual.rows().len(), rows_after_first);
    }

    #[test]
    fn hot_reload_edit_then_garbage_then_restore() {
        let mut manual = ApiManual::new();
        let v1 = "\
[[api_manual]]
id = \"anthropic-api\"
figure = \"balance\"
amount = 1.00
currency = \"USD\"
as_of = \"2026-07-20\"
";
        let v2 = "not valid toml [[";
        let v3 = "\
[[api_manual]]
id = \"anthropic-api\"
figure = \"balance\"
amount = 1.00
currency = \"USD\"
as_of = \"2026-07-20\"

[[api_manual]]
id = \"openai-api\"
figure = \"owed\"
amount = 2.00
currency = \"USD\"
as_of = \"2026-07-21\"
";
        let mut source = TestSource(
            vec![
                Some(v1.to_string()),
                Some(v2.to_string()),
                Some(v3.to_string()),
            ]
            .into(),
        );

        manual.poll(&mut source);
        assert_eq!(manual.provenance(), ApiManualProvenance::Fresh);
        assert_eq!(manual.rows().len(), 1);

        manual.poll(&mut source);
        assert_eq!(manual.provenance(), ApiManualProvenance::Stale);
        assert_eq!(manual.rows().len(), 1, "garbage keeps the last-good rows");
        assert!(manual.stale_reason().is_some());

        manual.poll(&mut source);
        assert_eq!(
            manual.provenance(),
            ApiManualProvenance::Fresh,
            "restoring valid content clears Stale"
        );
        assert_eq!(
            manual.rows().len(),
            2,
            "the restored content's new row lands"
        );
        assert!(
            manual.stale_reason().is_none(),
            "a recovered read must clear the stale reason"
        );
    }

    // ── the 35-day staleness boundary ──────────────────────────────────────────────────────────

    #[test]
    fn row_is_stale_boundary_35_exactly_fresh_36_stale() {
        // today = 2026-03-01. 2026-01-25 is exactly 35 days earlier (fresh); 2026-01-24 is 36
        // (stale). Day 36 is the first stale day per the pinned decision.
        let today = jiff::civil::date(2026, 3, 1);
        assert!(
            !row_is_stale("2026-01-25", today),
            "as_of + 35 == today must be fresh"
        );
        assert!(
            row_is_stale("2026-01-24", today),
            "as_of + 35 < today (day 36) must be stale"
        );
        // A future as_of is not stale.
        assert!(!row_is_stale("2026-12-31", today));
    }

    #[test]
    fn row_is_stale_unparsable_is_defensively_not_stale() {
        // row_is_stale is only ever called with a parse-validated as_of; an unparseable string
        // (which parse() would have degraded) returns false here rather than panicking.
        let today = jiff::civil::date(2026, 3, 1);
        assert!(!row_is_stale("not-a-date", today));
    }

    // ── helper: exact-match join on id (mirrors ledger::find) ──────────────────────────────────

    fn find<'a>(rows: &'a [ApiManualRow], id: &str) -> Option<&'a ApiManualRow> {
        rows.iter().find(|r| r.id == id)
    }
}
