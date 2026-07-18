//! The TUI event loop: draw the dashboard, fold key/tick messages, read the store. Only I/O site.
//!
//! Project: Tokenomics — monitor LLM subscription accounts (usage, limits, time-left) in a TUI
//! Module:  src/tui/mod.rs
//! Deps:    ratatui (init/restore + panic hook), crossterm (EventStream), tokio, jiff; store (read)
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

use crate::collector::{ConfigSource, FileConfigSource};
use crate::config::Config;
use crate::domain::Account;
use crate::error::{AppError, AppResult};
use crate::ledger::{
    self, find as ledger_find, FileLedgerSource, Ledger, LedgerProvenance, Subscription,
};
use crate::store::Store;

use model::{
    account_usage, build_account_view, build_fleet_view, collector_alert, error_view, AccountData,
    AccountUsage, AccountView, App, Dashboard, Msg,
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

    let mut terminal = ratatui::try_init()
        .map_err(|e| AppError::Terminal(format!("cannot start the TUI: {e}")))?;
    let result = event_loop(
        &mut terminal,
        &mut app,
        &mut cfg,
        config_source,
        &reader,
        &mut ledger,
        ledger_source.as_mut(),
    )
    .await;
    let _ = ratatui::try_restore();
    result
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
    ledger: &mut Ledger,
    mut ledger_source: Option<&mut FileLedgerSource>,
) -> AppResult<()> {
    let mut events = EventStream::new();
    let mut tick = tokio::time::interval(TICK);
    // After a suspend/stall, resume the redraw cadence "from now" rather than firing a burst of
    // backlogged ticks (spec 011 §F).
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    refresh(app, cfg, reader, ledger);
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
                // (spec 017 §B) — display-only, so it never changes which accounts are read below.
                apply_tui_reload(cfg, &mut config_source);
                poll_ledger(ledger, ledger_source.as_deref_mut());
                app.update(Msg::Tick);
                refresh(app, cfg, reader, ledger);
            }
            _ = tokio::signal::ctrl_c() => app.should_quit = true,
        }

        if app.reload_requested {
            app.reload_requested = false;
            refresh(app, cfg, reader, ledger);
        }
        if app.should_quit {
            break;
        }
    }
    Ok(())
}

/// Re-read the store into `app` — the one place `read_rows` feeds `Msg::Data`, so the three call
/// sites (initial load, the redraw tick, and a `r`/`i` refresh) can't drift out of sync.
fn refresh(app: &mut App, cfg: &Config, reader: &Store, ledger: &Ledger) {
    let rows = read_rows(
        cfg,
        reader,
        app.use_color,
        app.show_inactive,
        &app.rows,
        app.collector_alert.as_deref(),
        LedgerRead {
            rows: ledger.rows(),
            provenance: ledger.provenance(),
        },
    );
    app.update(Msg::Data(Box::new(rows)));
}

/// One ledger read (spec 017 §D), bundled so `read_rows` stays under clippy's argument-count lint —
/// mirrors [`AccountData`]'s own bundling for the same reason.
#[derive(Debug, Clone, Copy)]
struct LedgerRead<'a> {
    rows: &'a [Subscription],
    provenance: LedgerProvenance,
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
    ledger: LedgerRead<'_>,
) -> Dashboard {
    let LedgerRead {
        rows: ledger_rows,
        provenance: ledger_provenance,
    } = ledger;
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
    Dashboard {
        rows,
        aggregate_burn,
        fleet,
        collector_alert,
    }
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
            LedgerRead {
                rows: &[],
                provenance: LedgerProvenance::Off,
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
            LedgerRead {
                rows: &[],
                provenance: LedgerProvenance::Off,
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
            LedgerRead {
                rows: &[],
                provenance: LedgerProvenance::Off,
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
            LedgerRead {
                rows: &[],
                provenance: LedgerProvenance::Off,
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
}
