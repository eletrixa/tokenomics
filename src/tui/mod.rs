//! The TUI event loop: draw the dashboard, fold key/tick messages, read the store. Only I/O site.
//!
//! Project: Tokenomics — monitor LLM subscription accounts (usage, limits, time-left) in a TUI
//! Module:  src/tui/mod.rs
//! Deps:    ratatui (init/restore + panic hook), crossterm (EventStream), tokio, jiff; store (read);
//!          billing::manual (ApiManual read-through plane)
//! Tested:  the seams are tested in keys/model/view; this loop is the thin I/O shell. `read_rows`'s
//!          `active`/`show_inactive` filtering + fleet exclusion get an inline `#[cfg(test)]`
//!          (spec 014) since that wiring lives only here.
//!
//! Key responsibilities:
//! - Own the terminal (via `ratatui::try_init`/`try_restore` — installs the panic hook) and the loop.
//! - Draw the pure `view`, fold `keys`→`Msg`, and re-read the store on a modest tick / on refresh.
//!
//! - Hot-reload the config file on the existing tick (spec 015 §A2): the TUI is the other long-running
//!   process, so account/threshold edits reach the board without a relaunch. Same injectable
//!   `ConfigSource` seam as the collector; the TUI stays READ-ONLY on the store (only the FILE reloads).
//! - Poll the subscription ledger on the same tick (spec 017 §B): a third, TUI-only, read-through
//!   plane via the injectable `LedgerSource` seam — display-only, so unlike the config swap it never
//!   changes which accounts exist or are polled. The path resolves once at startup (a foreign-repo
//!   file, not expected to move mid-session — see `run`).
//! - Poll the manual API-billing TOML on the same tick, right after the ledger (spec 024/025): a
//!   fourth, TUI-only, read-through plane via the injectable `ApiManualSource` seam. Its path also
//!   resolves once at startup, same parity as the ledger's — a foreign, hand-edited file, not
//!   expected to move mid-session; `[[api]]` config entries themselves still hot-reload via `cfg`.
//!   Each read pass also re-reads the store's live `api_billing` rows and assembles both into the
//!   pure `build_api_view` so `Dashboard.api` is one consistent snapshot per tick, never mixed
//!   across reads.
//!
//! Design constraints:
//! - Reader only: the collector process writes the store; the TUI reads it (CLAUDE.md data planes).
//! - `view`/`update` do no I/O; the only I/O is this loop's store reads, config/ledger reload, and draws.

pub mod keys;
pub mod model;
pub mod view;

use std::path::Path;
use std::time::Duration;

use crossterm::event::{Event, EventStream};
use futures::StreamExt;
use jiff::Timestamp;
use ratatui::DefaultTerminal;

use crate::billing::manual::{
    self, ApiManual, ApiManualProvenance, ApiManualRow, FileApiManualSource,
};
use crate::billing::ApiAccount;
use crate::collector::{ConfigSource, FileConfigSource};
use crate::config::Config;
use crate::domain::Account;
use crate::error::{AppError, AppResult};
use crate::ledger::{
    self, find as ledger_find, FileLedgerSource, Ledger, LedgerProvenance, Subscription,
};
use crate::store::{ApiBillingRow, Store};

use model::{
    account_usage, build_account_view, build_api_view, build_fleet_view, collector_alert,
    error_view, AccountData, AccountUsage, AccountView, ApiData, App, Dashboard, Msg,
};

/// Redraw/store-read cadence (a modest tick, not a busy redraw).
const TICK: Duration = Duration::from_secs(1);
/// Collection ticks to pull for the header aggregate burn-rate sparkline.
const HISTORY_POINTS: usize = 64;

/// Run the dashboard until the user quits. Reads `store_path` (the collector's output).
pub async fn run(cfg: &Config, store_path: &Path) -> AppResult<()> {
    let use_color = std::env::var_os("NO_COLOR").is_none();
    let reader = Store::open(store_path)?;
    // Inactive accounts are hidden by default (spec 014 §C), so the pre-data header count is the
    // active-only count — matching what the first `read_rows` (show_inactive = false) will build.
    let active_count = cfg.accounts.iter().filter(|a| a.active).count();
    let mut app = App::new(active_count, use_color);

    // The TUI owns a working copy of the config and hot-reloads the FILE on its tick (spec 015 §A2).
    // The store stays read-only; only the config file is re-read, via the same seam as the collector.
    let mut cfg = cfg.clone();
    let config_source = FileConfigSource::new();

    // The ledger plane (spec 017 §B) resolves its path once at startup, unlike the config file — a
    // foreign-repo file whose location isn't expected to move mid-session (accounts/thresholds DO
    // hot-reload via `cfg`; the ledger's own CONTENT still hot-reloads below, just not its path).
    // ponytail: re-resolving the path every tick (in case `[settings] ledger_path` itself changes via
    // a config hot-reload) would add a second path-resolution seam for no test-driven need — add it
    // if a user asks to move the ledger file mid-session without restarting.
    let mut ledger = Ledger::new();
    let mut ledger_source = ledger::resolve_path(
        std::env::var(ledger::LEDGER_ENV).ok().as_deref(),
        cfg.settings.ledger_path.as_deref(),
    )
    .map(FileLedgerSource::new);

    // The manual API-billing plane (spec 024/025) resolves its path once at startup too — same
    // parity as the ledger above (a foreign, hand-edited file; `[[api]]` entries still hot-reload
    // via `cfg`, only the manual file's own location doesn't move mid-session).
    let mut api_manual = ApiManual::new();
    let mut api_manual_source = manual::resolve_path(
        std::env::var(manual::API_MANUAL_ENV).ok().as_deref(),
        cfg.settings.api_manual_path.as_deref(),
    )
    .map(FileApiManualSource::new);

    let mut terminal = ratatui::try_init()
        .map_err(|e| AppError::Terminal(format!("cannot start the TUI: {e}")))?;
    let planes = Planes {
        ledger: &mut ledger,
        ledger_source: ledger_source.as_mut(),
        api_manual: &mut api_manual,
        api_manual_source: api_manual_source.as_mut(),
    };
    let result = event_loop(
        &mut terminal,
        &mut app,
        &mut cfg,
        config_source,
        &reader,
        planes,
    )
    .await;
    let _ = ratatui::try_restore();
    result
}

/// Bundles the ledger + manual-billing pollers so `event_loop` stays under clippy's
/// argument-count lint (the read-only counterpart, [`PlanesRead`], bundles what one tick's polls
/// produce for `read_rows`; this bundles the mutable pollers themselves).
struct Planes<'a> {
    ledger: &'a mut Ledger,
    ledger_source: Option<&'a mut FileLedgerSource>,
    api_manual: &'a mut ApiManual,
    api_manual_source: Option<&'a mut FileApiManualSource>,
}

/// Poll the ledger source (spec 017 §B) — a no-op when the plane is unconfigured (`source` is
/// `None`, i.e. `Off` forever, per [`Ledger::new`]'s own doc). Mirrors [`apply_tui_reload`]'s split:
/// the keep-last-good / `Stale` discipline lives in `Ledger::poll` itself, this is just the one-line
/// call site the event loop needs.
fn poll_ledger(ledger: &mut Ledger, source: Option<&mut FileLedgerSource>) {
    if let Some(source) = source {
        ledger.poll(source);
    }
}

/// Poll the manual API-billing source (spec 024/025) — a no-op when the plane is unconfigured
/// (`source` is `None`, i.e. `Off` forever, per [`ApiManual::new`]'s own doc). Mirrors
/// [`poll_ledger`]'s one-line call-site split.
fn poll_api_manual(api_manual: &mut ApiManual, source: Option<&mut FileApiManualSource>) {
    if let Some(source) = source {
        api_manual.poll(source);
    }
}

/// Swap the TUI's working config from a polled reload (spec 015 §A2). A `Some` poll swaps the whole
/// config — accounts (add / remove / `active` flips) and thresholds take effect on the next store
/// read; a `None` poll — no change, or a bad reload the source kept last-good — leaves it untouched
/// (the TUI is not a log surface). Split out so the swap is unit-tested without a live event loop.
fn apply_tui_reload<S: ConfigSource>(cfg: &mut Config, source: &mut S) {
    if let Some(new_cfg) = source.poll() {
        *cfg = new_cfg;
    }
}

async fn event_loop<S: ConfigSource>(
    terminal: &mut DefaultTerminal,
    app: &mut App,
    cfg: &mut Config,
    mut config_source: S,
    reader: &Store,
    planes: Planes<'_>,
) -> AppResult<()> {
    let Planes {
        ledger,
        mut ledger_source,
        api_manual,
        mut api_manual_source,
    } = planes;

    let mut events = EventStream::new();
    let mut tick = tokio::time::interval(TICK);
    // After a suspend/stall, resume the redraw cadence "from now" rather than firing a burst of
    // backlogged ticks (spec 011 §F).
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    refresh(app, cfg, reader, ledger, api_manual);
    loop {
        terminal
            .draw(|f| view::render(f, app))
            .map_err(|e| AppError::Terminal(format!("draw failed: {e}")))?;

        tokio::select! {
            maybe = events.next() => match maybe {
                Some(Ok(Event::Key(key))) => {
                    if let Some(action) = keys::map(key) {
                        app.update(Msg::Key(action));
                    }
                }
                Some(Ok(Event::Resize(_, _))) => app.update(Msg::Resize),
                _ => {}
            },
            _ = tick.tick() => {
                // Hot-reload BEFORE the store read, so an added/(re)activated account or a changed
                // threshold reaches this tick's board (spec 015 §A2); the ledger polls the same tick
                // (spec 017 §B), then the manual API-billing plane right after it (spec 024/025) —
                // both display-only, so neither ever changes which accounts are read below.
                apply_tui_reload(cfg, &mut config_source);
                poll_ledger(ledger, ledger_source.as_deref_mut());
                poll_api_manual(api_manual, api_manual_source.as_deref_mut());
                app.update(Msg::Tick);
                refresh(app, cfg, reader, ledger, api_manual);
            }
            _ = tokio::signal::ctrl_c() => app.should_quit = true,
        }

        if app.reload_requested {
            app.reload_requested = false;
            refresh(app, cfg, reader, ledger, api_manual);
        }
        if app.should_quit {
            break;
        }
    }
    Ok(())
}

/// Re-read the store into `app` — the one place `read_rows` feeds `Msg::Data`, so the three call
/// sites (initial load, the redraw tick, and a `r`/`i` refresh) can't drift out of sync. Also the
/// one place the store's live `api_billing` rows are read (spec 025 §B): a failed read degrades to
/// an empty vec — the same silent-degrade the aggregate-burn read above it already uses — rather
/// than losing the whole tick's board over one query.
fn refresh(app: &mut App, cfg: &Config, reader: &Store, ledger: &Ledger, api_manual: &ApiManual) {
    let api_live = reader.latest_api_billing().unwrap_or_default();
    let rows = read_rows(
        cfg,
        reader,
        app.use_color,
        app.show_inactive,
        &app.rows,
        app.collector_alert.as_deref(),
        PlanesRead {
            ledger_rows: ledger.rows(),
            ledger_provenance: ledger.provenance(),
            api_live: &api_live,
            manual_rows: api_manual.rows(),
            manual_provenance: api_manual.provenance(),
        },
    );
    app.update(Msg::Data(Box::new(rows)));
}

/// One tick's read-through-plane snapshot — the ledger (spec 017 §D) plus the API billing overview's
/// live store rows and manual TOML rows (spec 025 §B) — bundled so `read_rows` stays under clippy's
/// argument-count lint (mirrors [`AccountData`]'s own bundling for the same reason).
#[derive(Debug, Clone, Copy)]
struct PlanesRead<'a> {
    ledger_rows: &'a [Subscription],
    ledger_provenance: LedgerProvenance,
    api_live: &'a [ApiBillingRow],
    manual_rows: &'a [ApiManualRow],
    manual_provenance: ApiManualProvenance,
}

/// Read the latest per-account rows plus the fleet-wide aggregate burn series from the store.
/// Per-account errors are isolated: a failing read keeps that account's previous row (or an error
/// placeholder) rather than crashing the whole loop; an aggregate read failure degrades to no bar.
///
/// `show_inactive` gates which accounts are visible at all (spec 014 §C): an inactive account is
/// skipped entirely unless `show_inactive` is on. `previous` is indexed positionally against this
/// same visible list, which holds as long as `show_inactive` didn't change since the read that
/// produced it — the one call (right after a toggle) where it might not is harmless: `.get` never
/// panics, and the very next read re-aligns.
fn read_rows(
    cfg: &Config,
    reader: &Store,
    use_color: bool,
    show_inactive: bool,
    previous: &[AccountView],
    previous_alert: Option<&str>,
    planes: PlanesRead<'_>,
) -> Dashboard {
    let PlanesRead {
        ledger_rows,
        ledger_provenance,
        api_live,
        manual_rows,
        manual_provenance,
    } = planes;
    let now = Timestamp::now();
    // "Today" for the ledger's day-count math (spec 017 §D: "calendar-day differences in local
    // time") — the system timezone, mirroring `doctor::run_doctor`'s
    // `now.to_zoned(TimeZone::system()).date()` so the TUI and `tok doctor` never disagree on
    // "today" (and neither reads a day early/late relative to the user's own clock).
    let today = now.to_zoned(jiff::tz::TimeZone::system()).date();
    let visible: Vec<&Account> = cfg
        .accounts
        .iter()
        .filter(|a| a.active || show_inactive)
        .collect();
    let mut rows = Vec::with_capacity(visible.len());
    let mut usages = Vec::with_capacity(visible.len());
    // Counted over ALL configured accounts, not just the visibility-filtered `visible` list below
    // (spec 017 acceptance 3 ties this token to the config↔ledger join, not row visibility) — with
    // `show_inactive` off, a ledger row matching a hidden inactive account must still count.
    let ledger_matched = cfg
        .accounts
        .iter()
        .filter(|a| ledger_find(ledger_rows, &a.id).is_some())
        .count();
    for (idx, account) in visible.into_iter().enumerate() {
        match read_account_row(reader, account, use_color, now, ledger_rows, today) {
            Ok((row, usage)) => {
                rows.push(row);
                // An inactive account's usage never feeds the fleet reduction (shared tokens/cost/
                // burn, worst provenance, oldest refresh) even when peeked at — display-only (§C).
                if account.active {
                    usages.push(usage);
                }
            }
            Err(e) => {
                rows.push(
                    previous
                        .get(idx)
                        .cloned()
                        .unwrap_or_else(|| error_view(account, use_color, &e.to_string())),
                );
                // A failed read contributes no usage — the reduction skips it (the other,
                // identical-by-shared-logs accounts still supply the fleet figures).
                if account.active {
                    usages.push(AccountUsage::default());
                }
            }
        }
    }
    let aggregate_burn = reader
        .aggregate_burn_history(HISTORY_POINTS)
        .unwrap_or_default();
    let fleet = build_fleet_view(
        &usages,
        now,
        use_color,
        cfg.settings.poll_local_secs,
        ledger_provenance,
        ledger_matched,
    );
    // Collector liveness: a stale/absent heartbeat means the store isn't being written — the rows
    // above are frozen, not live. A transient read error (SQLITE_BUSY) must NOT read as "never
    // started": it keeps last tick's verdict rather than flashing the loud "not running" banner.
    let collector_alert = collector_alert_for_read(
        &reader.heartbeat_age("collector", now.as_millisecond()),
        cfg.settings.poll_local_secs,
        previous_alert,
    );
    // The API billing plane (spec 025 §B) only assembles when configured — any `[[api]]` entry, or
    // a manual-only divergent id — so an unconfigured board never grows an empty section. The
    // env-key presence probe is the one place this module touches `std::env` for it; `build_api_view`
    // itself stays pure.
    let api = api_plane_configured(&cfg.api, manual_rows).then(|| {
        let key_present = |name: &str| std::env::var(name).is_ok_and(|v| !v.trim().is_empty());
        build_api_view(
            &ApiData {
                accounts: &cfg.api,
                live: api_live,
                manual: manual_rows,
                manual_provenance,
                key_present: &key_present,
                poll_billing_secs: cfg.settings.poll_billing_secs,
            },
            now,
            use_color,
        )
    });
    Dashboard {
        rows,
        aggregate_burn,
        fleet,
        collector_alert,
        api,
    }
}

/// Whether the API billing plane has anything to show this tick: any `[[api]]` config entry
/// configures it (even before its first live/manual figure lands — spec 025 §B renders its empty
/// state), or a manual-only divergent id does. Pure — unit-tested; keeps the "assemble or not" call
/// in `read_rows` a one-line predicate.
fn api_plane_configured(accounts: &[ApiAccount], manual_rows: &[ApiManualRow]) -> bool {
    !accounts.is_empty() || !manual_rows.is_empty()
}

/// Decide the collector-liveness banner for one store read, degrading a transient read error to the
/// previous tick's verdict instead of the loud "never started" banner. `Ok(None)` is a genuine
/// never-started heartbeat and still alarms; `Ok(Some(age))` classifies by the local cadence; an
/// `Err` — a transient `SQLITE_BUSY`, say — is a hiccup, so it keeps whatever the last tick showed
/// (spec 011 §A; the store-read twin of `read_rows`' per-row `previous` degrade). Pure — unit-tested.
fn collector_alert_for_read(
    heartbeat_age: &AppResult<Option<i64>>,
    poll_local_secs: u64,
    previous_alert: Option<&str>,
) -> Option<String> {
    match heartbeat_age {
        Ok(age) => collector_alert(*age, poll_local_secs),
        Err(_) => previous_alert.map(str::to_string),
    }
}

/// Read and build one account's row plus its fleet usage facts (the fallible part, so `read_rows`
/// can isolate failures).
fn read_account_row(
    reader: &Store,
    account: &Account,
    use_color: bool,
    now: Timestamp,
    ledger_rows: &[Subscription],
    today: jiff::civil::Date,
) -> AppResult<(AccountView, AccountUsage)> {
    let snapshot = reader.latest_snapshot(&account.id)?;
    let limits = reader.latest_limits(&account.id)?;
    let overlay_ms = reader.last_overlay_success(&account.id)?;
    let data = AccountData {
        snapshot: snapshot.as_ref(),
        limits: &limits,
        token_status: reader.latest_token_status(&account.id)?,
        overlay_failing_since: reader.overlay_failing_since(&account.id)?,
        overlay_ms,
        ledger_rows,
        today,
    };
    let usage = account_usage(snapshot.as_ref(), &limits, overlay_ms);
    Ok((build_account_view(account, data, now, use_color), usage))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::billing::{ApiFigureKind, ApiProvider};
    use crate::config::Settings;
    use crate::domain::{Limit, LimitKind, Provenance, Provider, Severity, UsageSnapshot};
    use std::path::PathBuf;

    fn account(id: &str, active: bool) -> Account {
        Account {
            id: id.to_string(),
            label: id.to_uppercase(),
            provider: Provider::Claude,
            config_dir: Some(PathBuf::from("/tmp")),
            api_key_env: None,
            color: None,
            active,
            limits_overlay: false,
        }
    }

    fn config(accounts: Vec<Account>) -> Config {
        Config {
            settings: Settings::default(),
            accounts,
            api: Vec::new(),
        }
    }

    /// An in-memory config source for the TUI hot-reload test: each poll delivers the next queued
    /// entry (`Some` = a swap, `None` = a bad/absent reload the source keeps last-good on).
    struct TestSource(std::collections::VecDeque<Option<Config>>);

    impl ConfigSource for TestSource {
        fn mtime_ms(&self) -> Option<i64> {
            None
        }
        fn config_path(&self) -> Option<String> {
            None
        }
        fn poll(&mut self) -> Option<Config> {
            self.0.pop_front().flatten()
        }
    }

    #[test]
    fn tui_config_swap_reaches_the_board_and_bad_reload_keeps_last_good() {
        // Spec 015 §A2 / acceptance 7: a mid-session config swap (a second account appears) reaches
        // the board within one tick via `apply_tui_reload` + the next `read_rows`; a bad reload
        // (a `None` poll) keeps the last-good config.
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Store::open(&dir.path().join("t.db")).expect("open");
        let now = Timestamp::now();
        store
            .upsert_accounts(&[account("a", true), account("b", true)])
            .expect("upsert");
        store
            .insert_snapshot(&snapshot("a", 100, now))
            .expect("a snap");
        store
            .insert_snapshot(&snapshot("b", 200, now))
            .expect("b snap");

        let mut cfg = config(vec![account("a", true)]); // only "a" visible at launch
        let mut source = TestSource(
            vec![
                Some(config(vec![account("a", true), account("b", true)])),
                None, // a subsequent bad reload
            ]
            .into(),
        );

        let before = read_rows(
            &cfg,
            &store,
            true,
            false,
            &[],
            None,
            PlanesRead {
                ledger_rows: &[],
                ledger_provenance: LedgerProvenance::Off,
                api_live: &[],
                manual_rows: &[],
                manual_provenance: ApiManualProvenance::Off,
            },
        );
        assert_eq!(before.rows.len(), 1, "only 'a' before the reload");

        apply_tui_reload(&mut cfg, &mut source);
        let after = read_rows(
            &cfg,
            &store,
            true,
            false,
            &before.rows,
            None,
            PlanesRead {
                ledger_rows: &[],
                ledger_provenance: LedgerProvenance::Off,
                api_live: &[],
                manual_rows: &[],
                manual_provenance: ApiManualProvenance::Off,
            },
        );
        assert_eq!(after.rows.len(), 2, "the swapped-in 'b' reaches the board");

        // A bad reload (None) leaves the last-good (2-account) config in place.
        apply_tui_reload(&mut cfg, &mut source);
        assert_eq!(cfg.accounts.len(), 2, "a bad reload keeps last-good");
    }

    fn limit(account_id: &str, pct: f64, severity: Severity) -> Limit {
        Limit {
            account_id: account_id.to_string(),
            provider: Provider::Claude,
            kind: LimitKind::Session,
            scope: None,
            utilization_pct: pct,
            resets_at: "2999-01-01T00:00:00Z".to_string(), // far future — never expired
            severity,
            source: Provenance::Derived,
        }
    }

    fn snapshot(account_id: &str, total_tokens: u64, now: Timestamp) -> UsageSnapshot {
        UsageSnapshot {
            account_id: account_id.to_string(),
            provider: Provider::Claude,
            collected_at: now,
            input: total_tokens,
            output: 0,
            cache_read: 0,
            cache_creation: 0,
            total_tokens,
            cost_notional: Some(1.0),
            window: None,
        }
    }

    /// Wiring test for spec 014 §C: an inactive account is entirely absent from `read_rows` by
    /// default (AC3), and even when `show_inactive` reveals it, its usage never joins the fleet
    /// reduction (AC3–4) — proven by a "dead" account whose crit limit + huge token count would
    /// otherwise dominate both the banner-driving severity and the shared fleet numbers.
    #[test]
    fn inactive_account_hidden_by_default_and_excluded_from_fleet_when_shown() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Store::open(&dir.path().join("t.db")).expect("open");
        let cfg = config(vec![account("active", true), account("dead", false)]);
        store.upsert_accounts(&cfg.accounts).expect("upsert");
        let now = Timestamp::now();

        store
            .insert_snapshot(&snapshot("active", 500, now))
            .expect("seed active snapshot");
        store
            .set_limits("active", &[limit("active", 10.0, Severity::Ok)], now)
            .expect("seed active limit");
        store
            .insert_snapshot(&snapshot("dead", 999_000, now))
            .expect("seed dead snapshot");
        store
            .set_limits("dead", &[limit("dead", 97.0, Severity::Crit)], now)
            .expect("seed dead limit");

        // Default: show_inactive = false ⇒ the dead account is entirely absent (AC3).
        let hidden = read_rows(
            &cfg,
            &store,
            true,
            false,
            &[],
            None,
            PlanesRead {
                ledger_rows: &[],
                ledger_provenance: LedgerProvenance::Off,
                api_live: &[],
                manual_rows: &[],
                manual_provenance: ApiManualProvenance::Off,
            },
        );
        assert_eq!(hidden.rows.len(), 1, "only the active account is visible");
        assert!(!hidden.rows[0].title.contains("(inactive)"));
        let fleet_hidden = hidden.fleet.expect("active account has data");
        assert_eq!(fleet_hidden.tokens, "500");

        // Shown: the dead row appears, tagged — but the fleet line is UNCHANGED (still only
        // active's 500 tokens, not dead's 999,000), and its crit never displaces the header count.
        let shown = read_rows(
            &cfg,
            &store,
            true,
            true,
            &hidden.rows,
            None,
            PlanesRead {
                ledger_rows: &[],
                ledger_provenance: LedgerProvenance::Off,
                api_live: &[],
                manual_rows: &[],
                manual_provenance: ApiManualProvenance::Off,
            },
        );
        assert_eq!(shown.rows.len(), 2, "both accounts now visible");
        let dead_row = shown
            .rows
            .iter()
            .find(|r| r.title.contains("(inactive)"))
            .expect("dead account row, tagged");
        assert!(dead_row.inactive);
        let fleet_shown = shown.fleet.expect("still has data");
        assert_eq!(
            fleet_shown.tokens, "500",
            "the inactive account's usage must never join the fleet reduction"
        );
    }

    #[test]
    fn collector_alert_read_error_keeps_previous_verdict_but_ok_none_still_alarms() {
        // Finding 1: a transient heartbeat read error must not be conflated with "never started".
        // `Ok(None)` is the ONLY genuine never-started signal and still raises the loud banner...
        assert_eq!(
            collector_alert_for_read(&Ok(None), 10, None).as_deref(),
            Some("collector not running — data frozen (start `tok collector`)")
        );
        // ...a fresh beat is live (no alert)...
        assert!(collector_alert_for_read(&Ok(Some(5_000)), 10, None).is_none());
        // ...but an `Err` (SQLITE_BUSY hiccup) keeps whatever the previous tick showed, rather than
        // flashing the "not running" banner the raw `.unwrap_or(None)` used to.
        assert_eq!(
            collector_alert_for_read(
                &Err(AppError::StoreData("database is locked".to_string())),
                10,
                Some("collector stalled — last beat 2m ago"),
            )
            .as_deref(),
            Some("collector stalled — last beat 2m ago")
        );
        // An `Err` on the very first read (no previous verdict) stays silent — never cry wolf.
        assert!(collector_alert_for_read(
            &Err(AppError::StoreData("database is locked".to_string())),
            10,
            None,
        )
        .is_none());
    }

    fn api_account(id: &str) -> ApiAccount {
        ApiAccount {
            id: id.to_string(),
            label: None,
            provider: ApiProvider::Anthropic,
            api_key_env: None,
            active: true,
        }
    }

    fn manual_row(id: &str) -> ApiManualRow {
        ApiManualRow {
            id: id.to_string(),
            figure: ApiFigureKind::Balance,
            amount_minor: 100,
            currency: "USD".to_string(),
            as_of: "2026-01-01".to_string(),
            notes: None,
        }
    }

    #[test]
    fn api_plane_configured_needs_an_api_account_or_a_manual_row() {
        // Spec 025 §B: the plane only assembles (and the section appears) when there's a `[[api]]`
        // entry to show, or a manual-only divergent id — never for a board with neither.
        assert!(!api_plane_configured(&[], &[]), "neither ⇒ unconfigured");
        assert!(
            api_plane_configured(&[api_account("a")], &[]),
            "a config entry alone configures it"
        );
        assert!(
            api_plane_configured(&[], &[manual_row("divergent")]),
            "a manual-only row alone configures it"
        );
    }
}
