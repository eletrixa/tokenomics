//! The subscription ledger: a third, read-through-only data plane (billing/lifecycle dates), never
//! conflated with the local ccusage plane or the opt-in overlay plane.
//!
//! Project: Tokenomics — monitor LLM subscription accounts (usage, limits, time-left) in a TUI
//! Module:  src/ledger.rs
//! Deps:    jiff (civil::Date), toml + serde (parsing, both already deps)
//! Tested:  inline `#[cfg(test)]` below — parse() row/file degradation (spec 017 §A acceptance 1),
//!          path resolution (§B acceptance 2), hot reload via the injectable `LedgerSource` seam
//!          (§B acceptance 6), exact-match join (§C acceptance 3); `verified` field parsing +
//!          `verified_current` math (spec 018 §A/§B acceptance 1/2)
//!
//! Key responsibilities:
//! - `Subscription`: one ledger row — `id`, `status` (two-variant), five optional local dates
//!   (including `verified`, spec 018). The ledger's `account` field (a raw email) is never given a
//!   place to land: it simply isn't a field on this struct, so it cannot be deserialized, logged, or
//!   rendered.
//! - `parse`: pure `&str` → `ParseOutcome`. Per-row degradation (one malformed row drops that row
//!   only, reported via `ParseOutcome::errors`); `Err` only for a WHOLLY unparseable file.
//! - `Ledger`: the stateful, keep-last-good loader (mirrors `collector::ConfigSource`'s discipline)
//!   behind the injectable `LedgerSource` trait, so hot reload never touches the filesystem in tests.
//! - `resolve_path`: `TOKENOMICS_LEDGER` env > `[settings] ledger_path` > off (§B). Pure.
//! - `find`: exact-string-match join on `Account.id` ↔ `Subscription.id` (§C). No fuzzy matching.
//! - `verified_current`: pure `Subscription` + `today` → is `verified` still current-period-current
//!   (spec 018 §B); shared by the TUI's pill math and `tok doctor`'s per-row annotation.
//!
//! Design constraints:
//! - Read-through only: never persisted to SQLite, never fetched over the network. TUI-only — the
//!   collector never reads the ledger (CLAUDE.md's two-plane rule, extended to a third plane here).
//! - No panic/unwrap/expect on any ledger content — a mid-edit (or hand-authored, malformed) ledger
//!   must never crash or blank the dashboard, only degrade to `Stale`/`Missing` + a doctor-visible
//!   reason.
//! - `Account.active` (spec 014) and ledger `status` are independent bits; nothing here reconciles
//!   them (see `tui::model::build_sub_view`, which renders each account's clause without touching
//!   `Account.active`).

use std::path::PathBuf;

use jiff::civil::Date;
use serde::Deserialize;

/// Env var overriding the ledger path (empty ⇒ ignored) — mirrors `paths::CONFIG_ENV`.
pub const LEDGER_ENV: &str = "TOKENOMICS_LEDGER";

/// A subscription's billing status, per the ledger's `status` field. Exactly two valid values
/// (`"active"` / `"cancelled"`) — anything else (including the common typo `"canceled"`) is a
/// malformed row, never coerced (spec 017 §A).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubStatus {
    /// The current billing period is live.
    Active,
    /// The subscription was cancelled (`cancelled_on` / `paid_through` may or may not be present).
    Cancelled,
}

impl SubStatus {
    /// Parse the ledger's `status` string. `None` for anything but the two exact spellings —
    /// including `"canceled"` (single-L), which must degrade the row, never silently coerce.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "active" => Some(Self::Active),
            "cancelled" => Some(Self::Cancelled),
            _ => None,
        }
    }
}

/// One clean ledger row. Dates are `None` when the ledger omits them — never guessed. There is
/// deliberately no `account` field: the ledger's raw-email field never gets a place to land in this
/// crate (spec 017 §A) — only `id`, `status`, and dates cross the boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Subscription {
    /// Stable join key, matched exactly against `Account.id` (spec 017 §C).
    pub id: String,
    /// `active` or `cancelled`.
    pub status: SubStatus,
    /// Current-period start (bumped on each renewal). Rendered verbatim, never rolled.
    pub purchased: Option<Date>,
    /// Current-period end.
    pub renews: Option<Date>,
    /// When a cancellation was recorded.
    pub cancelled_on: Option<Date>,
    /// Access lapses after this date — only meaningful when `status` is `Cancelled`.
    pub paid_through: Option<Date>,
    /// The date an agent last confirmed this row against the provider's billing web UI
    /// (Tier-0 read-only run). `None` = a human-typed row, never an error (spec 018 §A).
    pub verified: Option<Date>,
}

/// One row that failed to parse, kept ONLY for `tok doctor` — never for rendering (spec 017 §A/§E).
/// `id` is best-effort: present when the row at least had a readable `id`, so a bad `status`/date on
/// an otherwise-identifiable row is still diagnosable by which account it belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowError {
    /// The row's `id`, when present even though some other field was malformed.
    pub id: Option<String>,
    /// A human-readable reason (e.g. `"unknown status 'canceled'"`, `"missing id"`).
    pub reason: String,
}

/// The result of one `parse()` call: rows that parsed clean, plus rows that didn't (with reason).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParseOutcome {
    /// Every row that parsed cleanly.
    pub rows: Vec<Subscription>,
    /// Every row that degraded (dropped), with its reason — surfaced by `tok doctor`, never rendered.
    pub errors: Vec<RowError>,
}

/// Where the ledger plane's current read came from — carried once per read, not per row (spec 017
/// §A). `Off` renders nothing anywhere in the TUI; the other three degrade visibly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LedgerProvenance {
    /// Not configured (`TOKENOMICS_LEDGER` and `[settings] ledger_path` both unset). The default —
    /// a `Ledger` that is never `poll`ed stays here forever.
    #[default]
    Off,
    /// The last read parsed (the file may still hold rows that failed per-row — see `errors`).
    Fresh,
    /// A configured path's last successful parse is being retained after a failed re-parse (a
    /// mid-edit file), or nothing has ever parsed yet.
    Stale,
    /// A configured path could not be read at all (absent, permissions, …).
    Missing,
}

/// Raw `[[subscription]]` shape for structural (whole-document) deserialization only — every field
/// is loose/optional so a single bad row never fails the whole document; per-row semantic validation
/// (id present, status one of the two exact spellings, dates convertible) happens in `parse` after
/// this succeeds. No `deny_unknown_fields`: unrecognized keys (`account`, `plan`, `price`, `notes`,
/// `first_purchased`, …) are silently ignored by serde's default struct behaviour — never an error.
#[derive(Debug, Deserialize, Default)]
struct RawLedger {
    #[serde(default, rename = "subscription")]
    subscription: Vec<RawSub>,
}

#[derive(Debug, Deserialize, Default)]
struct RawSub {
    id: Option<String>,
    status: Option<String>,
    purchased: Option<toml::value::Datetime>,
    renews: Option<toml::value::Datetime>,
    cancelled_on: Option<toml::value::Datetime>,
    paid_through: Option<toml::value::Datetime>,
    verified: Option<toml::value::Datetime>,
}

/// Convert one optional TOML datetime field to `jiff::civil::Date`. `Ok(None)` when the field was
/// absent; `Err` (naming `field`) when present but not a valid calendar date — a row-degrading
/// condition per spec 017 §A ("bad date" is named alongside "bad status"/"missing id"), never a
/// silent `None`.
fn to_date(field: &str, dt: Option<toml::value::Datetime>) -> Result<Option<Date>, String> {
    let Some(dt) = dt else {
        return Ok(None);
    };
    let Some(d) = dt.date else {
        return Err(format!("{field} has no date component"));
    };
    let year = i16::try_from(d.year).map_err(|_| format!("{field} year out of range"))?;
    let month = i8::try_from(d.month).map_err(|_| format!("{field} month out of range"))?;
    let day = i8::try_from(d.day).map_err(|_| format!("{field} day out of range"))?;
    Date::new(year, month, day)
        .map(Some)
        .map_err(|_| format!("{field} is not a valid calendar date"))
}

/// Pure parse of ledger TOML text. `Ok` even when individual rows are malformed — those degrade into
/// `ParseOutcome::errors` and are dropped from `rows`, never rendered, never causing the whole read
/// to fail. `Err` is reserved for a WHOLLY unparseable file (bad TOML syntax, or a `[[subscription]]`
/// shape that doesn't even deserialize) — the caller (`Ledger::poll`) keeps its last-good rows and
/// marks the plane `Stale` rather than blanking the dashboard.
pub fn parse(text: &str) -> Result<ParseOutcome, String> {
    let raw: RawLedger = toml::from_str(text).map_err(|e| e.to_string())?;
    let mut rows = Vec::new();
    let mut errors = Vec::new();
    for row in raw.subscription {
        let Some(id) = row.id else {
            errors.push(RowError {
                id: None,
                reason: "missing id".to_string(),
            });
            continue;
        };
        let Some(status_str) = row.status else {
            errors.push(RowError {
                id: Some(id),
                reason: "missing status".to_string(),
            });
            continue;
        };
        let Some(status) = SubStatus::parse(&status_str) else {
            errors.push(RowError {
                id: Some(id),
                reason: format!("unknown status '{status_str}'"),
            });
            continue;
        };
        let degrade =
            |field: &str, dt: Option<toml::value::Datetime>| -> Result<Option<Date>, RowError> {
                to_date(field, dt).map_err(|reason| RowError {
                    id: Some(id.clone()),
                    reason,
                })
            };
        let purchased = match degrade("purchased", row.purchased) {
            Ok(d) => d,
            Err(e) => {
                errors.push(e);
                continue;
            }
        };
        let renews = match degrade("renews", row.renews) {
            Ok(d) => d,
            Err(e) => {
                errors.push(e);
                continue;
            }
        };
        let cancelled_on = match degrade("cancelled_on", row.cancelled_on) {
            Ok(d) => d,
            Err(e) => {
                errors.push(e);
                continue;
            }
        };
        let paid_through = match degrade("paid_through", row.paid_through) {
            Ok(d) => d,
            Err(e) => {
                errors.push(e);
                continue;
            }
        };
        let verified = match degrade("verified", row.verified) {
            Ok(d) => d,
            Err(e) => {
                errors.push(e);
                continue;
            }
        };
        rows.push(Subscription {
            id,
            status,
            purchased,
            renews,
            cancelled_on,
            paid_through,
            verified,
        });
    }
    Ok(ParseOutcome { rows, errors })
}

/// Resolve the ledger path per spec 017 §B: `TOKENOMICS_LEDGER` (non-empty) beats a non-empty
/// `[settings] ledger_path` beats off. Pure (values passed in, mirrors `paths::pick`) so it's
/// table-testable without touching the environment; no hardcoded default path exists anywhere.
pub fn resolve_path(env_override: Option<&str>, settings_path: Option<&str>) -> Option<PathBuf> {
    if let Some(env) = env_override {
        if !env.is_empty() {
            return Some(PathBuf::from(env));
        }
    }
    if let Some(settings) = settings_path {
        if !settings.is_empty() {
            return Some(PathBuf::from(settings));
        }
    }
    None
}

/// Best-effort recency window (spec 018 §B) for a `verified` date when the row carries no
/// `purchased` to compare against: `today − verified <= 31` days.
const VERIFIED_RECENCY_DAYS: i32 = 31;

/// Whether `sub.verified` still says something about the CURRENT billing period (spec 018 §B): with
/// `purchased` present, verified-current ⇔ `verified >= purchased` (older proves nothing about this
/// period's renewal); without it, a 31-day best-effort recency window against `today`. `false` when
/// `verified` is absent (a human-entered row), or when `verified` is in the future (a typo'd ledger
/// date proves nothing — never render a confident pill from data that can't yet be true). Pure,
/// `today` injected — shared by the pill math in `tui::model::SubView` and `tok doctor`'s per-row
/// annotation, so both stay in lockstep.
pub fn verified_current(sub: &Subscription, today: Date) -> bool {
    let Some(verified) = sub.verified else {
        return false;
    };
    if verified > today {
        return false;
    }
    match sub.purchased {
        Some(purchased) => verified >= purchased,
        None => today
            .since(verified)
            .is_ok_and(|span| span.get_days() <= VERIFIED_RECENCY_DAYS),
    }
}

/// Join `account_id` against the ledger rows by EXACT string match only — no fuzzy, prefix, or
/// case-insensitive matching (spec 017 §C: a near-miss id like `claude-rob7` vs `claude-rob-7` must
/// produce no clause, not a guess).
pub fn find<'a>(rows: &'a [Subscription], account_id: &str) -> Option<&'a Subscription> {
    rows.iter().find(|r| r.id == account_id)
}

/// A source of ledger text, injectable so tests never touch the filesystem — mirrors
/// `collector::ConfigSource`'s seam. `None` means unreadable/absent (drives `LedgerProvenance::Missing`).
pub trait LedgerSource {
    /// Read the current ledger content, or `None` when the path can't be read right now.
    fn read(&mut self) -> Option<String>;
}

/// Production `LedgerSource`: a fresh `read_to_string` of `path` each poll. The content-hash /
/// keep-last-good discipline lives in `Ledger::poll`, not here — this stays a thin file read (mirrors
/// how `FileConfigSource` splits "read the bytes" from "decide whether to act on them").
#[derive(Debug)]
pub struct FileLedgerSource {
    path: PathBuf,
}

impl FileLedgerSource {
    /// Watch `path` for ledger content.
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

impl LedgerSource for FileLedgerSource {
    fn read(&mut self) -> Option<String> {
        std::fs::read_to_string(&self.path).ok()
    }
}

/// The stateful, keep-last-good ledger reader the TUI polls once per tick (spec 017 §B). Starts
/// `Off` (the default, sane before any path is resolved) and only ever changes state via `poll`.
#[derive(Debug, Default)]
pub struct Ledger {
    rows: Vec<Subscription>,
    errors: Vec<RowError>,
    provenance: LedgerProvenance,
    /// Content-hash of the last `read()` this `Ledger` acted on, so an unchanged read (including a
    /// persistently-missing file) is a cheap no-op rather than a re-parse (mirrors
    /// `collector::FileConfigSource`'s content-hash discipline, spec 015 §A).
    last_seen: LastSeen,
    /// The whole-file parse error from the last failed re-parse (the `Stale`-causing `Err(reason)`
    /// from `parse`), so a wholly unparseable ledger is still diagnosable by `tok doctor` — not just
    /// "stale" with no reason. `None` once a read succeeds again.
    stale_reason: Option<String>,
}

/// `Ledger::last_seen`'s three states — never polled, last content unreadable/absent, or last
/// content's hash — kept as a named enum rather than `Option<Option<u64>>` (clippy pedantic).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum LastSeen {
    #[default]
    Never,
    Missing,
    Hash(u64),
}

impl Ledger {
    /// A fresh, unpolled ledger (`Off`, zero rows) — the caller only calls `poll` once a path has
    /// been resolved via `resolve_path`; an unconfigured plane is never polled at all.
    pub fn new() -> Self {
        Self::default()
    }

    /// The last-good parsed rows (kept across a failed re-parse — see `LedgerProvenance::Stale`).
    pub fn rows(&self) -> &[Subscription] {
        &self.rows
    }

    /// Rows that failed to parse on the last successful read (for `tok doctor`; never rendered).
    pub fn errors(&self) -> &[RowError] {
        &self.errors
    }

    /// The plane's current provenance.
    pub fn provenance(&self) -> LedgerProvenance {
        self.provenance
    }

    /// The whole-file parse error behind the current `Stale` provenance, when there is one — so a
    /// wholly unparseable ledger (bad TOML syntax) is diagnosable by `tok doctor`, not just "stale"
    /// with no reason. `None` when `provenance()` isn't `Stale` for a parse-error reason (e.g. a
    /// first-ever poll with nothing readable yet, or `Fresh`/`Missing`/`Off`).
    pub fn stale_reason(&self) -> Option<&str> {
        self.stale_reason.as_deref()
    }

    /// Poll `source` for the current content and fold the result in: a readable + parseable file
    /// becomes `Fresh` (rows replaced); a readable-but-unparseable file keeps the last-good rows and
    /// becomes `Stale`; an unreadable/absent file becomes `Missing` (last-good rows also kept — a
    /// transient read failure must never blank the dashboard). Mirrors `ConfigSource::poll`'s
    /// keep-last-good discipline, but for display data only — this never changes which accounts are
    /// monitored (spec 017 §B). A no-op when the content hash is unchanged since the last poll.
    pub fn poll<S: LedgerSource>(&mut self, source: &mut S) {
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
            self.provenance = LedgerProvenance::Missing;
            return;
        };
        match parse(&text) {
            Ok(outcome) => {
                self.rows = outcome.rows;
                self.errors = outcome.errors;
                self.provenance = LedgerProvenance::Fresh;
                self.stale_reason = None;
            }
            Err(reason) => {
                self.provenance = LedgerProvenance::Stale;
                self.stale_reason = Some(reason);
            }
        }
    }
}

/// Hash ledger text with `std`'s `DefaultHasher` — the content-change trigger for `Ledger::poll`,
/// mirroring `collector::FileConfigSource`'s `hash_bytes`. In-process only, never persisted.
fn hash_bytes(text: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── AC1: parse() — field combinations, row degradation, whole-file failure ─────────────────

    /// A synthetic ledger covering every field combination the schema allows: an active row with
    /// both dates, a cancelled row with both its dates, an active row with no dates at all, and a
    /// row carrying every unknown/ignored field the external schema may add (`account`, `plan`,
    /// `price`, `notes`, `first_purchased`). All ids/dates are made up (never real ledger content).
    const ALL_COMBOS: &str = "\
[[subscription]]
id = \"claude-alpha\"
status = \"active\"
purchased = 2026-07-14
renews = 2026-08-14

[[subscription]]
id = \"claude-bravo\"
status = \"cancelled\"
cancelled_on = 2026-07-10
paid_through = 2026-07-22

[[subscription]]
id = \"claude-charlie\"
status = \"active\"

[[subscription]]
id = \"claude-delta\"
status = \"active\"
account = \"someone@example.com\"
plan = \"max\"
price = 200
notes = \"unknown fields must be ignored, not rejected\"
first_purchased = 2025-01-01
";

    #[test]
    fn parse_yields_expected_rows_for_all_field_combinations() {
        let outcome = parse(ALL_COMBOS).expect("a well-formed ledger parses");
        assert!(
            outcome.errors.is_empty(),
            "no malformed rows in this fixture: {:?}",
            outcome.errors
        );
        assert_eq!(outcome.rows.len(), 4, "rows: {:?}", outcome.rows);

        let alpha = outcome
            .rows
            .iter()
            .find(|r| r.id == "claude-alpha")
            .expect("claude-alpha row");
        assert_eq!(alpha.status, SubStatus::Active);
        assert_eq!(alpha.purchased, Some(jiff::civil::date(2026, 7, 14)));
        assert_eq!(alpha.renews, Some(jiff::civil::date(2026, 8, 14)));
        assert_eq!(alpha.cancelled_on, None);
        assert_eq!(alpha.paid_through, None);

        let bravo = outcome
            .rows
            .iter()
            .find(|r| r.id == "claude-bravo")
            .expect("claude-bravo row");
        assert_eq!(bravo.status, SubStatus::Cancelled);
        assert_eq!(bravo.cancelled_on, Some(jiff::civil::date(2026, 7, 10)));
        assert_eq!(bravo.paid_through, Some(jiff::civil::date(2026, 7, 22)));

        let charlie = outcome
            .rows
            .iter()
            .find(|r| r.id == "claude-charlie")
            .expect("claude-charlie row (no dates at all)");
        assert_eq!(charlie.purchased, None);
        assert_eq!(charlie.renews, None);

        // The row with every unknown/ignored field present still parses — and the `account` value
        // never lands anywhere reachable (there is no field to hold it; see the structural test
        // below).
        assert!(
            outcome.rows.iter().any(|r| r.id == "claude-delta"),
            "row with unknown fields must still parse: {:?}",
            outcome.rows
        );
    }

    #[test]
    fn subscription_has_no_account_field() {
        // Structural guarantee (spec 017 §A): the ledger's raw-email `account` field is never given
        // a place to land — `Subscription` simply has no such field, so it cannot be deserialized,
        // logged, or rendered. An exhaustive destructure here means a future edit that adds an
        // `account` field must touch this test.
        let sub = Subscription {
            id: "claude-alpha".to_string(),
            status: SubStatus::Active,
            purchased: None,
            renews: None,
            cancelled_on: None,
            paid_through: None,
            verified: None,
        };
        let Subscription {
            id,
            status,
            purchased,
            renews,
            cancelled_on,
            paid_through,
            verified,
        } = sub;
        let _ = (
            id,
            status,
            purchased,
            renews,
            cancelled_on,
            paid_through,
            verified,
        );
    }

    #[test]
    fn malformed_status_degrades_only_that_row() {
        // "canceled" (single L) is the classic typo — spec 017 §A is explicit it must NOT be
        // coerced to `Cancelled`; the row degrades, the other two rows survive.
        let toml = "\
[[subscription]]
id = \"claude-alpha\"
status = \"active\"

[[subscription]]
id = \"claude-bravo\"
status = \"canceled\"

[[subscription]]
id = \"claude-charlie\"
status = \"active\"
";
        let outcome = parse(toml).expect("the file itself is well-formed TOML");
        assert_eq!(
            outcome
                .rows
                .iter()
                .map(|r| r.id.as_str())
                .collect::<Vec<_>>(),
            vec!["claude-alpha", "claude-charlie"],
            "only the malformed-status row drops"
        );
        assert_eq!(outcome.errors.len(), 1, "errors: {:?}", outcome.errors);
        assert_eq!(outcome.errors[0].id.as_deref(), Some("claude-bravo"));
        assert!(
            outcome.errors[0].reason.contains("canceled")
                || outcome.errors[0].reason.contains("status"),
            "reason should name the bad status: {:?}",
            outcome.errors[0]
        );
    }

    #[test]
    fn missing_id_degrades_only_that_row() {
        let toml = "\
[[subscription]]
status = \"active\"

[[subscription]]
id = \"claude-alpha\"
status = \"active\"
";
        let outcome = parse(toml).expect("the file itself is well-formed TOML");
        assert_eq!(outcome.rows.len(), 1, "rows: {:?}", outcome.rows);
        assert_eq!(outcome.rows[0].id, "claude-alpha");
        assert_eq!(outcome.errors.len(), 1, "errors: {:?}", outcome.errors);
        assert_eq!(
            outcome.errors[0].id, None,
            "no id was readable on the bad row"
        );
    }

    #[test]
    fn missing_status_degrades_only_that_row() {
        let toml = "\
[[subscription]]
id = \"claude-alpha\"

[[subscription]]
id = \"claude-bravo\"
status = \"active\"
";
        let outcome = parse(toml).expect("the file itself is well-formed TOML");
        assert_eq!(
            outcome
                .rows
                .iter()
                .map(|r| r.id.as_str())
                .collect::<Vec<_>>(),
            vec!["claude-bravo"]
        );
        assert_eq!(outcome.errors.len(), 1);
        assert_eq!(outcome.errors[0].id.as_deref(), Some("claude-alpha"));
    }

    #[test]
    fn wholly_unparseable_text_is_a_parse_error() {
        // Not valid TOML at all — the whole-file failure path (`Ledger::poll` turns this into
        // keep-last-good + `Stale`, tested below via the stateful loader).
        assert!(parse("this is [[[ not toml at all").is_err());
    }

    #[test]
    fn empty_ledger_parses_to_zero_rows_no_errors() {
        let outcome = parse("").expect("an empty file is valid (empty) TOML");
        assert!(outcome.rows.is_empty());
        assert!(outcome.errors.is_empty());
    }

    // ── AC2: path resolution ────────────────────────────────────────────────────────────────────

    #[test]
    fn env_override_beats_settings_path() {
        assert_eq!(
            resolve_path(
                Some("/env/subscriptions.toml"),
                Some("/settings/subscriptions.toml")
            ),
            Some(PathBuf::from("/env/subscriptions.toml"))
        );
    }

    #[test]
    fn settings_path_used_when_env_absent_or_empty() {
        assert_eq!(
            resolve_path(None, Some("/settings/subscriptions.toml")),
            Some(PathBuf::from("/settings/subscriptions.toml"))
        );
        assert_eq!(
            resolve_path(Some(""), Some("/settings/subscriptions.toml")),
            Some(PathBuf::from("/settings/subscriptions.toml")),
            "an empty env override must be ignored, like TOKENOMICS_CONFIG"
        );
    }

    #[test]
    fn both_unset_resolves_to_off() {
        assert_eq!(resolve_path(None, None), None);
        assert_eq!(resolve_path(Some(""), Some("")), None);
    }

    // ── AC2/AC6: the stateful loader — Missing / Stale / Fresh, and hot reload ─────────────────

    /// An in-memory `LedgerSource` for tests: each poll delivers the next queued entry. `None` means
    /// the file couldn't be read this poll (drives `Missing`).
    struct TestSource(std::collections::VecDeque<Option<String>>);

    impl LedgerSource for TestSource {
        fn read(&mut self) -> Option<String> {
            self.0.pop_front().flatten()
        }
    }

    #[test]
    fn fresh_ledger_never_polled_stays_off() {
        let ledger = Ledger::new();
        assert_eq!(ledger.provenance(), LedgerProvenance::Off);
        assert!(ledger.rows().is_empty());
    }

    #[test]
    fn configured_but_missing_file_is_missing_not_stale() {
        let mut ledger = Ledger::new();
        let mut source = TestSource(vec![None].into());
        ledger.poll(&mut source);
        assert_eq!(ledger.provenance(), LedgerProvenance::Missing);
        assert!(ledger.rows().is_empty());
    }

    #[test]
    fn first_load_of_unparseable_content_is_stale_with_zero_rows() {
        let mut ledger = Ledger::new();
        let mut source = TestSource(vec![Some("not [[[ valid toml".to_string())].into());
        ledger.poll(&mut source);
        assert_eq!(
            ledger.provenance(),
            LedgerProvenance::Stale,
            "first load, nothing parseable yet ⇒ Stale, not Missing/Off"
        );
        assert!(ledger.rows().is_empty());
        assert!(
            ledger.stale_reason().is_some(),
            "a wholly unparseable file must be diagnosable, not just 'stale' with no reason"
        );
    }

    #[test]
    fn good_load_then_bad_reparse_keeps_last_good_rows_and_marks_stale() {
        let mut ledger = Ledger::new();
        let good = "[[subscription]]\nid = \"claude-alpha\"\nstatus = \"active\"\n";
        let mut source = TestSource(vec![Some(good.to_string())].into());
        ledger.poll(&mut source);
        assert_eq!(ledger.provenance(), LedgerProvenance::Fresh);
        assert_eq!(ledger.rows().len(), 1);

        let mut source = TestSource(vec![Some("garbage [[[ mid-edit".to_string())].into());
        ledger.poll(&mut source);
        assert_eq!(ledger.provenance(), LedgerProvenance::Stale);
        assert_eq!(
            ledger.rows().len(),
            1,
            "the previous good row must survive a failed re-parse"
        );
        assert_eq!(ledger.rows()[0].id, "claude-alpha");
        assert!(
            ledger.stale_reason().is_some(),
            "the whole-file parse error must be retained for `tok doctor`, not swallowed"
        );
    }

    #[test]
    fn hot_reload_edit_then_garbage_then_restore() {
        // AC6: an edit reaches the next poll; garbage keeps the clause data and flags Stale;
        // restoring valid content clears Stale back to Fresh with the new content.
        let mut ledger = Ledger::new();
        let v1 = "[[subscription]]\nid = \"claude-alpha\"\nstatus = \"active\"\n";
        let v2 = "not valid toml [[[";
        let v3 = "\
[[subscription]]
id = \"claude-alpha\"
status = \"active\"

[[subscription]]
id = \"claude-bravo\"
status = \"cancelled\"
";
        let mut source = TestSource(
            vec![
                Some(v1.to_string()),
                Some(v2.to_string()),
                Some(v3.to_string()),
            ]
            .into(),
        );

        ledger.poll(&mut source);
        assert_eq!(ledger.provenance(), LedgerProvenance::Fresh);
        assert_eq!(ledger.rows().len(), 1);

        ledger.poll(&mut source);
        assert_eq!(ledger.provenance(), LedgerProvenance::Stale);
        assert_eq!(ledger.rows().len(), 1, "garbage keeps the last-good clause");
        assert!(ledger.stale_reason().is_some(), "garbage must set a reason");

        ledger.poll(&mut source);
        assert_eq!(
            ledger.provenance(),
            LedgerProvenance::Fresh,
            "restoring valid content clears Stale"
        );
        assert_eq!(
            ledger.rows().len(),
            2,
            "the restored content's new row lands"
        );
        assert!(
            ledger.stale_reason().is_none(),
            "a recovered read must clear the stale reason"
        );
    }

    // ── AC3: exact-match join only ──────────────────────────────────────────────────────────────

    fn sub(id: &str) -> Subscription {
        Subscription {
            id: id.to_string(),
            status: SubStatus::Active,
            purchased: None,
            renews: None,
            cancelled_on: None,
            paid_through: None,
            verified: None,
        }
    }

    #[test]
    fn find_is_exact_match_only_no_near_miss() {
        let rows = vec![sub("claude-rob-7")];
        assert_eq!(
            find(&rows, "claude-rob7"),
            None,
            "a near-miss id must never join"
        );
        assert_eq!(
            find(&rows, "claude-rob-7"),
            Some(&rows[0]),
            "the exact id must join"
        );
    }

    #[test]
    fn find_is_case_sensitive() {
        let rows = vec![sub("claude-Alpha")];
        assert_eq!(find(&rows, "claude-alpha"), None);
    }

    // ── spec 018 §A (acceptance 1): parser reads `verified` ────────────────────────────────────

    #[test]
    fn verified_absent_is_none() {
        let toml = "\
[[subscription]]
id = \"claude-alpha\"
status = \"active\"
";
        let outcome = parse(toml).expect("well-formed ledger");
        assert_eq!(outcome.rows[0].verified, None);
    }

    #[test]
    fn verified_present_parses_to_date() {
        let toml = "\
[[subscription]]
id = \"claude-alpha\"
status = \"active\"
purchased = 2026-07-01
verified = 2026-07-18
";
        let outcome = parse(toml).expect("well-formed ledger");
        assert_eq!(
            outcome.rows[0].verified,
            Some(jiff::civil::date(2026, 7, 18))
        );
    }

    #[test]
    fn invalid_verified_degrades_only_that_row() {
        let toml = "\
[[subscription]]
id = \"claude-alpha\"
status = \"active\"
verified = 12:00:00

[[subscription]]
id = \"claude-bravo\"
status = \"active\"
";
        let outcome = parse(toml).expect("the file itself is well-formed TOML");
        assert_eq!(
            outcome
                .rows
                .iter()
                .map(|r| r.id.as_str())
                .collect::<Vec<_>>(),
            vec!["claude-bravo"],
            "only the row with the bad `verified` date drops"
        );
        assert_eq!(outcome.errors.len(), 1);
        assert_eq!(outcome.errors[0].id.as_deref(), Some("claude-alpha"));
        assert!(
            outcome.errors[0].reason.contains("verified"),
            "reason should name the bad field: {:?}",
            outcome.errors[0]
        );
    }

    // ── spec 018 §B (acceptance 2): verified-current math ──────────────────────────────────────

    fn sub_with(purchased: Option<Date>, verified: Option<Date>) -> Subscription {
        Subscription {
            id: "claude-alpha".to_string(),
            status: SubStatus::Active,
            purchased,
            renews: None,
            cancelled_on: None,
            paid_through: None,
            verified,
        }
    }

    #[test]
    fn verified_current_true_when_verified_on_or_after_purchased() {
        let purchased = jiff::civil::date(2026, 7, 1);
        let today = jiff::civil::date(2026, 7, 20);
        assert!(
            verified_current(&sub_with(Some(purchased), Some(purchased)), today),
            "verified == purchased must count as current"
        );
        assert!(verified_current(
            &sub_with(Some(purchased), Some(jiff::civil::date(2026, 7, 15))),
            today
        ));
    }

    #[test]
    fn verified_current_false_when_verified_before_purchased() {
        let purchased = jiff::civil::date(2026, 7, 1);
        let today = jiff::civil::date(2026, 7, 20);
        assert!(!verified_current(
            &sub_with(Some(purchased), Some(jiff::civil::date(2026, 6, 30))),
            today
        ));
    }

    #[test]
    fn verified_current_purchased_absent_uses_31_day_window() {
        let today = jiff::civil::date(2026, 7, 31);
        // exactly 31 days ago ⇒ still in the window
        assert!(verified_current(
            &sub_with(None, Some(jiff::civil::date(2026, 6, 30))),
            today
        ));
        // 32 days ago ⇒ out of the window
        assert!(!verified_current(
            &sub_with(None, Some(jiff::civil::date(2026, 6, 29))),
            today
        ));
    }

    #[test]
    fn verified_current_false_when_verified_absent() {
        let today = jiff::civil::date(2026, 7, 20);
        assert!(!verified_current(
            &sub_with(Some(jiff::civil::date(2026, 7, 1)), None),
            today
        ));
        assert!(!verified_current(&sub_with(None, None), today));
    }

    #[test]
    fn verified_current_false_when_verified_is_in_the_future() {
        // A typo'd ledger date (e.g. `verified = 2027-07-18`) must never render a confident pill —
        // `verified >= purchased` and the 31-day window are both trivially/vacuously satisfied by a
        // future date, so this needs its own explicit guard (not just coverage by the other cases).
        let today = jiff::civil::date(2026, 7, 20);
        let purchased = jiff::civil::date(2026, 7, 1);
        assert!(
            !verified_current(
                &sub_with(Some(purchased), Some(jiff::civil::date(2027, 7, 18))),
                today
            ),
            "future-dated verified with purchased present must not read as current"
        );
        assert!(
            !verified_current(&sub_with(None, Some(jiff::civil::date(2027, 7, 18))), today),
            "future-dated verified with purchased absent must not read as current"
        );
    }
}
