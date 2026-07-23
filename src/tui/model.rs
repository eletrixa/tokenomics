//! Dashboard state + transitions + the precomputed per-account view rows (pure state machine).
//!
//! Project: Tokenomics — monitor LLM subscription accounts (usage, limits, time-left) in a TUI
//! Module:  src/tui/model.rs
//! Deps:    ratatui (Color), jiff; domain + format (display logic); ledger (subscription clause)
//! Tested:  inline `#[cfg(test)]` — update/selection, severity→color, NO_COLOR, view-row build,
//!          show-inactive toggle + tagged/dimmed inactive rows (spec 014), age-aware overlay hint
//!          (spec 015 §C); `build_sub_view` clause math + `Account.active`/ledger-status
//!          independence + `ledger_fleet_note` tokens (spec 017 §D acceptance 4/5/7); the verified
//!          pill's suppression + degrade-form math (spec 018 §B/§C acceptance 2)
//!
//! Key responsibilities:
//! - `App`: selection, help/quit/show-inactive flags, colour policy, and the precomputed
//!   `AccountView` rows.
//! - `update(Msg)`: fold Key / Data (store) / Tick / Resize; selection clamps to the row count.
//! - `build_account_view`: pure store-data → display-ready row (so `view` only lays out); it now
//!   also does the exact-id ledger join (`ledger::find`) and calls `build_sub_view`, independent of
//!   `Account.active` (spec 017 §C/§D).
//! - `build_sub_view`: pure ledger-row + `today` → `SubView` (spec 017 §D clause text), now also
//!   folding in the verified-pill suffixes (`pill_suffixes`, spec 018 §B/§C) — suppressed on the
//!   stale-`renews` and derived-`ended` states.
//!
//! Design constraints:
//! - Everything the view needs is precomputed here; colour is a pure function of state.
//! - `NO_COLOR` is honoured via `use_color` (resolved once at startup and threaded through).

use jiff::Timestamp;
use ratatui::style::Color;

use crate::domain::{Account, Limit, LimitKind, Provenance, Provider, Severity, UsageSnapshot};
use crate::format::{
    format_ago, format_ago_ms, format_cost, format_dollars, format_pct, format_reset,
    format_tokens, provenance_label, provenance_short, reset_expired, severity_label, RESET_DONE,
};
use crate::ledger::{
    find as ledger_find, verified_current, LedgerProvenance, SubStatus, Subscription,
};
use crate::store::TokenStatus;
use crate::tui::keys::Action;

/// The local ccusage plane is flagged **stale** once its newest data is older than this many local
/// poll intervals (spec 011 §B) — the age segment then styles as a warning instead of dim.
const LOCAL_STALE_FACTOR: i64 = 2;
/// The collector is treated as **down** once its heartbeat is older than this many local poll
/// intervals (spec 011 §A) — beyond normal jitter, the writer has stopped.
const COLLECTOR_DOWN_FACTOR: i64 = 3;

/// A single-line gauge's display data (already resolved for the current colour policy). The parts
/// are kept structured (not a single pre-joined label) so each responsive tier can compose them:
/// FULL/COMPACT render `"[scope ]pct sev · resets reset"`, MICRO renders the columns separately.
#[derive(Debug, Clone)]
pub struct GaugeView {
    /// Fill fraction 0.0–1.0 (drives the proportional bar).
    pub ratio: f64,
    /// Utilization percent, e.g. `"76%"`.
    pub pct: String,
    /// Severity tier (drives the colour + the `● / ▲ / ✖` glyph + the `ok/warn/crit` word).
    pub severity: Severity,
    /// The reset countdown rendered verbatim from its source, e.g. `"in 2h 41m"` /
    /// `"waiting for reset"`.
    pub reset: Option<String>,
    /// Optional scope label (a model family) for a scoped weekly gauge, e.g. `"Fable"`.
    pub scope: Option<String>,
    /// Bar/label colour (already `Reset` when colour is disabled).
    pub color: Color,
    /// The limit's reset time has passed: the window reset, so the stored percent is history — the
    /// gauge renders dormant (`"waiting for reset"`) and never alarms (spec 012 §A).
    pub expired: bool,
}

impl GaugeView {
    /// Compose the roomy right-hand label: `"[scope ]pct sev[ · resets reset]"`, e.g.
    /// `"Fable 92% crit · resets in 5d 9h"`. Used by the FULL and COMPACT tiers.
    pub fn label(&self) -> String {
        // An expired gauge says only what is true: the window reset and we're waiting for fresh
        // evidence — no stale percent, no "ok" filler (spec 012 §A).
        if self.expired {
            return match &self.scope {
                Some(scope) => format!("{scope} {RESET_DONE}"),
                None => RESET_DONE.to_string(),
            };
        }
        let mut s = String::new();
        if let Some(scope) = &self.scope {
            s.push_str(scope);
            s.push(' ');
        }
        s.push_str(&self.pct);
        s.push(' ');
        s.push_str(severity_label(self.severity));
        if let Some(reset) = &self.reset {
            // A past reset reads standalone; a countdown takes the "resets" verb ("· resets in
            // 2h 41m"). Never "resets waiting for reset".
            s.push_str(if reset == RESET_DONE {
                " · "
            } else {
                " · resets "
            });
            s.push_str(reset);
        }
        s
    }

    /// The reset countdown without the leading `"in "` filler, for the tight MICRO columns
    /// (`"in 2h 41m"` → `"2h 41m"`). The time value itself is never reformatted (verbatim rule).
    pub fn reset_short(&self) -> Option<&str> {
        self.reset
            .as_deref()
            .map(|r| r.strip_prefix("in ").unwrap_or(r))
    }
}

/// A small coloured badge (e.g. the provenance tag).
#[derive(Debug, Clone)]
pub struct Badge {
    /// Badge text, e.g. `"derived"`.
    pub text: String,
    /// Abbreviated badge text for tight tiers, e.g. `"drv"`.
    pub short: String,
    /// Badge colour (already resolved for the colour policy).
    pub color: Color,
}

/// The precomputed subscription-lifecycle clause (ledger plane, spec 017 §D) for one account's
/// header — every FULL/COMPACT form `view.rs` might need, so it stays a pure lookup with zero date
/// math at render time. All `None` means "the ledger has nothing to say" (unmatched id, plane off,
/// or an active row with no dates) — the header then renders byte-identical to the pre-spec-017 form.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SubView {
    /// FULL tier, roomiest form (start segment + absolute date [+ the ` ✓ <date>` pill when
    /// verified-current, spec 018 §C]), e.g.
    /// `"· period 2026-07-14 → · renews in 27d (2026-08-14) ✓ 2026-07-18"`.
    pub full: Option<String>,
    /// FULL tier, degrade step 1: the pill's own date dropped (a bare ` ✓` stays, spec 018 §C) —
    /// everything else identical to `full`. Equal to `full` when no pill applies.
    pub full_no_pill_date: Option<String>,
    /// FULL tier, degrade step 2: the start segment also dropped.
    pub full_no_start: Option<String>,
    /// FULL tier, degrade step 3: the absolute `(…)` date also dropped.
    pub full_no_date: Option<String>,
    /// COMPACT tier's short dim clause, e.g. `"· renews 27d"` / `"· ends 4d"` / `"· cancelled"` —
    /// already carries a trailing `" ✓"` when verified-current (spec 018 §C), so the clause and its
    /// pill render or drop together in the two-step ladder.
    pub compact: Option<String>,
    /// The roomiest pill suffix (` ✓ <date>`), present only in `full`. `view.rs` uses this (and
    /// [`Self::pill_bare`]) to style the pill distinctly from the rest of the FULL-tier title.
    pub pill_full: Option<String>,
    /// The bare pill suffix (` ✓`, no date), present in `full_no_pill_date`/`full_no_start`/
    /// `full_no_date` when verified-current.
    pub pill_bare: Option<String>,
}

/// Whole calendar days from `from` to `to` (positive when `to` is in the future) — pure date math,
/// no wall-clock/timezone involved beyond the injected `civil::Date`s themselves. `Date - Date`
/// never fails (jiff guarantees this for two `civil::Date`s), but `since` is used over the `-`
/// operator so no runtime path here can ever reach an `.expect()`; the `map_or(0, …)` fallback is
/// unreachable in practice, not a silently-wrong default.
fn days_between(from: jiff::civil::Date, to: jiff::civil::Date) -> i32 {
    to.since(from).map_or(0, |span| span.get_days())
}

/// `"today"` for a zero-day difference, else `"in {n}d"` (FULL tier's relative phrase).
fn day_phrase_full(days: i32) -> String {
    if days == 0 {
        "today".to_string()
    } else {
        format!("in {days}d")
    }
}

/// `"today"` for a zero-day difference, else `"{n}d"` (COMPACT tier's shorter relative phrase).
fn day_phrase_compact(days: i32) -> String {
    if days == 0 {
        "today".to_string()
    } else {
        format!("{days}d")
    }
}

/// Append `suffix` (when present) to `base`, else return `base` unchanged. Used to fold the
/// verified-pill suffix (spec 018 §C) onto an already-built clause string.
fn append_opt(base: &str, suffix: Option<&str>) -> String {
    suffix.map_or_else(|| base.to_string(), |s| format!("{base}{s}"))
}

/// The verified pill's two suffix forms (spec 018 §C) for a pill-eligible clause: `(" ✓ <date>",
/// " ✓")` when `sub` is verified-current, else `(None, None)` — the caller never applies these to a
/// suppressed state (stale-`renews` / ended, spec 018 §B) by simply not calling this there.
fn pill_suffixes(sub: &Subscription, today: jiff::civil::Date) -> (Option<String>, Option<String>) {
    if !verified_current(sub, today) {
        return (None, None);
    }
    let Some(verified) = sub.verified else {
        return (None, None);
    };
    (Some(format!(" ✓ {verified}")), Some(" ✓".to_string()))
}

/// Build the `active` states' clause (spec 017 §D table, rows 1–2; spec 018 §B/§C pill): a
/// future/`today` `renews` gets the full "period → renews" form, plus the verified pill when
/// verified-current; a past `renews` gets the never-negative stale marker AND suppresses the pill
/// (a stale-marked clause never shows `✓`, spec 018 §B); no `renews` at all carries no information.
fn build_active_sub_view(sub: &Subscription, today: jiff::civil::Date) -> SubView {
    let Some(renews) = sub.renews else {
        return SubView::default();
    };
    let days = days_between(today, renews);
    let date = renews.to_string();
    if days < 0 {
        // The renewal date is in the past and no new one has landed — the ledger is stale, not the
        // subscription negative. Never a negative countdown (acceptance 4), never a pill.
        let full = format!("· renews {date} (past — ledger stale?)");
        return SubView {
            full: Some(full.clone()),
            full_no_pill_date: Some(full.clone()),
            full_no_start: Some(full.clone()),
            full_no_date: Some(full),
            compact: Some("· renews ?".to_string()),
            pill_full: None,
            pill_bare: None,
        };
    }
    let (pill_full, pill_bare) = pill_suffixes(sub, today);
    let renews_clause = format!("· renews {} ({date})", day_phrase_full(days));
    let renews_clause_no_date = format!("· renews {}", day_phrase_full(days));
    let base_full = sub.purchased.map_or_else(
        || renews_clause.clone(),
        |purchased| format!("· period {purchased} → {renews_clause}"),
    );
    let compact_base = format!("· renews {}", day_phrase_compact(days));
    SubView {
        full: Some(append_opt(&base_full, pill_full.as_deref())),
        full_no_pill_date: Some(append_opt(&base_full, pill_bare.as_deref())),
        full_no_start: Some(append_opt(&renews_clause, pill_bare.as_deref())),
        full_no_date: Some(append_opt(&renews_clause_no_date, pill_bare.as_deref())),
        compact: Some(append_opt(&compact_base, pill_bare.as_deref())),
        pill_full,
        pill_bare,
    }
}

/// Build the `cancelled` states' clause (spec 017 §D table, rows 3–5; spec 018 §B/§C pill): a
/// future/`today` `paid_through` reads "ends", verb-swapped but visually parallel to `renews`, plus
/// the verified pill when verified-current; a past one reads "ended" (dimmed by the caller, never a
/// pill — the derived "ended" state suppresses it, spec 018 §B); an unknown one is the bare
/// `"· cancelled"` label — still pill-eligible, since the status alone is information.
fn build_cancelled_sub_view(sub: &Subscription, today: jiff::civil::Date) -> SubView {
    let Some(paid_through) = sub.paid_through else {
        let (pill_full, pill_bare) = pill_suffixes(sub, today);
        let bare = "· cancelled".to_string();
        let full = append_opt(&bare, pill_full.as_deref());
        let narrow = append_opt(&bare, pill_bare.as_deref());
        return SubView {
            full: Some(full),
            full_no_pill_date: Some(narrow.clone()),
            full_no_start: Some(narrow.clone()),
            full_no_date: Some(narrow.clone()),
            compact: Some(narrow),
            pill_full,
            pill_bare,
        };
    };
    let days = days_between(today, paid_through);
    let date = paid_through.to_string();
    if days < 0 {
        let full = format!("· cancelled · ended {date}");
        return SubView {
            full: Some(full.clone()),
            full_no_pill_date: Some(full.clone()),
            full_no_start: Some(full.clone()),
            full_no_date: Some(full),
            compact: Some("· ended".to_string()),
            pill_full: None,
            pill_bare: None,
        };
    }
    let (pill_full, pill_bare) = pill_suffixes(sub, today);
    let base = format!("· cancelled · ends {} ({date})", day_phrase_full(days));
    let base_no_date = format!("· cancelled · ends {}", day_phrase_full(days));
    let narrow = append_opt(&base, pill_bare.as_deref());
    let compact_base = format!("· ends {}", day_phrase_compact(days));
    SubView {
        full: Some(append_opt(&base, pill_full.as_deref())),
        full_no_pill_date: Some(narrow.clone()),
        full_no_start: Some(narrow),
        full_no_date: Some(append_opt(&base_no_date, pill_bare.as_deref())),
        compact: Some(append_opt(&compact_base, pill_bare.as_deref())),
        pill_full,
        pill_bare,
    }
}

/// Build one account's ledger clause (spec 017 §D), pure with `today` injected. `sub` is the
/// already-joined ledger row (via [`crate::ledger::find`]) — `None` for an unmatched id, an off
/// plane, or before the first poll. `Account.active` never enters this function — the ledger's
/// `status` is the only input (spec 017 §C independence).
pub fn build_sub_view(sub: Option<&Subscription>, today: jiff::civil::Date) -> SubView {
    let Some(sub) = sub else {
        return SubView::default();
    };
    match sub.status {
        SubStatus::Active => build_active_sub_view(sub, today),
        SubStatus::Cancelled => build_cancelled_sub_view(sub, today),
    }
}

/// The fleet header's one dim ledger-plane token (spec 017 §D): `Missing` → `"· no ledger"`,
/// `Stale` → `"· ledger stale"`, `Fresh` with zero matched rows → `"· ledger: 0 matched"`. `Off` (and
/// `Fresh` with ≥1 matched row — the per-account clauses already say enough) render nothing.
pub fn ledger_fleet_note(provenance: LedgerProvenance, matched: usize) -> Option<String> {
    match provenance {
        LedgerProvenance::Missing => Some("· no ledger".to_string()),
        LedgerProvenance::Stale => Some("· ledger stale".to_string()),
        LedgerProvenance::Fresh if matched == 0 => Some("· ledger: 0 matched".to_string()),
        LedgerProvenance::Off | LedgerProvenance::Fresh => None,
    }
}

/// One account's display-ready row. `view` only lays these out — no computation.
#[derive(Debug, Clone)]
pub struct AccountView {
    /// Panel title, e.g. `"Personal [claude]"`.
    pub title: String,
    /// Accent colour for the panel border.
    pub accent: Color,
    /// The 5h session gauge, when a session limit exists.
    pub session: Option<GaugeView>,
    /// The weekly (all-models) gauge — `None` until the overlay lands ⇒ [`Self::weekly_hint`].
    pub weekly: Option<GaugeView>,
    /// The fallback note for a missing weekly gauge, honest about WHY it is missing: opt in, refresh
    /// the token, wait for the next overlay pass, or (spec 015 §C) age a past success that has gone
    /// silent. Owned because that last case is computed text, not a fixed literal.
    pub weekly_hint: String,
    /// The most-utilized per-model weekly gauge (e.g. `"Fable 92% crit"`), when the overlay
    /// reports a scoped weekly limit. `None` otherwise (no extra line is drawn).
    pub weekly_scoped: Option<GaugeView>,
    /// The single most-utilized gauge across session/weekly/scoped — the scariest number. Drives the
    /// one-line MICRO tier and the worst-offender banner so a glance always lands on the real risk.
    pub headline: Option<GaugeView>,
    /// A status note shown when there is no active session (e.g. `"idle"`, `"no data yet"`).
    pub status: Option<String>,
    /// The row's overall severity (drives the alert banner).
    pub severity: Severity,
    /// Mirrors `Account.active == false` (spec 014). An inactive row is only ever present in
    /// `App::rows` while `show_inactive` is on; it always stays excluded from the alert banner,
    /// the warn count, and the fleet reductions — see [`App::alert_count`] and `view::worst`.
    pub inactive: bool,
    /// The ledger-plane clause (spec 017 §D), precomputed and pure — `view.rs` renders whichever
    /// degrade-order form fits, never computing dates itself. See [`SubView`].
    pub sub: SubView,
}

/// The fleet-wide usage line: the shared token / cost / burn figures shown **once** in the header
/// instead of repeated on every panel. On this deployment every account reads the same physical logs
/// (a shared `projects/` symlink — see spec 010), so a per-account meta line is the same number four
/// times; this collapses it to one row. Reducers take the representative usage and the *worst*
/// provenance / *oldest* refresh, so a single degraded account still surfaces.
// ponytail: the token/cost/burn reducers assume the shared-logs invariant (identical per-account
// usage). If accounts ever get their own real `projects/`, switch those reducers from max→sum for a
// true fleet total.
#[derive(Debug, Clone)]
pub struct FleetView {
    /// Shared total tokens, e.g. `"445.63M"` (or `"—"`).
    pub tokens: String,
    /// Notional cost, fully labeled: `"$382.65 (notional)"` (or `"—"`).
    pub cost_notional: String,
    /// Whole-dollar notional cost, self-labeled for tight widths: `"$382n"` (or `"—"`).
    pub cost_short: String,
    /// Fleet burn rate as tokens/hour, e.g. `"232.70M/h"` (or `None` when idle).
    pub burn_rate: Option<String>,
    /// The worst (most degraded) provenance across accounts.
    pub provenance: Option<Badge>,
    /// `"usage 12s ago"` — the local ccusage plane's age, from the *newest* snapshot across accounts.
    /// Present whenever any account has a snapshot, independent of the opt-in overlay (spec 011 §B).
    pub usage_age: Option<String>,
    /// Whether the local plane is stale (older than `LOCAL_STALE_FACTOR × poll_local_secs`) — the
    /// view then styles `usage_age` as a warning so a frozen number *looks* frozen.
    pub usage_stale: bool,
    /// `"limits 4m ago"` — the overlay/authoritative plane's age, from the *oldest* overlay refresh
    /// across accounts (most stale wins). Distinct from `usage_age` so the two planes never conflate.
    pub overlay_age: Option<String>,
    /// The ledger plane's one dim fleet-header token (spec 017 §D) — see [`ledger_fleet_note`].
    pub ledger_note: Option<String>,
}

/// One account's shared usage facts, extracted from its store reads — the raw numeric inputs to the
/// fleet reduction (kept numeric, unlike the pre-formatted `AccountView` strings, so they reduce).
#[derive(Debug, Clone, Copy, Default)]
pub struct AccountUsage {
    /// Total tokens in the account's latest snapshot.
    pub total_tokens: Option<u64>,
    /// Notional cost of the latest snapshot.
    pub cost_notional: Option<f64>,
    /// Active-window burn rate (ccusage tokens/minute), when a block is actively burning.
    pub tokens_per_minute: Option<f64>,
    /// The account's session-limit provenance (drives the fleet badge).
    pub provenance: Option<Provenance>,
    /// Epoch-millis of the account's last successful overlay fetch, if any (overlay-plane freshness).
    pub overlay_ms: Option<i64>,
    /// Epoch-millis of the account's latest snapshot, if any (local ccusage-plane freshness).
    pub collected_at_ms: Option<i64>,
}

/// One full store read: the per-account rows, the fleet-wide aggregate burn series, and the fleet
/// usage line (all read in a single tick so the header and the panels never mix data from different
/// reads).
#[derive(Debug, Default)]
pub struct Dashboard {
    /// The per-account display rows.
    pub rows: Vec<AccountView>,
    /// `Σ burn_tpm` per collection tick, oldest → newest (the header aggregate sparkline).
    pub aggregate_burn: Vec<u64>,
    /// The fleet-wide usage line, or `None` when no account has any data yet.
    pub fleet: Option<FleetView>,
    /// A collector-liveness alert (banner text) when the writer looks down/stalled, else `None`
    /// (spec 011 §A). Computed at the store-read site from the collector heartbeat age.
    pub collector_alert: Option<String>,
}

impl From<Vec<AccountView>> for Dashboard {
    /// Rows with no aggregate series or fleet line (used by tests and any rows-only refresh).
    fn from(rows: Vec<AccountView>) -> Self {
        Self {
            rows,
            aggregate_burn: Vec::new(),
            fleet: None,
            collector_alert: None,
        }
    }
}

/// A message folded by [`App::update`].
#[derive(Debug)]
pub enum Msg {
    /// A mapped key action.
    Key(Action),
    /// Fresh rows + aggregate series + fleet line read from the store. Boxed: it dwarfs the other
    /// variants, so a bare `Dashboard` would bloat every `Msg` (clippy `large_enum_variant`).
    Data(Box<Dashboard>),
    /// A periodic tick (clears the transient message).
    Tick,
    /// The terminal was resized (redraw happens naturally).
    Resize,
}

/// The dashboard state.
// Five independent UI flags (quit / help / reload / colour policy / show-inactive); grouping
// unrelated booleans into an enum would obscure, not clarify.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug)]
pub struct App {
    /// Precomputed per-account rows — only the *visible* set (active, plus inactive when
    /// `show_inactive` is on). Selection/scrolling operate over this set directly (spec 014 §C).
    pub rows: Vec<AccountView>,
    /// Fleet-wide burn-rate series (`Σ burn_tpm` per tick, oldest → newest) for the header bar.
    pub aggregate_burn: Vec<u64>,
    /// The fleet-wide usage line (shared token/cost/burn/provenance/refresh), or `None` before data.
    pub fleet: Option<FleetView>,
    /// Collector-liveness alert (banner text) when the writer looks down/stalled, else `None`.
    pub collector_alert: Option<String>,
    /// Selected row index (clamped to `rows`).
    pub selected: usize,
    /// Set when the user asks to quit.
    pub should_quit: bool,
    /// Whether the help overlay is shown.
    pub show_help: bool,
    /// Set by `r` or `i`; the loop re-reads the store and clears it.
    pub reload_requested: bool,
    /// Colour policy (false when `NO_COLOR` is set).
    pub use_color: bool,
    /// Whether inactive accounts are included in `rows` (toggled by `i`; default off — spec 014 §C).
    pub show_inactive: bool,
    /// Number of visible accounts (for the header). Tracks `rows.len()` once data has landed; before
    /// the first store read it holds the caller's initial estimate.
    pub account_count: usize,
    /// A transient footer message.
    pub message: Option<String>,
}

impl App {
    /// Build an empty dashboard for an initial `account_count` (the caller passes the *visible*
    /// count for `show_inactive`'s default-off state, i.e. active accounts only).
    pub fn new(account_count: usize, use_color: bool) -> Self {
        Self {
            rows: Vec::new(),
            aggregate_burn: Vec::new(),
            fleet: None,
            collector_alert: None,
            selected: 0,
            should_quit: false,
            show_help: false,
            reload_requested: false,
            use_color,
            show_inactive: false,
            account_count,
            message: None,
        }
    }

    /// Fold a message into the state.
    pub fn update(&mut self, msg: Msg) {
        match msg {
            Msg::Key(action) => self.handle(action),
            Msg::Data(data) => {
                let data = *data;
                self.rows = data.rows;
                self.aggregate_burn = data.aggregate_burn;
                self.fleet = data.fleet;
                self.collector_alert = data.collector_alert;
                // Rows already reflect the current `show_inactive` filter (see `read_rows`), so the
                // header count tracks exactly what's on screen — including across a toggle.
                self.account_count = self.rows.len();
                self.clamp_selection();
            }
            Msg::Tick => self.message = None,
            Msg::Resize => {}
        }
    }

    fn handle(&mut self, action: Action) {
        match action {
            Action::Quit => self.should_quit = true,
            Action::Up => self.select_prev(),
            Action::Down => self.select_next(),
            Action::Refresh => {
                self.reload_requested = true;
                self.message = Some("refreshed".to_string());
            }
            Action::Help => self.show_help = !self.show_help,
            Action::ToggleInactive => {
                self.show_inactive = !self.show_inactive;
                self.reload_requested = true;
            }
        }
    }

    fn select_next(&mut self) {
        if !self.rows.is_empty() && self.selected + 1 < self.rows.len() {
            self.selected += 1;
        }
    }

    fn select_prev(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    fn clamp_selection(&mut self) {
        if self.rows.is_empty() {
            self.selected = 0;
        } else if self.selected >= self.rows.len() {
            self.selected = self.rows.len() - 1;
        }
    }

    /// How many rows are at or above `Warn` (drives the alert banner). Inactive rows are excluded
    /// even when shown — peeking at a dead account must never raise the banner (spec 014 §C).
    pub fn alert_count(&self) -> usize {
        self.rows
            .iter()
            .filter(|r| !r.inactive && r.severity != Severity::Ok)
            .count()
    }
}

/// Map a severity to its bar/text colour (a pure function of state).
pub fn severity_color(severity: Severity) -> Color {
    match severity {
        Severity::Ok => Color::Green,
        Severity::Warn => Color::Yellow,
        Severity::Crit => Color::Red,
    }
}

/// Map a severity to an escalating status glyph (calm dot → caution triangle → cross). Paired with
/// the `ok/warn/crit` word so severity survives `NO_COLOR` — the glyph is never the sole cue.
pub fn severity_glyph(severity: Severity) -> &'static str {
    match severity {
        Severity::Ok => "●",
        Severity::Warn => "▲",
        Severity::Crit => "✖",
    }
}

/// Map a provenance to its badge colour.
pub fn provenance_color(source: Provenance) -> Color {
    match source {
        Provenance::Authoritative => Color::Green,
        Provenance::Derived => Color::Cyan,
        Provenance::Estimate => Color::DarkGray,
    }
}

/// Apply the colour policy: the real colour when enabled, else `Reset` (honours `NO_COLOR`).
pub fn resolve_color(use_color: bool, color: Color) -> Color {
    if use_color {
        color
    } else {
        Color::Reset
    }
}

/// The accent colour for an account's panel border (from its configured colour, else a default).
fn account_color(account: &Account, use_color: bool) -> Color {
    let color = account
        .color
        .as_deref()
        .and_then(parse_named_color)
        .unwrap_or(Color::Cyan);
    resolve_color(use_color, color)
}

/// Parse a named ratatui colour or a `#rrggbb` hex string.
fn parse_named_color(name: &str) -> Option<Color> {
    let lower = name.trim().to_ascii_lowercase();
    if let Some(hex) = lower.strip_prefix('#') {
        if hex.len() == 6 {
            let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
            let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
            let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
            return Some(Color::Rgb(r, g, b));
        }
        return None;
    }
    let color = match lower.as_str() {
        "black" => Color::Black,
        "red" => Color::Red,
        "green" => Color::Green,
        "yellow" => Color::Yellow,
        "blue" => Color::Blue,
        "magenta" => Color::Magenta,
        "cyan" => Color::Cyan,
        "gray" | "grey" => Color::Gray,
        "darkgray" | "darkgrey" => Color::DarkGray,
        "white" => Color::White,
        "lightred" => Color::LightRed,
        "lightgreen" => Color::LightGreen,
        "lightyellow" => Color::LightYellow,
        "lightblue" => Color::LightBlue,
        "lightmagenta" => Color::LightMagenta,
        "lightcyan" => Color::LightCyan,
        _ => return None,
    };
    Some(color)
}

/// One account's stored data, as read by the TUI loop — the raw inputs to [`build_account_view`].
/// Bundled so the builder stays a small `(account, data, now, use_color)` call. All fields are
/// references or small `Copy` scalars, so the bundle itself is `Copy` (cheap to pass by value).
#[derive(Debug, Clone, Copy)]
pub struct AccountData<'a> {
    /// The last-good usage snapshot, if any.
    pub snapshot: Option<&'a UsageSnapshot>,
    /// The current merged limit set (session + any weekly rows).
    pub limits: &'a [Limit],
    /// The account's token freshness, if recorded.
    pub token_status: Option<TokenStatus>,
    /// Epoch-millis the overlay has been failing continuously since, if it is failing now.
    pub overlay_failing_since: Option<i64>,
    /// Epoch-millis of the account's last successful overlay fetch, if any — ages the "waiting for
    /// overlay" hint honestly once a past success has gone silent without a recorded failure
    /// (spec 015 §C; the same value `account_usage` reads for the fleet header).
    pub overlay_ms: Option<i64>,
    /// The ledger plane's current rows (spec 017 §C), or empty when `Off`/`Missing`/unpolled.
    /// `build_account_view` joins these against `account.id` via `ledger::find` (exact match only).
    pub ledger_rows: &'a [Subscription],
    /// "Today" for the ledger's day-count math (spec 017 §D) — injected so the date math inside
    /// `build_sub_view` stays pure; the caller derives it from `now` once per read.
    pub today: jiff::civil::Date,
}

/// How long an opted-in overlay must fail continuously before the row flags "check account". A live
/// account's 429s clear on the next success well within this; only a dead/blocked subscription (which
/// 429s every pass) stays failing long enough to trip it. Also the grace before a just-started
/// collector's first pending pass is called stalled.
const OVERLAY_STALL_MS: i64 = 15 * 60 * 1000;

/// Build one account's display-ready row from its stored data. Pure (`now` injected).
pub fn build_account_view(
    account: &Account,
    data: AccountData<'_>,
    now: Timestamp,
    use_color: bool,
) -> AccountView {
    let AccountData {
        snapshot,
        limits,
        token_status,
        overlay_failing_since,
        overlay_ms,
        ledger_rows,
        today,
    } = data;

    // Exact-id join (spec 017 §C) — `Account.active` never enters this lookup or `build_sub_view`,
    // so an inactive (config-flagged) account still shows an active ledger clause and vice versa.
    let sub = build_sub_view(ledger_find(ledger_rows, &account.id), today);

    // Every gauge carries its OWN reset countdown (like Claude's /usage) — session, weekly-all, and
    // the per-model scoped weekly — so each line reads consistently: <pct> <sev> · resets <when>.
    let session_limit = limits.iter().find(|l| l.kind == LimitKind::Session);
    let session = session_limit.map(|limit| gauge_from_limit(limit, None, now, use_color));

    let weekly = limits
        .iter()
        .find(|l| l.kind == LimitKind::WeeklyAll)
        .map(|limit| gauge_from_limit(limit, None, now, use_color));
    let weekly_scoped = limits
        .iter()
        .filter(|l| l.kind == LimitKind::WeeklyScoped)
        .max_by(|a, b| a.utilization_pct.total_cmp(&b.utilization_pct))
        .map(|limit| gauge_from_limit(limit, limit.scope.as_deref(), now, use_color));

    // The headline is the single most-utilized gauge (session/weekly/scoped) — the scariest number.
    // The one-line MICRO tier and the worst-offender banner render this, so the eye lands on real risk.
    let headline = [&session, &weekly, &weekly_scoped]
        .into_iter()
        .flatten()
        .max_by(|a, b| a.ratio.total_cmp(&b.ratio))
        .cloned();
    // An inactive account (shown only via `i`) renders its last-known rows dimmed, uniformly, so a
    // peeked-at dead account never competes visually with a live one's severity colour (spec 014 §C).
    let (session, weekly, weekly_scoped, headline) = if account.active {
        (session, weekly, weekly_scoped, headline)
    } else {
        let dim = |g: Option<GaugeView>| g.map(|g| dim_gauge(g, use_color));
        (dim(session), dim(weekly), dim(weekly_scoped), dim(headline))
    };

    // Token / cost / burn / provenance / refresh are identical across accounts (shared logs) and now
    // render once on the fleet header line — see `build_fleet_view` / `account_usage`. This per-account
    // row keeps only what differs between accounts: its gauges, status, and severity.
    let status = if token_status == Some(TokenStatus::Stale) {
        Some("token stale — open Claude to refresh".to_string())
    } else {
        match snapshot {
            None => Some("no data yet".to_string()),
            Some(s) if s.window.is_none() => Some("idle (no active block)".to_string()),
            Some(_) => None,
        }
    };
    // The row's severity is the worst of its NON-EXPIRED limits, so a critical scoped weekly lights
    // the banner even when the 5h window is calm — but a limit whose reset already passed is history
    // (the window reset) and must stop alarming the moment the countdown crosses zero (spec 012 §A).
    let severity = limits
        .iter()
        .filter(|l| !reset_expired(&l.resets_at, now))
        .map(|l| l.severity)
        .max()
        .unwrap_or(Severity::Ok);

    // The weekly fallback must not nudge "enable overlay" at an account that already opted in —
    // when the overlay is on but absent, the honest reasons are a stale token or a pending pass.
    let overlay_stalled = overlay_failing_since
        .is_some_and(|since| now.as_millisecond().saturating_sub(since) >= OVERLAY_STALL_MS);
    // A past overlay success that has since gone quiet with no *failed* attempt recorded (the
    // collector never retried it, e.g. spec 015's hot-reload gap) must not keep claiming "waiting" —
    // that reads as freshly pending when it is really stale. Aging the last success is the honest
    // signal; a genuinely never-succeeded account keeps the plain waiting hint.
    // Reached only via the `else if` below, i.e. once `overlay_stalled` is already known false —
    // that is the "stall flag hasn't tripped" condition from spec 015 §C.
    let overlay_silent_since_ms =
        overlay_ms.filter(|&ms| now.as_millisecond().saturating_sub(ms) > OVERLAY_STALL_MS);
    // Gemini has no limits/quota surface at all (spec 020 §C) — `limits_overlay` is accepted but
    // IGNORED, so neither "enable overlay" (nothing to enable) nor "waiting for overlay" (nothing
    // will ever arrive) is honest, however the flag is set. Grok's weekly quota comes from its
    // LOCAL billing log (spec 022 §D — overlay equally ignored): when no row exists the honest
    // reason is that the grok CLI hasn't logged a quota line for a live period yet. Every other
    // provider keeps the overlay-state hint below.
    let weekly_hint = if account.provider == Provider::Gemini {
        "n/a (no limits surface)".to_string()
    } else if account.provider == Provider::Grok {
        "n/a (awaiting grok billing log)".to_string()
    } else if !account.limits_overlay {
        "n/a (enable overlay)".to_string()
    } else if token_status == Some(TokenStatus::Stale) {
        "n/a (token stale — open Claude)".to_string()
    } else if overlay_stalled {
        // Warm token, opted in, but the overlay has failed every pass for a while — the account's
        // subscription is likely gone/blocked (a dead sub 429s /api/oauth/usage indefinitely).
        "n/a (overlay stalled — check account)".to_string()
    } else if let Some(ms) = overlay_silent_since_ms {
        format!("n/a (overlay silent {})", format_ago(ms, now))
    } else {
        "n/a (waiting for overlay)".to_string()
    };

    AccountView {
        title: account_title(account),
        accent: account_accent(account, use_color),
        session,
        weekly,
        weekly_hint,
        weekly_scoped,
        headline,
        status,
        severity,
        inactive: !account.active,
        sub,
    }
}

/// Panel title, e.g. `"Personal [claude]"` — or `"Personal [claude] (inactive)"` when the account is
/// unsubscribed/paused. The tag lives in the title text (not just colour) so it survives `NO_COLOR`
/// and appears everywhere the title does (FULL/COMPACT/MICRO all render this same field).
fn account_title(account: &Account) -> String {
    let tag = if account.active { "" } else { " (inactive)" };
    format!("{} [{}]{tag}", account.label, account.provider)
}

/// The account's accent colour: its configured colour when active, or flattened to dim grey when
/// inactive (spec 014 §C, mirrors `dim_gauge`). Shared by `build_account_view` and `error_view` so
/// a store-read error never draws a stale-but-colourful accent for an account marked inactive.
fn account_accent(account: &Account, use_color: bool) -> Color {
    if account.active {
        account_color(account, use_color)
    } else {
        resolve_color(use_color, Color::DarkGray)
    }
}

/// Flatten a gauge's colour to dim grey — keeps its ratio/pct/severity/label data intact (still
/// accurate), just strips the colour emphasis (spec 014 §C: "dimmed", not hidden).
fn dim_gauge(mut g: GaugeView, use_color: bool) -> GaugeView {
    g.color = resolve_color(use_color, Color::DarkGray);
    g
}

/// Convert ccusage's tokens/minute burn rate to whole tokens/hour. Clamped non-negative and finite;
/// the `as u64` is bounded (any realistic rate fits u64 after the ×60), so truncation/sign loss are
/// unreachable — the `allow` documents that, matching the ccusage module's cast policy.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn tokens_per_hour(tokens_per_minute: f64) -> u64 {
    let per_hour = (tokens_per_minute * 60.0).round();
    if per_hour.is_finite() && per_hour >= 0.0 {
        per_hour as u64
    } else {
        0
    }
}

/// Build a gauge from a limit: keeps the parts structured (pct, severity, verbatim reset, scope) so
/// each tier composes its own label. `scope` names a model family (scoped weeklies); the reset is the
/// countdown to *this* limit's own reset, so every gauge shows the reset it belongs to.
fn gauge_from_limit(
    limit: &Limit,
    scope: Option<&str>,
    now: Timestamp,
    use_color: bool,
) -> GaugeView {
    // Reset already passed ⇒ the window reset and the stored percent is history, not state. Render
    // a dormant gauge — empty dim bar, no percent, Ok — that reads "waiting for reset" until fresh
    // evidence (a collect, or an overlay success after login) brings the new countdown (spec 012 §A).
    if reset_expired(&limit.resets_at, now) {
        return GaugeView {
            ratio: 0.0,
            pct: "—".to_string(),
            severity: Severity::Ok,
            reset: Some(RESET_DONE.to_string()),
            scope: scope.map(str::to_string),
            color: resolve_color(use_color, Color::DarkGray),
            expired: true,
        };
    }
    GaugeView {
        ratio: (limit.utilization_pct / 100.0).clamp(0.0, 1.0),
        pct: format_pct(limit.utilization_pct),
        severity: limit.severity,
        // An idle window carries no reset time (`resets_at: ""`) — no countdown, no "resets" tail.
        reset: (!limit.resets_at.is_empty()).then(|| format_reset(&limit.resets_at, now)),
        scope: scope.map(str::to_string),
        color: resolve_color(use_color, severity_color(limit.severity)),
        expired: false,
    }
}

/// Extract one account's shared usage facts from its store reads (pure) — the numeric inputs the
/// fleet reduction needs, mirroring how `build_account_view` picks the session provenance and the
/// active-window burn rate.
pub fn account_usage(
    snapshot: Option<&UsageSnapshot>,
    limits: &[Limit],
    overlay_ms: Option<i64>,
) -> AccountUsage {
    AccountUsage {
        total_tokens: snapshot.map(|s| s.total_tokens),
        cost_notional: snapshot.and_then(|s| s.cost_notional),
        tokens_per_minute: snapshot
            .and_then(|s| s.window.as_ref())
            .map(|w| w.tokens_per_minute)
            .filter(|&tpm| tpm > 0.0),
        provenance: limits
            .iter()
            .find(|l| l.kind == LimitKind::Session)
            .map(|l| l.source),
        overlay_ms,
        collected_at_ms: snapshot.map(|s| s.collected_at.as_millisecond()),
    }
}

/// Classify collector liveness from its heartbeat age (ms) against the local poll cadence, returning
/// the banner text when the writer looks down — else `None` (live). `None` age = the heartbeat row is
/// absent (never started against this store). Pure; the age is read at the store-read site.
pub fn collector_alert(heartbeat_age_ms: Option<i64>, poll_local_secs: u64) -> Option<String> {
    match heartbeat_age_ms {
        None => Some("collector not running — data frozen (start `tok collector`)".to_string()),
        Some(age_ms) => {
            let down_ms = i64::try_from(poll_local_secs.saturating_mul(1000))
                .unwrap_or(i64::MAX)
                .saturating_mul(COLLECTOR_DOWN_FACTOR);
            (age_ms > down_ms)
                .then(|| format!("collector stalled — last beat {}", format_ago_ms(age_ms)))
        }
    }
}

/// Degradation rank for the fleet reduction — higher is more degraded, so `max_by_key` picks the
/// worst provenance present (a single derived/estimate account is never hidden behind authoritative).
fn provenance_rank(source: Provenance) -> u8 {
    match source {
        Provenance::Authoritative => 0,
        Provenance::Derived => 1,
        Provenance::Estimate => 2,
    }
}

/// Reduce the per-account usage facts into the one fleet-wide line, or `None` when no account has any
/// data yet (so the header simply omits the line rather than showing a bare `"—"`). See [`FleetView`]
/// for why usage is a representative (max), not a sum.
pub fn build_fleet_view(
    usages: &[AccountUsage],
    now: Timestamp,
    use_color: bool,
    poll_local_secs: u64,
    ledger_provenance: LedgerProvenance,
    ledger_matched: usize,
) -> Option<FleetView> {
    let total_tokens = usages.iter().filter_map(|u| u.total_tokens).max();
    let cost = usages
        .iter()
        .filter_map(|u| u.cost_notional)
        .max_by(f64::total_cmp);
    let tpm = usages
        .iter()
        .filter_map(|u| u.tokens_per_minute)
        .max_by(f64::total_cmp);
    let worst_prov = usages
        .iter()
        .filter_map(|u| u.provenance)
        .max_by_key(|p| provenance_rank(*p));
    let oldest_overlay = usages.iter().filter_map(|u| u.overlay_ms).min();
    // The local plane's freshness comes from the NEWEST snapshot across accounts (the most recent
    // collect); its age is shown even with the overlay off — the default deployment (spec 011 §B).
    let newest_local = usages.iter().filter_map(|u| u.collected_at_ms).max();

    if total_tokens.is_none()
        && cost.is_none()
        && tpm.is_none()
        && worst_prov.is_none()
        && oldest_overlay.is_none()
        && newest_local.is_none()
    {
        return None;
    }

    let stale_ms = i64::try_from(poll_local_secs.saturating_mul(1000))
        .unwrap_or(i64::MAX)
        .saturating_mul(LOCAL_STALE_FACTOR);
    let usage_stale = newest_local.is_some_and(|ms| now.as_millisecond() - ms > stale_ms);

    Some(FleetView {
        tokens: total_tokens.map_or_else(|| "—".to_string(), format_tokens),
        cost_notional: cost.map_or_else(|| "—".to_string(), format_cost),
        cost_short: cost.map_or_else(|| "—".to_string(), |c| format!("{}n", format_dollars(c))),
        burn_rate: tpm.map(|t| format!("{}/h", format_tokens(tokens_per_hour(t)))),
        provenance: worst_prov.map(|source| Badge {
            text: provenance_label(source).to_string(),
            short: provenance_short(source).to_string(),
            color: resolve_color(use_color, provenance_color(source)),
        }),
        usage_age: newest_local.map(|ms| format!("usage {}", format_ago(ms, now))),
        usage_stale,
        overlay_age: oldest_overlay.map(|ms| format!("limits {}", format_ago(ms, now))),
        ledger_note: ledger_fleet_note(ledger_provenance, ledger_matched),
    })
}

/// A minimal row shown when an account's store read fails — so one bad read never blanks or crashes
/// the whole dashboard (the loop keeps the other accounts and retries next tick).
pub fn error_view(account: &Account, use_color: bool, message: &str) -> AccountView {
    AccountView {
        title: account_title(account),
        accent: account_accent(account, use_color),
        session: None,
        weekly: None,
        weekly_hint: "n/a".to_string(),
        weekly_scoped: None,
        headline: None,
        status: Some(format!("store read error: {message}")),
        severity: Severity::Ok,
        inactive: !account.active,
        sub: SubView::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::Window;
    use crate::ledger::SubStatus;
    use std::path::PathBuf;

    fn view(severity: Severity) -> AccountView {
        AccountView {
            title: "T".to_string(),
            accent: Color::Cyan,
            session: None,
            weekly: None,
            weekly_hint: "n/a (enable overlay)".to_string(),
            weekly_scoped: None,
            headline: None,
            status: None,
            severity,
            inactive: false,
            sub: SubView::default(),
        }
    }

    #[test]
    fn selection_clamps_to_row_count() {
        let mut app = App::new(3, true);
        app.update(Msg::Data(Box::new(
            vec![view(Severity::Ok), view(Severity::Ok)].into(),
        )));
        app.update(Msg::Key(Action::Down));
        app.update(Msg::Key(Action::Down)); // would be index 2, clamps to 1
        assert_eq!(app.selected, 1);
        app.update(Msg::Key(Action::Up));
        assert_eq!(app.selected, 0);
        app.update(Msg::Key(Action::Up)); // saturating at 0
        assert_eq!(app.selected, 0);
    }

    #[test]
    fn data_with_fewer_rows_reclamps_selection() {
        let mut app = App::new(3, true);
        app.update(Msg::Data(Box::new(vec![view(Severity::Ok); 3].into())));
        app.update(Msg::Key(Action::Down));
        app.update(Msg::Key(Action::Down));
        assert_eq!(app.selected, 2);
        app.update(Msg::Data(Box::new(vec![view(Severity::Ok)].into()))); // shrinks to 1
        assert_eq!(app.selected, 0);
    }

    #[test]
    fn empty_data_reclamps_selection_and_renders_the_empty_state() {
        // Finding 7: an all-inactive / toggle-off transition delivers an EMPTY Msg::Data. Selection
        // (driven non-zero first) must reclamp to 0, further Up/Down must not panic on empty rows,
        // and the render must fall back to the empty-state placeholder rather than a tier layout.
        use crate::tui::view::render;
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let mut app = App::new(3, true);
        app.update(Msg::Data(Box::new(vec![view(Severity::Ok); 3].into())));
        app.update(Msg::Key(Action::Down));
        app.update(Msg::Key(Action::Down));
        assert_eq!(app.selected, 2);

        // Everything drops out (e.g. all accounts inactive with show_inactive off).
        app.update(Msg::Data(Box::new(Vec::<AccountView>::new().into())));
        assert_eq!(app.selected, 0, "selection reclamps to 0 on empty data");

        // Further navigation on an empty board must be a no-op, never a panic/underflow.
        app.update(Msg::Key(Action::Down));
        app.update(Msg::Key(Action::Up));
        assert_eq!(app.selected, 0);

        // The render falls back to the empty-state placeholder rather than a tier layout.
        let mut term = Terminal::new(TestBackend::new(80, 20)).expect("backend");
        term.draw(|f| render(f, &app)).expect("draw");
        let buf = term.backend().buffer();
        let mut text = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                text.push_str(buf[(x, y)].symbol());
            }
        }
        assert!(
            text.contains("collecting"),
            "empty board must show the collecting… placeholder, got:\n{text}"
        );
    }

    #[test]
    fn quit_help_and_refresh_flags() {
        let mut app = App::new(1, true);
        app.update(Msg::Key(Action::Help));
        assert!(app.show_help);
        app.update(Msg::Key(Action::Refresh));
        assert!(app.reload_requested);
        app.update(Msg::Key(Action::Quit));
        assert!(app.should_quit);
    }

    #[test]
    fn toggle_inactive_flips_flag_and_requests_reload() {
        let mut app = App::new(1, true);
        assert!(!app.show_inactive, "hidden by default (spec 014 §C)");
        app.update(Msg::Key(Action::ToggleInactive));
        assert!(app.show_inactive);
        assert!(app.reload_requested, "toggling must re-read the store");
        app.reload_requested = false;
        app.update(Msg::Key(Action::ToggleInactive));
        assert!(!app.show_inactive, "pressing i again hides it");
        assert!(app.reload_requested);
    }

    #[test]
    fn alert_count_counts_warn_and_crit() {
        let mut app = App::new(3, true);
        app.update(Msg::Data(Box::new(
            vec![
                view(Severity::Ok),
                view(Severity::Warn),
                view(Severity::Crit),
            ]
            .into(),
        )));
        assert_eq!(app.alert_count(), 2);
    }

    #[test]
    fn alert_count_ignores_inactive_rows_even_at_crit() {
        // A crit-level stored limit on an inactive (peeked-at) account must never raise the banner
        // or the warn count (spec 014 §C, acceptance criteria 3–4).
        let mut app = App::new(2, true);
        let mut inactive_crit = view(Severity::Crit);
        inactive_crit.inactive = true;
        app.update(Msg::Data(Box::new(
            vec![view(Severity::Warn), inactive_crit].into(),
        )));
        assert_eq!(app.alert_count(), 1, "only the active warn row counts");
    }

    #[test]
    fn account_count_tracks_visible_rows_across_data() {
        // account_count starts at the caller's initial (active-only) estimate, then tracks
        // `rows.len()` as data lands — including a shrink/grow across a show_inactive toggle.
        let mut app = App::new(1, true);
        assert_eq!(app.account_count, 1);
        app.update(Msg::Data(Box::new(
            vec![view(Severity::Ok), view(Severity::Ok)].into(),
        )));
        assert_eq!(app.account_count, 2);
    }

    #[test]
    fn severity_maps_to_stable_colors() {
        assert_eq!(severity_color(Severity::Ok), Color::Green);
        assert_eq!(severity_color(Severity::Warn), Color::Yellow);
        assert_eq!(severity_color(Severity::Crit), Color::Red);
    }

    #[test]
    fn no_color_resolves_to_reset() {
        assert_eq!(resolve_color(true, Color::Red), Color::Red);
        assert_eq!(resolve_color(false, Color::Red), Color::Reset);
    }

    #[test]
    fn parses_named_and_hex_colors() {
        assert_eq!(parse_named_color("cyan"), Some(Color::Cyan));
        assert_eq!(parse_named_color("  LightBlue "), Some(Color::LightBlue));
        assert_eq!(parse_named_color("#00ffcc"), Some(Color::Rgb(0, 255, 204)));
        assert_eq!(parse_named_color("burple"), None);
    }

    fn account() -> Account {
        Account {
            id: "personal".to_string(),
            label: "Personal".to_string(),
            provider: Provider::Claude,
            config_dir: Some(PathBuf::from("/tmp")),
            api_key_env: None,
            color: Some("cyan".to_string()),
            active: true,
            limits_overlay: false,
        }
    }

    #[test]
    fn build_view_from_snapshot_and_limit() {
        let snapshot = UsageSnapshot {
            account_id: "personal".to_string(),
            provider: Provider::Claude,
            collected_at: "2026-07-04T10:00:00Z".parse().unwrap(),
            input: 1,
            output: 1,
            cache_read: 1,
            cache_creation: 1,
            total_tokens: 1_234_567,
            cost_notional: Some(1.7),
            window: None,
        };
        let limit = Limit {
            account_id: "personal".to_string(),
            provider: Provider::Claude,
            kind: LimitKind::Session,
            scope: None,
            utilization_pct: 76.0,
            resets_at: "2026-07-04T12:00:00Z".to_string(),
            severity: Severity::Warn,
            source: Provenance::Derived,
        };
        let now: Timestamp = "2026-07-04T10:49:00Z".parse().unwrap();
        let limits = [limit];
        let data = AccountData {
            snapshot: Some(&snapshot),
            limits: &limits,
            token_status: None,
            overlay_failing_since: None,
            overlay_ms: None,
            ledger_rows: &[],
            today: jiff::civil::Date::default(),
        };
        let row = build_account_view(&account(), data, now, true);

        assert_eq!(row.title, "Personal [claude]");
        let session = row.session.expect("session gauge");
        assert!((session.ratio - 0.76).abs() < 1e-9);
        // The composed 5h label carries its own reset countdown.
        assert_eq!(session.label(), "76% warn · resets in 1h 11m");
        // The tight MICRO tier strips the leading "in " (but never reformats the time value).
        assert_eq!(session.reset_short(), Some("1h 11m"));
        // With only a session limit, the headline is that session.
        assert_eq!(row.headline.expect("headline").label(), session.label());
        assert_eq!(row.status.as_deref(), Some("idle (no active block)"));
        assert_eq!(row.severity, Severity::Warn);
        // The shared token/cost/provenance figures now come off `account_usage` (fleet line), not the
        // per-account row. No active window ⇒ no burn rate.
        let usage = account_usage(Some(&snapshot), &limits, None);
        assert_eq!(usage.total_tokens, Some(1_234_567));
        assert_eq!(usage.cost_notional, Some(1.7));
        assert_eq!(usage.provenance, Some(Provenance::Derived));
        assert_eq!(usage.tokens_per_minute, None);
    }

    // ── spec 019 §D (AC5): a zai account renders session % with no countdown, weekly % with a
    // countdown, and no usage/cost row (it has no local usage lane this wave). The rendering path
    // itself is provider-agnostic (session/weekly gauges keyed off `LimitKind`, the usage row keyed
    // off `Option<UsageSnapshot>`) — this pins that the existing generic path already covers a new
    // provider correctly, with zero zai-specific TUI code.

    #[test]
    fn zai_account_renders_session_without_countdown_and_weekly_with_countdown_and_no_usage_row() {
        let zai_account = Account {
            id: "zai-lite".to_string(),
            label: "z.ai GLM Lite".to_string(),
            provider: Provider::Zai,
            config_dir: None,
            api_key_env: Some("Z_AI_CODING_KEY".to_string()),
            color: None,
            active: true,
            limits_overlay: true,
        };
        let session = Limit {
            account_id: "zai-lite".to_string(),
            provider: Provider::Zai,
            kind: LimitKind::Session,
            scope: None,
            utilization_pct: 42.0,
            resets_at: String::new(), // the rolling 5h window has no reset instant (spec 019 §B)
            severity: Severity::Ok,
            source: Provenance::Authoritative,
        };
        let weekly = Limit {
            account_id: "zai-lite".to_string(),
            provider: Provider::Zai,
            kind: LimitKind::WeeklyAll,
            scope: None,
            utilization_pct: 81.0,
            resets_at: "2026-07-11T00:00:00Z".to_string(),
            severity: Severity::Warn,
            source: Provenance::Authoritative,
        };
        let now: Timestamp = "2026-07-04T10:00:00Z".parse().unwrap();
        let limits = [session, weekly];
        // Spec 019 §D: the spec-017 ledger clause + spec-018 verified pill are provider-agnostic —
        // a matching zai-lite ledger row must render its clause and pill exactly like any account.
        let mut sub = ledger_sub(SubStatus::Active);
        sub.id = "zai-lite".to_string();
        sub.purchased = Some(jiff::civil::date(2026, 7, 1));
        sub.renews = Some(jiff::civil::date(2026, 8, 1));
        sub.verified = Some(jiff::civil::date(2026, 7, 4));
        let data = AccountData {
            snapshot: None, // always idle this wave (spec 019 §C) — no usage lane
            limits: &limits,
            token_status: None,
            overlay_failing_since: None,
            overlay_ms: None,
            ledger_rows: std::slice::from_ref(&sub),
            today: jiff::civil::date(2026, 7, 4),
        };
        let row = build_account_view(&zai_account, data, now, true);

        assert_eq!(row.title, "z.ai GLM Lite [zai]");
        assert!(
            row.sub
                .full
                .as_deref()
                .is_some_and(|s| s.contains("renews")),
            "the ledger clause renders unchanged for a zai id: {:?}",
            row.sub.full
        );
        assert!(
            row.sub.full.as_deref().is_some_and(|s| s.contains('✓')),
            "the verified pill renders unchanged for a zai id: {:?}",
            row.sub.full
        );
        let session_gauge = row.session.expect("session gauge");
        assert!((session_gauge.ratio - 0.42).abs() < 1e-9);
        assert_eq!(
            session_gauge.reset, None,
            "an empty resets_at must render as no countdown, never a fabricated one"
        );
        assert_eq!(session_gauge.label(), "42% ok", "no ' · resets' tail");

        let weekly_gauge = row.weekly.expect("weekly gauge");
        assert!((weekly_gauge.ratio - 0.81).abs() < 1e-9);
        assert!(
            weekly_gauge.reset.is_some(),
            "the weekly gauge has a real resets_at — it must carry a countdown"
        );

        let usage = account_usage(None, &limits, None);
        assert_eq!(
            usage.total_tokens, None,
            "no usage row — always idle this wave"
        );
        assert_eq!(usage.cost_notional, None, "never a fabricated cost");
    }

    // ── spec 020 §D (AC5): a gemini account renders its tokens (no cost line) with BOTH gauges
    // honestly `n/a` — no derived anything, no invented limit. Same provider-agnostic rendering
    // path as the zai test above proves the generic path already covers this shape correctly.

    #[test]
    fn gemini_account_renders_tokens_with_both_gauges_na_and_no_cost_line() {
        let gemini_account = Account {
            id: "gemini-personal".to_string(),
            label: "Gemini Personal".to_string(),
            provider: Provider::Gemini,
            config_dir: Some(PathBuf::from("/home/example/.gemini")),
            api_key_env: None,
            color: None,
            active: true,
            limits_overlay: false, // spec 020 §A: no limits surface exists to opt into
        };
        let snapshot = UsageSnapshot {
            account_id: "gemini-personal".to_string(),
            provider: Provider::Gemini,
            collected_at: "2026-07-19T10:00:00Z".parse().unwrap(),
            input: 2_400,
            output: 150,
            cache_read: 600,
            cache_creation: 0,
            total_tokens: 3_550,
            cost_notional: None, // no public subscription pricing basis (spec 020 §B)
            window: None,        // the 5h lookback is a scan bound, not a window claim
        };
        let now: Timestamp = "2026-07-19T10:49:00Z".parse().unwrap();
        // Spec 020 §D: the spec-017 ledger clause + spec-018 verified pill are provider-agnostic —
        // a matching gemini-personal ledger row must render its clause and pill exactly like any
        // account, even though Gemini has no ledger row until the operator actually buys AI Pro/Ultra.
        let mut sub = ledger_sub(SubStatus::Active);
        sub.id = "gemini-personal".to_string();
        sub.purchased = Some(jiff::civil::date(2026, 7, 1));
        sub.renews = Some(jiff::civil::date(2026, 8, 1));
        sub.verified = Some(jiff::civil::date(2026, 7, 19));
        let data = AccountData {
            snapshot: Some(&snapshot),
            limits: &[], // no limits code exists for Gemini (spec 020 §C) — never invented here
            token_status: None,
            overlay_failing_since: None,
            overlay_ms: None,
            ledger_rows: std::slice::from_ref(&sub),
            today: jiff::civil::date(2026, 7, 19),
        };
        let row = build_account_view(&gemini_account, data, now, true);

        assert_eq!(row.title, "Gemini Personal [gemini]");
        assert!(
            row.sub
                .full
                .as_deref()
                .is_some_and(|s| s.contains("renews")),
            "the ledger clause renders unchanged for a gemini id: {:?}",
            row.sub.full
        );
        assert!(
            row.sub.full.as_deref().is_some_and(|s| s.contains('✓')),
            "the verified pill renders unchanged for a gemini id: {:?}",
            row.sub.full
        );
        assert!(
            row.session.is_none(),
            "no session limit exists for Gemini — the gauge must be honestly absent, never derived"
        );
        assert!(
            row.weekly.is_none(),
            "no weekly limit exists for Gemini — the gauge must be honestly absent"
        );
        assert_eq!(
            row.weekly_hint, "n/a (no limits surface)",
            "gemini has no limits surface at all — never nudge enabling a flag that does nothing"
        );

        let usage = account_usage(Some(&snapshot), &[], None);
        assert_eq!(
            usage.total_tokens,
            Some(3_550),
            "tokens must show — the usage lane is real"
        );
        assert_eq!(usage.cost_notional, None, "never a fabricated cost");
        assert_eq!(
            usage.provenance, None,
            "no session limit ⇒ no provenance badge for this account"
        );
    }

    // A gemini account that DOES set `limits_overlay = true` must still show the honest
    // no-limits-surface hint, never "waiting for overlay" forever (nothing will ever arrive —
    // the flag is accepted but ignored, spec 020 §A/§C).
    #[test]
    fn a_gemini_account_opted_into_the_ignored_overlay_still_shows_no_limits_surface() {
        let gemini_account = Account {
            id: "gemini-personal".to_string(),
            label: "Gemini Personal".to_string(),
            provider: Provider::Gemini,
            config_dir: Some(PathBuf::from("/home/example/.gemini")),
            api_key_env: None,
            color: None,
            active: true,
            limits_overlay: true, // opted in even though gemini has nothing to opt into
        };
        let now: Timestamp = "2026-07-19T10:49:00Z".parse().unwrap();
        let data = AccountData {
            snapshot: None,
            limits: &[],
            token_status: None,
            overlay_failing_since: None,
            overlay_ms: None,
            ledger_rows: &[],
            today: jiff::civil::date(2026, 7, 19),
        };
        let row = build_account_view(&gemini_account, data, now, true);
        assert_eq!(
            row.weekly_hint, "n/a (no limits surface)",
            "the ignored flag must never produce a 'waiting for overlay' that waits forever"
        );
    }

    // ── spec 022 §D (AC5): grok's weekly gauge renders from a stored row; the hint is grok-honest.

    fn grok_account() -> Account {
        Account {
            id: "grok-main".to_string(),
            label: "Grok".to_string(),
            provider: Provider::Grok,
            config_dir: Some(PathBuf::from("/home/example/.grok")),
            api_key_env: None,
            color: None,
            active: true,
            limits_overlay: false,
        }
    }

    #[test]
    fn a_grok_account_without_a_weekly_row_hints_at_the_billing_log_not_the_overlay() {
        let now: Timestamp = "2026-07-20T10:00:00Z".parse().unwrap();
        let data = AccountData {
            snapshot: None,
            limits: &[],
            token_status: None,
            overlay_failing_since: None,
            overlay_ms: None,
            ledger_rows: &[],
            today: jiff::civil::date(2026, 7, 20),
        };
        let row = build_account_view(&grok_account(), data, now, true);
        assert!(row.weekly.is_none());
        assert_eq!(
            row.weekly_hint, "n/a (awaiting grok billing log)",
            "grok's missing weekly must name its local source, never the (ignored) overlay"
        );
    }

    #[test]
    fn a_grok_weekly_quota_row_renders_the_gauge_through_the_shared_machinery() {
        let now: Timestamp = "2026-07-20T10:00:00Z".parse().unwrap();
        let mut weekly = authoritative(LimitKind::WeeklyAll, None, 12.5, Severity::Ok);
        weekly.account_id = "grok-main".to_string();
        weekly.provider = Provider::Grok;
        weekly.resets_at = "2026-07-26T00:00:00+00:00".to_string();
        let limits = vec![weekly];
        let data = AccountData {
            snapshot: None,
            limits: &limits,
            token_status: None,
            overlay_failing_since: None,
            overlay_ms: None,
            ledger_rows: &[],
            today: jiff::civil::date(2026, 7, 20),
        };
        let row = build_account_view(&grok_account(), data, now, true);
        let gauge = row
            .weekly
            .expect("a stored WeeklyAll row must render the gauge");
        assert!(
            (gauge.ratio - 0.125).abs() < 1e-9,
            "the gauge must carry the quota percent verbatim: {gauge:?}"
        );
    }

    // ── spec 020 §D (AC4): a gemini account's `cost_notional = None` must never poison the fleet
    // cost line when another account in the fleet DOES have a real notional cost.
    #[test]
    fn fleet_cost_line_is_not_poisoned_by_a_gemini_accounts_none_cost() {
        let now: Timestamp = "2026-07-19T12:00:00Z".parse().unwrap();
        let gemini_usage = AccountUsage {
            total_tokens: Some(3_550),
            cost_notional: None, // spec 020 §B: no public subscription pricing basis
            tokens_per_minute: None,
            provenance: None,
            overlay_ms: None,
            collected_at_ms: Some(now.as_millisecond()),
        };
        let claude_usage = AccountUsage {
            total_tokens: Some(1_000_000),
            cost_notional: Some(12.34),
            tokens_per_minute: None,
            provenance: Some(Provenance::Authoritative),
            overlay_ms: None,
            collected_at_ms: Some(now.as_millisecond()),
        };
        let fleet = build_fleet_view(
            &[gemini_usage, claude_usage],
            now,
            true,
            20,
            LedgerProvenance::Off,
            0,
        )
        .expect("fleet");
        assert_eq!(
            fleet.cost_notional, "$12.34 (notional)",
            "the claude account's real cost must survive a mixed-in None, never dropped to \"—\""
        );
        assert_eq!(
            fleet.tokens, "1.00M",
            "the representative-usage reduction is unaffected by the None cost"
        );
    }

    #[test]
    fn inactive_account_view_is_tagged_and_dimmed() {
        // Spec 014 §C: when shown (via `i`), an inactive account's title carries an "(inactive)" tag
        // and every gauge/accent colour is flattened to dim grey — but its numbers stay accurate.
        let inactive = Account {
            active: false,
            ..account()
        };
        let limit = Limit {
            account_id: "personal".to_string(),
            provider: Provider::Claude,
            kind: LimitKind::Session,
            scope: None,
            utilization_pct: 91.0,
            resets_at: "2026-07-04T12:00:00Z".to_string(),
            severity: Severity::Crit,
            source: Provenance::Derived,
        };
        let now: Timestamp = "2026-07-04T10:49:00Z".parse().unwrap();
        let limits = [limit];
        let row = build_account_view(&inactive, data_from(&limits), now, true);

        assert_eq!(row.title, "Personal [claude] (inactive)");
        assert!(row.inactive);
        assert_eq!(row.accent, Color::DarkGray);
        let session = row.session.expect("session gauge");
        // The percent/severity data is untouched (still an accurate peek)...
        assert_eq!(session.pct, "91%");
        assert_eq!(session.severity, Severity::Crit);
        // ...but the colour is flattened to dim grey rather than the crit-red it would otherwise be.
        assert_eq!(session.color, Color::DarkGray);
        assert_eq!(row.headline.expect("headline").color, Color::DarkGray);

        // NO_COLOR: the tag stays (it's structural, in the title text) even though colour collapses.
        let mono = build_account_view(&inactive, data_from(&limits), now, false);
        assert_eq!(mono.title, "Personal [claude] (inactive)");
        assert_eq!(mono.accent, Color::Reset);
    }

    #[test]
    fn fleet_view_formats_tokens_per_hour_from_window_burn_rate() {
        // 4520.0 tok/min × 60 = 271_200 tok/hour ⇒ "271.2K/h" (same basis/formatter as `tokens`).
        let snapshot = UsageSnapshot {
            account_id: "personal".to_string(),
            provider: Provider::Claude,
            collected_at: "2026-07-04T10:00:00Z".parse().unwrap(),
            input: 1,
            output: 1,
            cache_read: 1,
            cache_creation: 1,
            total_tokens: 5_000_000,
            cost_notional: Some(1.7),
            window: Some(Window {
                start: "2026-07-04T07:00:00Z".parse().unwrap(),
                end: "2026-07-04T12:00:00Z".parse().unwrap(),
                remaining_minutes: Some(90),
                tokens_per_minute: 4520.0,
                cost_per_hour: 6.7,
            }),
        };
        let now: Timestamp = "2026-07-04T10:49:00Z".parse().unwrap();
        let usage = account_usage(Some(&snapshot), &[], None);
        assert_eq!(usage.tokens_per_minute, Some(4520.0));
        let fleet =
            build_fleet_view(&[usage], now, true, 20, LedgerProvenance::Off, 0).expect("fleet");
        assert_eq!(fleet.burn_rate.as_deref(), Some("271.2K/h"));
    }

    #[test]
    fn fleet_view_shows_local_usage_age_even_with_overlay_off() {
        // Default deployment: overlay off (overlay_ms None), but a fresh snapshot ⇒ a local age still
        // shows, and past 2× the cadence it is flagged stale (so it can be styled as a warning).
        let now: Timestamp = "2026-07-04T12:00:00Z".parse().unwrap();
        let fresh: Timestamp = "2026-07-04T11:59:48Z".parse().unwrap(); // 12s ago
        let usage = account_usage(
            Some(&UsageSnapshot {
                account_id: "a".to_string(),
                provider: Provider::Claude,
                collected_at: fresh,
                input: 1,
                output: 1,
                cache_read: 1,
                cache_creation: 1,
                total_tokens: 4,
                cost_notional: Some(0.1),
                window: None,
            }),
            &[],
            None,
        );
        let fleet =
            build_fleet_view(&[usage], now, true, 20, LedgerProvenance::Off, 0).expect("fleet");
        assert_eq!(fleet.usage_age.as_deref(), Some("usage 12s ago"));
        assert!(!fleet.usage_stale, "12s < 2×20s is fresh");
        assert_eq!(fleet.overlay_age, None, "overlay off ⇒ no limits age");

        // A snapshot 90s old (> 2×20s) is stale.
        let stale: Timestamp = "2026-07-04T11:58:30Z".parse().unwrap();
        let mut u = usage;
        u.collected_at_ms = Some(stale.as_millisecond());
        let fleet = build_fleet_view(&[u], now, true, 20, LedgerProvenance::Off, 0).expect("fleet");
        assert!(fleet.usage_stale, "90s > 2×20s is stale");
    }

    #[test]
    fn collector_alert_flags_down_and_stalled_but_not_live() {
        // Never started (no heartbeat row) ⇒ loud "not running" banner.
        assert_eq!(
            collector_alert(None, 10).as_deref(),
            Some("collector not running — data frozen (start `tok collector`)")
        );
        // Fresh beat (5s < 3×10s) ⇒ no alert.
        assert!(collector_alert(Some(5_000), 10).is_none());
        // Stalled beat (90s > 3×10s) ⇒ "stalled — last beat …".
        assert_eq!(
            collector_alert(Some(90_000), 10).as_deref(),
            Some("collector stalled — last beat 1m ago")
        );
        // Exact strict-greater-than boundary: down_ms = 3 × 10s = 30_000ms. Equal is NOT stalled
        // (the `>` is strict, so the boundary tick itself stays live)...
        assert!(collector_alert(Some(30_000), 10).is_none());
        // ...and one ms past it trips the stalled banner.
        assert!(collector_alert(Some(30_001), 10).is_some());
    }

    #[test]
    fn tokens_per_hour_clamps_non_finite_and_negative() {
        assert_eq!(tokens_per_hour(1000.0), 60_000);
        assert_eq!(tokens_per_hour(0.0), 0);
        assert_eq!(tokens_per_hour(-5.0), 0);
        assert_eq!(tokens_per_hour(f64::NAN), 0);
        assert_eq!(tokens_per_hour(f64::INFINITY), 0);
    }

    /// Minimal `AccountData` (no snapshot/token) for limit-focused build tests.
    fn data_from(limits: &[Limit]) -> AccountData<'_> {
        AccountData {
            snapshot: None,
            limits,
            token_status: None,
            overlay_failing_since: None,
            overlay_ms: None,
            ledger_rows: &[],
            today: jiff::civil::Date::default(),
        }
    }

    fn authoritative(kind: LimitKind, scope: Option<&str>, pct: f64, sev: Severity) -> Limit {
        Limit {
            account_id: "personal".to_string(),
            provider: Provider::Claude,
            kind,
            scope: scope.map(str::to_string),
            utilization_pct: pct,
            resets_at: "2026-07-10T03:00:00.44+00:00".to_string(),
            severity: sev,
            source: Provenance::Authoritative,
        }
    }

    #[test]
    fn build_view_surfaces_weekly_and_scoped_gauges_and_worst_severity() {
        let now: Timestamp = "2026-07-04T10:00:00Z".parse().unwrap();
        let limits = vec![
            authoritative(LimitKind::Session, None, 29.0, Severity::Ok),
            authoritative(LimitKind::WeeklyAll, None, 78.0, Severity::Warn),
            authoritative(LimitKind::WeeklyScoped, Some("Fable"), 92.0, Severity::Crit),
        ];
        let row = build_account_view(&account(), data_from(&limits), now, true);

        // Every gauge carries its own reset — session, weekly-all, and the scoped weekly alike.
        assert!(row
            .session
            .expect("session")
            .label()
            .starts_with("29% ok · resets "));
        assert!(row
            .weekly
            .expect("weekly-all")
            .label()
            .starts_with("78% warn · resets "));
        let scoped = row.weekly_scoped.expect("scoped weekly");
        assert!(scoped.label().starts_with("Fable 92% crit · resets "));
        assert!((scoped.ratio - 0.92).abs() < 1e-9);
        // The headline is the scariest gauge — the 92% crit scoped weekly, not the calm 29% session.
        assert!(row
            .headline
            .expect("headline")
            .label()
            .starts_with("Fable 92% crit"));
        // Worst-of-all: the critical scoped weekly wins even though the 5h session is Ok.
        assert_eq!(row.severity, Severity::Crit);
    }

    #[test]
    fn expired_limit_renders_waiting_for_reset_and_stops_alarming() {
        // A crit weekly whose reset passed a day ago: the window reset, so the stored 100% is
        // history — the gauge goes dormant and the row leaves the banner (spec 012 §A).
        let now: Timestamp = "2026-07-08T10:00:00Z".parse().unwrap();
        let mut past = authoritative(LimitKind::WeeklyAll, None, 100.0, Severity::Crit);
        past.resets_at = "2026-07-07T09:00:00Z".to_string();
        let limits = vec![past];
        let row = build_account_view(&account(), data_from(&limits), now, true);
        let weekly = row.weekly.expect("weekly");
        assert!(weekly.expired);
        assert_eq!(weekly.label(), "waiting for reset");
        assert_eq!(weekly.pct, "—");
        assert!(weekly.ratio.abs() < 1e-9, "dormant bar");
        assert_eq!(weekly.severity, Severity::Ok);
        assert_eq!(row.severity, Severity::Ok, "expired crit must not alarm");
    }

    #[test]
    fn non_expired_severity_still_wins_and_scoped_expired_keeps_its_scope() {
        let now: Timestamp = "2026-07-08T10:00:00Z".parse().unwrap();
        let mut expired = authoritative(LimitKind::WeeklyAll, None, 100.0, Severity::Crit);
        expired.resets_at = "2026-07-07T09:00:00Z".to_string();
        // The session fixture resets 2026-07-10 — still live, so its warn drives the row.
        let live = authoritative(LimitKind::Session, None, 80.0, Severity::Warn);
        let row = build_account_view(&account(), data_from(&[live, expired]), now, true);
        assert_eq!(row.severity, Severity::Warn);

        let mut scoped =
            authoritative(LimitKind::WeeklyScoped, Some("Fable"), 99.0, Severity::Crit);
        scoped.resets_at = "2026-07-07T09:00:00Z".to_string();
        let row = build_account_view(&account(), data_from(&[scoped]), now, true);
        assert_eq!(
            row.weekly_scoped.expect("scoped").label(),
            "Fable waiting for reset"
        );
    }

    #[test]
    fn build_view_picks_the_most_utilized_scoped_weekly() {
        let now: Timestamp = "2026-07-04T10:00:00Z".parse().unwrap();
        let limits = vec![
            authoritative(LimitKind::WeeklyScoped, Some("Sonnet"), 40.0, Severity::Ok),
            authoritative(LimitKind::WeeklyScoped, Some("Fable"), 92.0, Severity::Crit),
        ];
        let row = build_account_view(&account(), data_from(&limits), now, true);
        assert!(row
            .weekly_scoped
            .expect("scoped")
            .label()
            .starts_with("Fable 92% crit · resets "));
    }

    #[test]
    fn sustained_overlay_failure_flags_check_account_but_a_recent_one_still_waits() {
        let now: Timestamp = "2026-07-04T12:00:00Z".parse().unwrap();
        let opted_in = Account {
            active: true,
            limits_overlay: true,
            ..account()
        };
        let mk = |failing_since: Option<Timestamp>| AccountData {
            snapshot: None,
            limits: &[],
            token_status: None,
            overlay_failing_since: failing_since.map(jiff::Timestamp::as_millisecond),
            overlay_ms: None,
            ledger_rows: &[],
            today: jiff::civil::Date::default(),
        };

        // Failing for 20m (past the 15m stall threshold) ⇒ the honest "check account" flag.
        let stalled: Timestamp = "2026-07-04T11:40:00Z".parse().unwrap();
        let row = build_account_view(&opted_in, mk(Some(stalled)), now, true);
        assert_eq!(row.weekly_hint, "n/a (overlay stalled — check account)");

        // A 5m-old failure is within grace (a live account's transient 429) ⇒ still just waiting.
        let recent: Timestamp = "2026-07-04T11:55:00Z".parse().unwrap();
        let row = build_account_view(&opted_in, mk(Some(recent)), now, true);
        assert_eq!(row.weekly_hint, "n/a (waiting for overlay)");
    }

    #[test]
    fn overlay_silent_after_past_success_shows_aged_hint() {
        // The overlay succeeded once, long enough ago to cross the stall threshold, but no failed
        // attempt has been recorded since (e.g. the collector never got around to retrying it) —
        // the honest hint ages the past success instead of pretending it is freshly pending.
        let now: Timestamp = "2026-07-04T12:00:00Z".parse().unwrap();
        let opted_in = Account {
            active: true,
            limits_overlay: true,
            ..account()
        };
        let last_success: Timestamp = "2026-07-04T11:30:00Z".parse().unwrap(); // 30m ago > 15m stall
        let data = AccountData {
            snapshot: None,
            limits: &[],
            token_status: None,
            overlay_failing_since: None,
            overlay_ms: Some(last_success.as_millisecond()),
            ledger_rows: &[],
            today: jiff::civil::Date::default(),
        };
        let row = build_account_view(&opted_in, data, now, true);
        assert_eq!(row.weekly_hint, "n/a (overlay silent 30m ago)");
    }

    #[test]
    fn overlay_never_succeeded_keeps_waiting_hint() {
        let now: Timestamp = "2026-07-04T12:00:00Z".parse().unwrap();
        let opted_in = Account {
            active: true,
            limits_overlay: true,
            ..account()
        };
        let data = AccountData {
            snapshot: None,
            limits: &[],
            token_status: None,
            overlay_failing_since: None,
            overlay_ms: None,
            ledger_rows: &[],
            today: jiff::civil::Date::default(),
        };
        let row = build_account_view(&opted_in, data, now, true);
        assert_eq!(row.weekly_hint, "n/a (waiting for overlay)");
    }

    #[test]
    fn overlay_fresh_success_keeps_waiting_hint() {
        // A success inside the stall window is not stale enough to age — same honest "waiting" as
        // a never-succeeded account, since nothing is actually wrong yet.
        let now: Timestamp = "2026-07-04T12:00:00Z".parse().unwrap();
        let opted_in = Account {
            active: true,
            limits_overlay: true,
            ..account()
        };
        let recent_success: Timestamp = "2026-07-04T11:58:00Z".parse().unwrap(); // 2m ago < 15m
        let data = AccountData {
            snapshot: None,
            limits: &[],
            token_status: None,
            overlay_failing_since: None,
            overlay_ms: Some(recent_success.as_millisecond()),
            ledger_rows: &[],
            today: jiff::civil::Date::default(),
        };
        let row = build_account_view(&opted_in, data, now, true);
        assert_eq!(row.weekly_hint, "n/a (waiting for overlay)");
    }

    #[test]
    fn overlay_stalled_flag_wins_over_silent_hint() {
        // Both a tripped stall flag AND a stale past success are present — the stalled branch still
        // wins (a dead/blocked subscription is worse, more actionable news than "gone quiet").
        let now: Timestamp = "2026-07-04T12:00:00Z".parse().unwrap();
        let opted_in = Account {
            active: true,
            limits_overlay: true,
            ..account()
        };
        let stalled_since: Timestamp = "2026-07-04T11:40:00Z".parse().unwrap();
        let last_success: Timestamp = "2026-07-04T10:00:00Z".parse().unwrap();
        let data = AccountData {
            snapshot: None,
            limits: &[],
            token_status: None,
            overlay_failing_since: Some(stalled_since.as_millisecond()),
            overlay_ms: Some(last_success.as_millisecond()),
            ledger_rows: &[],
            today: jiff::civil::Date::default(),
        };
        let row = build_account_view(&opted_in, data, now, true);
        assert_eq!(row.weekly_hint, "n/a (overlay stalled — check account)");
    }

    #[test]
    fn fleet_view_reports_overlay_refresh_age_scoped_as_limits() {
        let now: Timestamp = "2026-07-04T12:00:00Z".parse().unwrap();
        let two_min_ago: Timestamp = "2026-07-04T11:58:00Z".parse().unwrap();
        let usage = account_usage(None, &[], Some(two_min_ago.as_millisecond()));
        let fleet =
            build_fleet_view(&[usage], now, true, 20, LedgerProvenance::Off, 0).expect("fleet");
        // The overlay age is scoped "limits …" so it can never imply the local numbers are fresh.
        assert_eq!(fleet.overlay_age.as_deref(), Some("limits 2m ago"));
        assert_eq!(fleet.usage_age, None, "no snapshot ⇒ no local age");

        // No recorded overlay success and no snapshot anywhere ⇒ no fleet line.
        assert!(build_fleet_view(
            &[account_usage(None, &[], None)],
            now,
            true,
            20,
            LedgerProvenance::Off,
            0
        )
        .is_none());
    }

    #[test]
    fn account_usage_extracts_shared_facts() {
        let snapshot = UsageSnapshot {
            account_id: "personal".to_string(),
            provider: Provider::Claude,
            collected_at: "2026-07-04T10:00:00Z".parse().unwrap(),
            input: 1,
            output: 1,
            cache_read: 1,
            cache_creation: 1,
            total_tokens: 280_000_000,
            cost_notional: Some(335.95),
            window: Some(Window {
                start: "2026-07-04T07:00:00Z".parse().unwrap(),
                end: "2026-07-04T12:00:00Z".parse().unwrap(),
                remaining_minutes: Some(90),
                tokens_per_minute: 4520.0,
                cost_per_hour: 6.7,
            }),
        };
        let limits = vec![authoritative(LimitKind::Session, None, 42.0, Severity::Ok)];
        let u = account_usage(Some(&snapshot), &limits, Some(1_720_000_000_000));
        assert_eq!(u.total_tokens, Some(280_000_000));
        assert_eq!(u.cost_notional, Some(335.95));
        assert_eq!(u.tokens_per_minute, Some(4520.0));
        assert_eq!(u.provenance, Some(Provenance::Authoritative));
        assert_eq!(u.overlay_ms, Some(1_720_000_000_000));
        assert_eq!(
            u.collected_at_ms,
            Some(snapshot.collected_at.as_millisecond())
        );

        // No snapshot / no session limit ⇒ all facts absent (the error-isolation path).
        let empty = account_usage(None, &[], None);
        assert_eq!(empty.total_tokens, None);
        assert_eq!(empty.tokens_per_minute, None);
        assert_eq!(empty.provenance, None);
        assert_eq!(empty.collected_at_ms, None);
    }

    #[test]
    fn fleet_view_reduces_representative_usage_worst_provenance_oldest_refresh() {
        let now: Timestamp = "2026-07-04T12:00:00Z".parse().unwrap();
        // Two accounts reading the same shared logs (identical usage), collected a tick apart, and
        // one whose overlay has degraded to derived.
        let a = AccountUsage {
            total_tokens: Some(445_630_000),
            cost_notional: Some(382.65),
            tokens_per_minute: Some(3_878_000.0),
            provenance: Some(Provenance::Authoritative),
            overlay_ms: Some(
                "2026-07-04T11:58:00Z"
                    .parse::<Timestamp>()
                    .unwrap()
                    .as_millisecond(),
            ),
            collected_at_ms: Some(
                "2026-07-04T11:59:00Z"
                    .parse::<Timestamp>()
                    .unwrap()
                    .as_millisecond(),
            ),
        };
        let b = AccountUsage {
            provenance: Some(Provenance::Derived),
            overlay_ms: Some(
                "2026-07-04T11:50:00Z"
                    .parse::<Timestamp>()
                    .unwrap()
                    .as_millisecond(),
            ),
            ..a
        };
        let fleet =
            build_fleet_view(&[a, b], now, true, 20, LedgerProvenance::Off, 0).expect("some fleet");
        // Representative (identical) usage — never summed.
        assert_eq!(fleet.tokens, "445.63M");
        assert_eq!(fleet.cost_notional, "$382.65 (notional)");
        assert_eq!(fleet.cost_short, "$382n");
        assert_eq!(fleet.burn_rate.as_deref(), Some("232.68M/h")); // 3.878M × 60 = 232.68M
                                                                   // Worst provenance (derived beats authoritative), oldest overlay (11:50 = "limits 10m ago"),
                                                                   // and the NEWEST local snapshot (11:59 = "usage 1m ago").
        assert_eq!(fleet.provenance.expect("badge").text, "derived");
        assert_eq!(fleet.overlay_age.as_deref(), Some("limits 10m ago"));
        assert_eq!(fleet.usage_age.as_deref(), Some("usage 1m ago"));

        // No account has any data ⇒ no fleet line at all.
        assert!(build_fleet_view(
            &[AccountUsage::default()],
            now,
            true,
            20,
            LedgerProvenance::Off,
            0
        )
        .is_none());
    }

    // ── spec 017 §D (acceptance 4): clause math, pure, `today` injected ───────────────────────

    fn ledger_sub(status: SubStatus) -> Subscription {
        Subscription {
            id: "claude-alpha".to_string(),
            status,
            purchased: None,
            renews: None,
            cancelled_on: None,
            paid_through: None,
            verified: None,
        }
    }

    #[test]
    fn no_ledger_row_omits_the_clause() {
        // "active with no dates / no row / plane off" ⇒ every SubView field is None — the header
        // must render byte-identical to the pre-spec-017 form.
        let today = jiff::civil::date(2026, 7, 18);
        let view = build_sub_view(None, today);
        assert_eq!(view, SubView::default());
    }

    #[test]
    fn active_no_dates_at_all_omits_the_clause() {
        let today = jiff::civil::date(2026, 7, 18);
        let sub = ledger_sub(SubStatus::Active);
        let view = build_sub_view(Some(&sub), today);
        assert_eq!(
            view,
            SubView::default(),
            "an active row with no dates carries no information to render"
        );
    }

    #[test]
    fn active_future_renews_with_purchased_full_clause() {
        // Spec 017 §D table, row 1: `purchased` present ⇒ the start segment is shown.
        let today = jiff::civil::date(2026, 7, 18); // 27 days before renews
        let mut sub = ledger_sub(SubStatus::Active);
        sub.purchased = Some(jiff::civil::date(2026, 7, 14));
        sub.renews = Some(jiff::civil::date(2026, 8, 14));
        let view = build_sub_view(Some(&sub), today);
        assert_eq!(
            view.full.as_deref(),
            Some("· period 2026-07-14 → · renews in 27d (2026-08-14)")
        );
        assert_eq!(
            view.full_no_start.as_deref(),
            Some("· renews in 27d (2026-08-14)"),
            "degrade step 1: start segment dropped"
        );
        assert_eq!(
            view.full_no_date.as_deref(),
            Some("· renews in 27d"),
            "degrade step 2: absolute date also dropped"
        );
        assert_eq!(view.compact.as_deref(), Some("· renews 27d"));
    }

    #[test]
    fn active_future_renews_without_purchased_has_no_start_segment() {
        let today = jiff::civil::date(2026, 7, 18);
        let mut sub = ledger_sub(SubStatus::Active);
        sub.renews = Some(jiff::civil::date(2026, 8, 14));
        let view = build_sub_view(Some(&sub), today);
        assert_eq!(
            view.full.as_deref(),
            Some("· renews in 27d (2026-08-14)"),
            "no `purchased` ⇒ the roomiest form already has no start segment"
        );
    }

    #[test]
    fn zero_day_renews_reads_today_not_in_0d() {
        let mut sub = ledger_sub(SubStatus::Active);
        sub.renews = Some(jiff::civil::date(2026, 8, 14));
        let today = jiff::civil::date(2026, 8, 14); // renews is today
        let view = build_sub_view(Some(&sub), today);
        assert_eq!(view.full.as_deref(), Some("· renews today (2026-08-14)"));
        assert_eq!(view.compact.as_deref(), Some("· renews today"));
    }

    #[test]
    fn active_past_renews_shows_the_stale_marker_never_a_negative_countdown() {
        let mut sub = ledger_sub(SubStatus::Active);
        sub.renews = Some(jiff::civil::date(2026, 8, 14));
        let today = jiff::civil::date(2026, 8, 20); // 6 days AFTER renews
        let view = build_sub_view(Some(&sub), today);
        assert_eq!(
            view.full.as_deref(),
            Some("· renews 2026-08-14 (past — ledger stale?)")
        );
        assert!(
            // NOTE: the exact clause above legitimately contains ASCII hyphens (the ISO date
            // itself) — a blanket `contains('-')` would contradict the `assert_eq!` right above
            // it. The actual invariant is "no negative relative count" (e.g. never `in -6d`),
            // which the fixed `(past — ledger stale?)` marker replaces entirely, so check for
            // that specific pattern instead.
            !view.full.as_deref().unwrap_or_default().contains("in -"),
            "must never render a negative day count"
        );
        assert_eq!(view.compact.as_deref(), Some("· renews ?"));
    }

    #[test]
    fn cancelled_future_paid_through_reads_ends_in_parallel_to_active() {
        let mut sub = ledger_sub(SubStatus::Cancelled);
        sub.paid_through = Some(jiff::civil::date(2026, 7, 22));
        let today = jiff::civil::date(2026, 7, 18); // 4 days before paid_through
        let view = build_sub_view(Some(&sub), today);
        assert_eq!(
            view.full.as_deref(),
            Some("· cancelled · ends in 4d (2026-07-22)")
        );
        assert_eq!(view.compact.as_deref(), Some("· ends 4d"));
    }

    #[test]
    fn cancelled_past_paid_through_reads_ended() {
        let mut sub = ledger_sub(SubStatus::Cancelled);
        sub.paid_through = Some(jiff::civil::date(2026, 7, 22));
        let today = jiff::civil::date(2026, 7, 30); // after paid_through
        let view = build_sub_view(Some(&sub), today);
        assert_eq!(view.full.as_deref(), Some("· cancelled · ended 2026-07-22"));
        assert_eq!(view.compact.as_deref(), Some("· ended"));
    }

    #[test]
    fn cancelled_unknown_paid_through_is_the_bare_label() {
        let sub = ledger_sub(SubStatus::Cancelled);
        let today = jiff::civil::date(2026, 7, 18);
        let view = build_sub_view(Some(&sub), today);
        assert_eq!(
            view.full.as_deref(),
            Some("· cancelled"),
            "the status alone is information, never hidden — but never a placeholder either"
        );
        assert_eq!(view.compact.as_deref(), Some("· cancelled"));
    }

    // ── spec 018 §B/§C (acceptance 2): the verified pill ────────────────────────────────────────

    #[test]
    fn active_future_renews_verified_current_shows_the_pill() {
        let today = jiff::civil::date(2026, 7, 18);
        let mut sub = ledger_sub(SubStatus::Active);
        sub.purchased = Some(jiff::civil::date(2026, 7, 14));
        sub.renews = Some(jiff::civil::date(2026, 8, 14));
        sub.verified = Some(jiff::civil::date(2026, 7, 18)); // >= purchased ⇒ current
        let view = build_sub_view(Some(&sub), today);
        assert_eq!(
            view.full.as_deref(),
            Some("· period 2026-07-14 → · renews in 27d (2026-08-14) ✓ 2026-07-18")
        );
        assert_eq!(
            view.full_no_pill_date.as_deref(),
            Some("· period 2026-07-14 → · renews in 27d (2026-08-14) ✓"),
            "degrade step 1: the pill's own date drops, bare ✓ stays"
        );
        assert_eq!(
            view.full_no_start.as_deref(),
            Some("· renews in 27d (2026-08-14) ✓"),
            "degrade step 2: start segment also dropped, bare ✓ stays"
        );
        assert_eq!(
            view.full_no_date.as_deref(),
            Some("· renews in 27d ✓"),
            "degrade step 3: absolute date also dropped, bare ✓ still stays"
        );
        assert_eq!(view.compact.as_deref(), Some("· renews 27d ✓"));
    }

    #[test]
    fn verified_older_than_purchased_shows_no_pill() {
        let today = jiff::civil::date(2026, 7, 18);
        let mut sub = ledger_sub(SubStatus::Active);
        sub.purchased = Some(jiff::civil::date(2026, 7, 14));
        sub.renews = Some(jiff::civil::date(2026, 8, 14));
        sub.verified = Some(jiff::civil::date(2026, 6, 1)); // predates this period's start
        let view = build_sub_view(Some(&sub), today);
        assert_eq!(
            view.full.as_deref(),
            Some("· period 2026-07-14 → · renews in 27d (2026-08-14)"),
            "a verification older than the current period proves nothing about it"
        );
        assert_eq!(view.compact.as_deref(), Some("· renews 27d"));
    }

    #[test]
    fn purchased_absent_uses_the_31_day_recency_window() {
        let mut sub = ledger_sub(SubStatus::Active);
        sub.renews = Some(jiff::civil::date(2026, 8, 14));
        sub.verified = Some(jiff::civil::date(2026, 6, 30));

        let today_in_window = jiff::civil::date(2026, 7, 31); // 31 days after verified
        let view = build_sub_view(Some(&sub), today_in_window);
        assert!(
            view.full.as_deref().unwrap_or_default().contains('✓'),
            "31 days ago is still within the recency window: {:?}",
            view.full
        );

        let today_outside_window = jiff::civil::date(2026, 8, 1); // 32 days after verified
        let view = build_sub_view(Some(&sub), today_outside_window);
        assert!(
            !view.full.as_deref().unwrap_or_default().contains('✓'),
            "32 days ago is outside the recency window: {:?}",
            view.full
        );
    }

    #[test]
    fn stale_renews_suppresses_the_pill_even_when_verified_current() {
        let mut sub = ledger_sub(SubStatus::Active);
        sub.purchased = Some(jiff::civil::date(2026, 7, 14));
        sub.renews = Some(jiff::civil::date(2026, 8, 14));
        sub.verified = Some(jiff::civil::date(2026, 8, 20)); // >= purchased ⇒ would be current
        let today = jiff::civil::date(2026, 8, 21); // AFTER renews ⇒ stale-marked clause
        let view = build_sub_view(Some(&sub), today);
        assert_eq!(
            view.full.as_deref(),
            Some("· renews 2026-08-14 (past — ledger stale?)"),
            "a stale-marked clause never shows ✓, even when verified-current"
        );
        assert!(!view.full.as_deref().unwrap_or_default().contains('✓'));
        assert_eq!(view.compact.as_deref(), Some("· renews ?"));
    }

    #[test]
    fn ended_state_suppresses_the_pill_even_when_verified_current() {
        let mut sub = ledger_sub(SubStatus::Cancelled);
        sub.paid_through = Some(jiff::civil::date(2026, 7, 22));
        sub.verified = Some(jiff::civil::date(2026, 7, 25)); // after paid_through, still "current"
        let today = jiff::civil::date(2026, 7, 30); // after paid_through ⇒ ended
        let view = build_sub_view(Some(&sub), today);
        assert_eq!(view.full.as_deref(), Some("· cancelled · ended 2026-07-22"));
        assert!(!view.full.as_deref().unwrap_or_default().contains('✓'));
        assert_eq!(view.compact.as_deref(), Some("· ended"));
    }

    #[test]
    fn cancelled_future_paid_through_verified_current_shows_the_pill() {
        let mut sub = ledger_sub(SubStatus::Cancelled);
        sub.paid_through = Some(jiff::civil::date(2026, 7, 22));
        sub.verified = Some(jiff::civil::date(2026, 7, 18));
        let today = jiff::civil::date(2026, 7, 18);
        let view = build_sub_view(Some(&sub), today);
        assert_eq!(
            view.full.as_deref(),
            Some("· cancelled · ends in 4d (2026-07-22) ✓ 2026-07-18")
        );
        assert_eq!(view.compact.as_deref(), Some("· ends 4d ✓"));
    }

    #[test]
    fn bare_cancelled_verified_current_shows_the_pill() {
        let mut sub = ledger_sub(SubStatus::Cancelled);
        sub.verified = Some(jiff::civil::date(2026, 7, 1));
        let today = jiff::civil::date(2026, 7, 18); // within the 31-day window, no `purchased`
        let view = build_sub_view(Some(&sub), today);
        assert_eq!(view.full.as_deref(), Some("· cancelled ✓ 2026-07-01"));
        assert_eq!(view.compact.as_deref(), Some("· cancelled ✓"));
    }

    #[test]
    fn no_verified_field_shows_no_pill_anywhere() {
        let mut sub = ledger_sub(SubStatus::Active);
        sub.renews = Some(jiff::civil::date(2026, 8, 14));
        let today = jiff::civil::date(2026, 7, 18);
        let view = build_sub_view(Some(&sub), today);
        assert!(!view.full.as_deref().unwrap_or_default().contains('✓'));
        assert!(!view.compact.as_deref().unwrap_or_default().contains('✓'));
    }

    // ── spec 017 §C/§D (acceptance 7): independence — Account.active vs ledger status ─────────

    #[test]
    fn inactive_config_account_still_shows_an_active_ledger_clause() {
        // `active = false` (spec 014, dims/tags the row) must not suppress an `active`-status
        // ledger clause — the two bits are independent. Exercises `build_account_view`'s OWN
        // exact-id join (`ledger_rows` is non-empty and matches `account.id`), not a manually
        // stapled-on `SubView` — proving the join itself, not just `build_sub_view` in isolation.
        let account = Account {
            id: "personal".to_string(),
            label: "Personal".to_string(),
            provider: Provider::Claude,
            config_dir: Some(PathBuf::from("/tmp")),
            api_key_env: None,
            color: None,
            active: false,
            limits_overlay: false,
        };
        let mut sub = ledger_sub(SubStatus::Active);
        sub.id = "personal".to_string();
        sub.renews = Some(jiff::civil::date(2026, 8, 14));
        let row = build_account_view(
            &account,
            AccountData {
                snapshot: None,
                limits: &[],
                token_status: None,
                overlay_failing_since: None,
                overlay_ms: None,
                ledger_rows: std::slice::from_ref(&sub),
                today: jiff::civil::date(2026, 7, 18),
            },
            "2026-07-18T00:00:00Z".parse().unwrap(),
            true,
        );
        assert!(row.inactive, "the config flag still dims/tags the row");
        assert!(
            row.sub.full.is_some(),
            "an inactive (config) account must still render its active-in-the-ledger clause"
        );
    }

    #[test]
    fn active_config_account_still_shows_a_cancelled_ledger_clause() {
        // `active = true` must not suppress a `cancelled`-status ledger clause — the ledger never
        // drives monitoring on/off, and the config flag never drives the date display. Same
        // real-join exercise as above, mirrored.
        let account = Account {
            id: "personal".to_string(),
            label: "Personal".to_string(),
            provider: Provider::Claude,
            config_dir: Some(PathBuf::from("/tmp")),
            api_key_env: None,
            color: None,
            active: true,
            limits_overlay: false,
        };
        let mut sub = ledger_sub(SubStatus::Cancelled);
        sub.id = "personal".to_string();
        let row = build_account_view(
            &account,
            AccountData {
                snapshot: None,
                limits: &[],
                token_status: None,
                overlay_failing_since: None,
                overlay_ms: None,
                ledger_rows: std::slice::from_ref(&sub),
                today: jiff::civil::date(2026, 7, 18),
            },
            "2026-07-18T00:00:00Z".parse().unwrap(),
            true,
        );
        assert!(!row.inactive);
        assert_eq!(
            row.sub.full.as_deref(),
            Some("· cancelled"),
            "a monitored (active) account still shows its ledger-cancelled clause"
        );
    }

    #[test]
    fn build_account_view_does_not_join_a_near_miss_id() {
        // Spec 017 §C acceptance 3: the join inside `build_account_view` itself (not just
        // `ledger::find` in isolation) must stay exact-match — a near-miss id joins nothing.
        let account = Account {
            id: "claude-rob-7".to_string(),
            label: "Rob".to_string(),
            provider: Provider::Claude,
            config_dir: Some(PathBuf::from("/tmp")),
            api_key_env: None,
            color: None,
            active: true,
            limits_overlay: false,
        };
        let mut sub = ledger_sub(SubStatus::Active);
        sub.id = "claude-rob7".to_string();
        sub.renews = Some(jiff::civil::date(2026, 8, 14));
        let row = build_account_view(
            &account,
            AccountData {
                snapshot: None,
                limits: &[],
                token_status: None,
                overlay_failing_since: None,
                overlay_ms: None,
                ledger_rows: std::slice::from_ref(&sub),
                today: jiff::civil::date(2026, 7, 18),
            },
            "2026-07-18T00:00:00Z".parse().unwrap(),
            true,
        );
        assert_eq!(
            row.sub,
            SubView::default(),
            "a near-miss id must never join through build_account_view"
        );
    }

    // ── spec 017 §D (fleet header token, part of acceptance 3/5) ───────────────────────────────

    #[test]
    fn ledger_fleet_note_tokens_per_provenance() {
        assert_eq!(
            ledger_fleet_note(LedgerProvenance::Missing, 0).as_deref(),
            Some("· no ledger")
        );
        assert_eq!(
            ledger_fleet_note(LedgerProvenance::Stale, 0).as_deref(),
            Some("· ledger stale")
        );
        assert_eq!(
            ledger_fleet_note(LedgerProvenance::Fresh, 0).as_deref(),
            Some("· ledger: 0 matched")
        );
        assert_eq!(
            ledger_fleet_note(LedgerProvenance::Fresh, 3),
            None,
            "Fresh with at least one matched row says nothing extra"
        );
        assert_eq!(
            ledger_fleet_note(LedgerProvenance::Off, 0),
            None,
            "Off renders nothing anywhere in the TUI"
        );
    }
}
