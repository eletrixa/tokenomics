//! `tok doctor`: read-only diagnostics for config, credentials, ccusage, and the overlay.
//!
//! Project: Tokenomics — monitor LLM subscription accounts (usage, limits, time-left) in a TUI
//! Module:  src/doctor.rs
//! Deps:    jiff; runner (ccusage/codex version), providers::claude (adapter/creds/overlay),
//!          providers::codex (app-server rate-limits probe)
//! Tested:  the parseable parts are covered in their own modules; this orchestrates + prints. The
//!          ledger diagnostics' pure helpers (`ledger_status_line`, `ledger_past_dated`,
//!          `ledger_join_divergence`) get their own inline tests (spec 017 §E acceptance 8); the
//!          per-row `verified_annotation` / `ledger_verified_lines` helpers get theirs (spec 018 §D
//!          acceptance 5).
//!
//! Key responsibilities:
//! - Per Claude account: config_dir exists; `.credentials.json` present + owner-only; ccusage active
//!   block; overlay reachability (only if opted-in AND active — an inactive account is labeled and
//!   its overlay probe skipped, spec 014 §D).
//! - Per Codex account: config_dir + `sessions/` present; `auth.json` present (existence only);
//!   `codex --version`; app-server reachability (only if opted-in AND active) (spec 013 §D).
//! - Cross-account (Claude only): `CLAUDE_CONFIG_DIR` round-trip distinctness + shared-`projects/`.
//! - Ledger plane (spec 017 §E): resolved path + provenance (`"ledger: not configured"` when `Off`);
//!   past-dated rows (`renews`/`paid_through` already behind `today`); failed-parse rows with reason;
//!   join divergence in both directions (config account with no ledger row, ledger row with no
//!   matching config account); per-matched-row `verified` annotation — current / outdated /
//!   human-entered (spec 018 §D). One read-only poll via the injectable `FileLedgerSource` — never
//!   written, never held past this run.
//! - Config divergence (spec 015 §B): flag when the RECORDED config path is newer than what the
//!   running collector loaded, but only once the collector has heartbeated a few local cadences past
//!   the edit without reloading (else the reload is simply pending). A persistent mismatch means the
//!   edit fails to parse/validate or the collector predates hot-reload. Also (§B2) flag when the
//!   recorded binary's on-disk mtime is newer than what the collector started with (rebuilt after
//!   start). The store is opened READ-ONLY (migration-free) so diagnosing never migrates it.
//!
//! Design constraints:
//! - Strictly READ-ONLY: runs ccusage/codex and (only for opted-in accounts) a single overlay probe;
//!   never writes anything, and never creates (or migrates) the store where none exists. No secret is
//!   ever printed (only token freshness + expiry); `auth.json` content is never read.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use jiff::tz::TimeZone;
use jiff::Timestamp;

use crate::config::Config;
use crate::domain::Provider;
use crate::error::AppResult;
use crate::ledger::{
    verified_current, FileLedgerSource, Ledger, LedgerProvenance, SubStatus, Subscription,
    LEDGER_ENV,
};
use crate::providers::claude::ccusage::CcusageInvocation;
use crate::providers::claude::overlay::{HttpUsageEndpoint, UsageEndpoint};
use crate::providers::claude::{creds, ClaudeAdapter};
use crate::providers::codex::rate_limits::{AppServerClient, RateLimitsSource};
use crate::providers::ProviderAdapter;
use crate::runner::{CommandSpec, Exec, Runner};
use crate::store::Store;

const DOCTOR_TIMEOUT_SECS: u64 = 30;

/// Run all diagnostics and print a report. Read-only.
pub async fn run_doctor(cfg: &Config) -> AppResult<()> {
    let invocation = CcusageInvocation::from_override(cfg.settings.ccusage_cmd.as_deref());
    println!("ccusage: {}", ccusage_version(&invocation).await);

    let adapter = ClaudeAdapter::new(Exec, invocation, Duration::from_secs(DOCTOR_TIMEOUT_SECS));
    let now = Timestamp::now();
    let now_ms = now.as_millisecond();
    let mut signatures: Vec<(String, Option<u64>)> = Vec::new();

    for account in &cfg.accounts {
        let overlay = if account.limits_overlay { "on" } else { "off" };
        let status = if account.active { "" } else { "  INACTIVE" };
        println!(
            "\n{} ({}) [{}]  overlay:{overlay}{status}",
            account.id, account.label, account.provider
        );
        report_config_dir(account);
        match account.provider {
            Provider::Claude => {
                report_credentials(account, now_ms);
                let signature = report_ccusage(&adapter, account, now).await;
                signatures.push((account.id.clone(), signature));
                // Never poll an account we've been told is dead — a cancelled subscription 429s the
                // overlay forever, so an inactive account skips the probe even when opted in (014 §D).
                if account.limits_overlay && account.active {
                    report_overlay(account, now_ms).await;
                } else if account.limits_overlay {
                    println!("  overlay:     skipped — account inactive");
                }
            }
            Provider::Codex => report_codex(account).await,
        }
    }

    report_distinctness(&signatures);
    report_shared_projects(cfg);
    report_config_divergence(cfg);
    report_ledger(cfg, now.to_zoned(TimeZone::system()).date());
    Ok(())
}

/// How many local cadences the collector must have heartbeated PAST the config file's mtime before a
/// divergence is real (not just a reload still pending). The collector reloads within one local tick,
/// so heartbeating this many cadences later without recording the edit is a demonstrable failure.
const DIVERGENCE_CADENCE_FACTOR: i64 = 3;

/// Flag a config the running collector never reloaded (§B) and a binary rebuilt after start (§B2).
/// Read-only and store-creation-free: opens the store READ-ONLY (skipping migration), reads what the
/// collector RECORDED loading, and stats those recorded paths — never doctor's own re-resolution, so
/// two different environments never compare two different files. Nothing new is printed when the store
/// is absent, the heartbeat row is missing, or the stamp columns are NULL.
fn report_config_divergence(cfg: &Config) {
    let Ok(store_path) = crate::paths::store_path() else {
        return;
    };
    if !store_path.exists() {
        return; // store absent — never create it just to diagnose
    }
    let Ok(store) = Store::open_readonly(&store_path) else {
        return;
    };
    let Some(stamp) = store.collector_stamp("collector") else {
        return; // no heartbeat row / pre-hot-reload collector — nothing recorded to compare
    };

    // §B2: the recorded binary rebuilt after the collector started (its on-disk mtime moved forward).
    let current_exe_mtime = stamp
        .exe_path
        .as_deref()
        .and_then(|p| crate::paths::file_mtime_ms(Path::new(p)));
    if let Some(message) = exe_staleness_hint(current_exe_mtime, stamp.exe_mtime) {
        println!("\n{message}");
    }

    // §B: the recorded config file is newer than what the collector loaded, and the collector has
    // demonstrably had time to reload it.
    let Some(recorded_path) = stamp.config_path.as_deref() else {
        return;
    };
    // A recorded path that differs from doctor's own resolution is itself worth telling the user —
    // doctor and the collector may run in different environments (spec 015 §B).
    if let Ok(own) = crate::paths::config_path() {
        if own.to_string_lossy() != recorded_path {
            println!(
                "\nnote: the running collector loaded {recorded_path}, but this environment resolves \
                 {} — comparing against the recorded path",
                own.display()
            );
        }
    }
    let Some(file_ms) = crate::paths::file_mtime_ms(Path::new(recorded_path)) else {
        return; // the recorded config path no longer stats — nothing to compare
    };
    let now_ms = Timestamp::now().as_millisecond();
    let heartbeat_ms = store
        .heartbeat_age("collector", now_ms)
        .ok()
        .flatten()
        .map(|age| now_ms - age);
    if let Some(message) = config_divergence_hint(
        file_ms,
        stamp.config_mtime,
        heartbeat_ms,
        cfg.settings.poll_local_secs,
    ) {
        println!("\n{message}");
    }
}

/// Pure: the divergence warning when the recorded config file (mtime `file_ms`) is newer than the
/// collector's recorded load (`recorded_ms`) AND the collector demonstrably had time to reload it —
/// its heartbeat (`heartbeat_ms`) has ticked at least `DIVERGENCE_CADENCE_FACTOR` local cadences past
/// the file's mtime. Inside that window the reload is simply pending, so this stays silent. `None`
/// also when nothing was recorded (row/column absent) or the file is not strictly newer.
fn config_divergence_hint(
    file_ms: i64,
    recorded_ms: Option<i64>,
    heartbeat_ms: Option<i64>,
    poll_local_secs: u64,
) -> Option<String> {
    let recorded = recorded_ms?;
    let heartbeat = heartbeat_ms?;
    if file_ms <= recorded {
        return None; // the collector loaded this config (or newer) — no divergence
    }
    let window = i64::try_from(poll_local_secs.saturating_mul(1000))
        .unwrap_or(i64::MAX)
        .saturating_mul(DIVERGENCE_CADENCE_FACTOR);
    if heartbeat.saturating_sub(file_ms) < window {
        return None; // still inside the reload-pending window — the reload is simply in flight
    }
    Some(
        "config divergence: tokenomics.toml is newer than the config the running collector loaded, \
         yet the collector has kept heartbeating past the edit without reloading it. Run \
         `tok validate` — the collector hot-reloads only a config that parses and validates; if the \
         file is clean, the running collector predates hot-reload and needs a restart (`tok collector`)."
            .to_string(),
    )
}

/// Pure: warn when the collector's on-disk binary is newer than the running one — rebuilt after start
/// (spec 015 §B2). `None` when either mtime is absent or the on-disk build is not strictly newer.
fn exe_staleness_hint(current_mtime: Option<i64>, recorded_mtime: Option<i64>) -> Option<String> {
    match (current_mtime, recorded_mtime) {
        (Some(current), Some(recorded)) if current > recorded => Some(
            "collector binary rebuilt after start — restart the collector (`tok collector`) so it \
             runs the new build."
                .to_string(),
        ),
        _ => None,
    }
}

// ── spec 017 §E: ledger diagnostics (provenance/path, freshness, failed-parse, join divergence) ──

/// Read the ledger once (path resolution → a single poll of the injectable `FileLedgerSource`) and
/// print: the resolved path + provenance (or `"not configured"`); past-dated rows; failed-parse rows
/// with reason; join divergence in both directions. Read-only — never writes the ledger, never holds
/// the reader past this call.
fn report_ledger(cfg: &Config, today: jiff::civil::Date) {
    let env_override = std::env::var(LEDGER_ENV).ok();
    let path =
        crate::ledger::resolve_path(env_override.as_deref(), cfg.settings.ledger_path.as_deref());
    let Some(path) = path else {
        println!("\n{}", ledger_status_line(None, LedgerProvenance::Off));
        return;
    };

    let mut ledger = Ledger::new();
    let mut source = FileLedgerSource::new(path.clone());
    ledger.poll(&mut source);

    println!("\n{}", ledger_status_line(Some(&path), ledger.provenance()));
    if ledger.provenance() == LedgerProvenance::Stale {
        if let Some(reason) = ledger.stale_reason() {
            println!("  ledger stale reason: {reason}");
        }
    }

    let past = ledger_past_dated(ledger.rows(), today);
    if !past.is_empty() {
        println!("  ledger past-dated: {}", past.join(", "));
    }
    for err in ledger.errors() {
        println!(
            "  ledger parse error ({}): {}",
            err.id.as_deref().unwrap_or("<no id>"),
            err.reason
        );
    }

    let config_ids: Vec<&str> = cfg.accounts.iter().map(|a| a.id.as_str()).collect();
    for line in ledger_verified_lines(&config_ids, ledger.rows(), today) {
        println!("{line}");
    }

    let (config_only, ledger_only) = ledger_join_divergence(&config_ids, ledger.rows());
    if !config_only.is_empty() {
        println!(
            "  ledger divergence: config account(s) with no ledger row: {}",
            config_only.join(", ")
        );
    }
    if !ledger_only.is_empty() {
        println!(
            "  ledger divergence: ledger row(s) with no matching config account: {}",
            ledger_only.join(", ")
        );
    }
}

/// The `"ledger: …"` status line: `"ledger: not configured"` when no path resolved (`Off`), else
/// `"ledger: {path} [{provenance}]"`. Pure.
fn ledger_status_line(path: Option<&Path>, provenance: LedgerProvenance) -> String {
    match path {
        None => "ledger: not configured".to_string(),
        Some(p) => format!("ledger: {} [{}]", p.display(), provenance_label(provenance)),
    }
}

/// Lowercase provenance label for the doctor status line (diagnostics text only — not TUI render).
fn provenance_label(provenance: LedgerProvenance) -> &'static str {
    match provenance {
        LedgerProvenance::Off => "off",
        LedgerProvenance::Fresh => "fresh",
        LedgerProvenance::Stale => "stale",
        LedgerProvenance::Missing => "missing",
    }
}

/// Ledger rows whose `renews` (active) or `paid_through` (cancelled) date has already passed
/// `today` — a freshness signal ("this ledger looks stale, an agent should renew/reconcile it").
/// Pure; returns row ids in ledger order.
fn ledger_past_dated(rows: &[Subscription], today: jiff::civil::Date) -> Vec<String> {
    rows.iter()
        .filter(|r| {
            let relevant = match r.status {
                SubStatus::Active => r.renews,
                SubStatus::Cancelled => r.paid_through,
            };
            relevant.is_some_and(|d| d < today)
        })
        .map(|r| r.id.clone())
        .collect()
}

/// Per-row verified annotation lines for `tok doctor`'s ledger section (spec 018 §D), one per row
/// matched to a config account (§C join) — an orphan ledger row is already covered by the divergence
/// report below, so it isn't repeated here. Pure; returns lines in ledger order.
fn ledger_verified_lines(
    config_ids: &[&str],
    rows: &[Subscription],
    today: jiff::civil::Date,
) -> Vec<String> {
    let config_id_set: std::collections::HashSet<&str> = config_ids.iter().copied().collect();
    rows.iter()
        .filter(|r| config_id_set.contains(r.id.as_str()))
        .map(|r| {
            format!(
                "  ledger verified ({}): {}",
                r.id,
                verified_annotation(r, today)
            )
        })
        .collect()
}

/// One row's verified annotation: `"verified <date> (current)"`, `"verified <date> (outdated —
/// before current period)"`, or `"human-entered (no verified)"` (spec 018 §D). Shares
/// [`crate::ledger::verified_current`] with the TUI's pill math so both stay in lockstep.
fn verified_annotation(sub: &Subscription, today: jiff::civil::Date) -> String {
    let Some(verified) = sub.verified else {
        return "human-entered (no verified)".to_string();
    };
    if verified_current(sub, today) {
        format!("verified {verified} (current)")
    } else {
        format!("verified {verified} (outdated — before current period)")
    }
}

/// Join divergence in both directions (spec 017 §C/§E): `(config ids with no ledger row, ledger ids
/// with no matching config account)`. Exact-match join only (mirrors `ledger::find`). Pure.
fn ledger_join_divergence(
    config_ids: &[&str],
    ledger_rows: &[Subscription],
) -> (Vec<String>, Vec<String>) {
    let ledger_ids: std::collections::HashSet<&str> =
        ledger_rows.iter().map(|r| r.id.as_str()).collect();
    let config_only: Vec<String> = config_ids
        .iter()
        .filter(|id| !ledger_ids.contains(*id))
        .map(|id| (*id).to_string())
        .collect();

    let config_id_set: std::collections::HashSet<&str> = config_ids.iter().copied().collect();
    let ledger_only: Vec<String> = ledger_rows
        .iter()
        .filter(|r| !config_id_set.contains(r.id.as_str()))
        .map(|r| r.id.clone())
        .collect();

    (config_only, ledger_only)
}

/// Codex diagnostics for one account (read-only): `sessions/` present, `auth.json` present
/// (existence ONLY — content never read, auth stays inside the `codex` binary), `codex --version`,
/// and — only when active AND opted-in — an app-server reachability probe (spec 013 §D). Attribution
/// is the account's `CODEX_HOME` (its `config_dir`), never the logs.
async fn report_codex(account: &crate::domain::Account) {
    let sessions_marker = if account.config_dir.join("sessions").is_dir() {
        "present"
    } else {
        "MISSING (idle until Codex writes a rollout)"
    };
    println!("  sessions/:   {sessions_marker}");
    let auth_marker = if account.config_dir.join("auth.json").exists() {
        "present"
    } else {
        "MISSING (run `codex login`)"
    };
    println!("  auth.json:   {auth_marker}");
    println!("  codex:       {}", codex_version().await);
    // Mirror the Claude overlay gate: probe only an opted-in, active account.
    if account.limits_overlay && account.active {
        report_codex_overlay(account).await;
    } else if account.limits_overlay {
        println!("  overlay:     skipped — account inactive");
    }
}

/// Best-effort `codex --version` string (read-only), via the bounded argv runner seam.
async fn codex_version() -> String {
    let spec = CommandSpec {
        program: "codex".to_string(),
        args: vec!["--version".to_string()],
        env: Vec::new(),
        timeout: Duration::from_secs(DOCTOR_TIMEOUT_SECS),
    };
    match Exec.run(&spec).await {
        Ok(bytes) => String::from_utf8_lossy(&bytes).trim().to_string(),
        Err(e) => format!("unavailable ({e})"),
    }
}

/// Probe `codex app-server account/rateLimits/read` for an opted-in Codex account (spec 013 §C).
/// Read-only; reports reachable/error only — the raw response is NEVER dumped (auth could surface).
async fn report_codex_overlay(account: &crate::domain::Account) {
    let client = AppServerClient::new(Duration::from_secs(DOCTOR_TIMEOUT_SECS));
    match client.fetch(&account.config_dir).await {
        Ok(_) => println!("  overlay:     reachable (codex app-server answered rateLimits)"),
        Err(e) => println!("  overlay:     {e}"),
    }
}

/// Report whether any accounts share one `projects/` usage-log directory — the precise, deterministic
/// root cause of identical per-account totals (each `<config_dir>/projects` resolving to the same
/// real path, e.g. all symlinked to a shared `~/.claude/projects`). Read-only (canonicalize only).
fn report_shared_projects(cfg: &Config) {
    // Claude-only: the shared-`projects/` symlink lane is a Claude ccusage concept; a Codex account
    // has its own sessions tree under `CODEX_HOME` and shares no `projects/` dir (spec 013 §D).
    let resolved: Vec<(String, PathBuf)> = cfg
        .accounts
        .iter()
        .filter(|a| a.provider == Provider::Claude)
        .filter_map(|a| {
            std::fs::canonicalize(a.config_dir.join("projects"))
                .ok()
                .map(|real| (a.id.clone(), real))
        })
        .collect();
    let groups = shared_projects_groups(&resolved);
    if groups.is_empty() {
        println!("\nprojects/ isolation: distinct ✓ (each account has its own usage logs)");
        return;
    }
    println!("\nprojects/ isolation: SHARED ✗ — per-account usage attribution disabled:");
    for group in &groups {
        println!("  {} → one shared projects/ dir", group.join(", "));
    }
    println!(
        "  give each account its own real projects/ (not a symlink to shared) to attribute usage; \
         until then the aggregate burn bar is the reliable signal"
    );
}

/// Pure: group account ids by their already-resolved `projects/` real path, returning only the
/// groups shared by ≥2 accounts (each such group cannot be attributed apart).
fn shared_projects_groups(resolved: &[(String, PathBuf)]) -> Vec<Vec<String>> {
    let mut by_real: BTreeMap<&PathBuf, Vec<String>> = BTreeMap::new();
    for (id, real) in resolved {
        by_real.entry(real).or_default().push(id.clone());
    }
    by_real.into_values().filter(|ids| ids.len() >= 2).collect()
}

fn report_config_dir(account: &crate::domain::Account) {
    let marker = if account.config_dir.is_dir() {
        "exists"
    } else {
        "MISSING"
    };
    println!("  config_dir:  {} [{marker}]", account.config_dir.display());
}

fn report_credentials(account: &crate::domain::Account, now_ms: i64) {
    match creds::read_token(&account.config_dir) {
        Ok(token) => {
            let state = if token.is_warm(now_ms) {
                "warm"
            } else {
                "EXPIRED"
            };
            let expires = Timestamp::from_millisecond(token.expires_at_ms)
                .map_or_else(|_| "?".to_string(), |t| t.to_string());
            println!("  credentials: present, owner-only, {state} (expires {expires})");
        }
        Err(e) => println!("  credentials: {e}"),
    }
}

/// Run ccusage for this account and print a one-line summary; return a distinctness signature.
async fn report_ccusage<R: Runner>(
    adapter: &ClaudeAdapter<R>,
    account: &crate::domain::Account,
    now: Timestamp,
) -> Option<u64> {
    match adapter.collect(account, now).await {
        Ok(Some(snapshot)) => {
            let ends = snapshot
                .window
                .as_ref()
                .map_or_else(String::new, |w| format!(", ends {}", w.end));
            println!(
                "  ccusage:     active block, {} total tokens{ends}",
                snapshot.total_tokens
            );
            Some(snapshot.total_tokens)
        }
        Ok(None) => {
            println!("  ccusage:     no active block (idle)");
            None
        }
        Err(e) => {
            println!("  ccusage:     error: {e}");
            None
        }
    }
}

/// Probe the overlay for an opted-in account (single GET). Read-only; the token is never printed.
async fn report_overlay(account: &crate::domain::Account, now_ms: i64) {
    let token = match creds::read_token(&account.config_dir) {
        Ok(token) => token,
        Err(e) => {
            println!("  overlay:     skipped — {e}");
            return;
        }
    };
    if !token.is_warm(now_ms) {
        println!("  overlay:     skipped — token stale (open Claude to refresh)");
        return;
    }
    let endpoint = match HttpUsageEndpoint::new() {
        Ok(endpoint) => endpoint,
        Err(e) => {
            println!("  overlay:     client error: {e}");
            return;
        }
    };
    match endpoint.fetch(token.access_token()).await {
        Ok(bytes) => println!("  overlay:     reachable (HTTP 200, {} bytes)", bytes.len()),
        Err(e) => println!("  overlay:     {e}"),
    }
}

/// Report whether the accounts' active blocks look distinct (the `CLAUDE_CONFIG_DIR` gate).
fn report_distinctness(signatures: &[(String, Option<u64>)]) {
    let present: Vec<u64> = signatures.iter().filter_map(|(_, s)| *s).collect();
    if present.len() < 2 {
        println!("\nCLAUDE_CONFIG_DIR round-trip: n/a (need ≥2 active accounts to compare)");
        return;
    }
    let mut sorted = present.clone();
    sorted.sort_unstable();
    let distinct = sorted.windows(2).all(|w| w[0] != w[1]);
    if distinct {
        println!("\nCLAUDE_CONFIG_DIR round-trip: distinct ✓ (per-account attribution works)");
    } else {
        println!(
            "\nCLAUDE_CONFIG_DIR round-trip: IDENTICAL ✗ — accounts share an active block; \
             attribution may be broken (consider the direct-JSONL fallback)"
        );
    }
}

/// Best-effort ccusage version string (read-only).
async fn ccusage_version(invocation: &CcusageInvocation) -> String {
    let mut args = invocation.prefix_args.clone();
    args.push("--version".to_string());
    let spec = CommandSpec {
        program: invocation.program.clone(),
        args,
        env: Vec::new(),
        timeout: Duration::from_secs(DOCTOR_TIMEOUT_SECS),
    };
    match Exec.run(&spec).await {
        Ok(bytes) => String::from_utf8_lossy(&bytes).trim().to_string(),
        Err(e) => format!("unavailable ({e})"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair(id: &str, path: &str) -> (String, PathBuf) {
        (id.to_string(), PathBuf::from(path))
    }

    #[test]
    fn shared_projects_groups_flags_only_shared_realpaths() {
        let resolved = vec![
            pair("alpha", "/home/user/.claude/projects"),
            pair("bravo", "/home/user/.claude/projects"),
            pair("charlie", "/home/user/.claude/projects"),
            pair("delta", "/home/user/.claude-acct/delta/projects"), // its own real dir
        ];
        let groups = shared_projects_groups(&resolved);
        assert_eq!(groups.len(), 1, "one shared group expected: {groups:?}");
        // ids keep insertion order within a group (BTreeMap sorts keys, not the grouped values).
        assert_eq!(groups[0], vec!["alpha", "bravo", "charlie"]);
    }

    #[test]
    fn shared_projects_groups_empty_when_all_distinct() {
        let resolved = vec![pair("a", "/dirs/a/projects"), pair("b", "/dirs/b/projects")];
        assert!(shared_projects_groups(&resolved).is_empty());
    }

    #[test]
    fn divergence_hint_gated_on_recorded_newness_and_the_reload_window() {
        let poll = 10; // reload-pending window = 3 × 10s = 30s
                       // File newer than recorded AND the heartbeat 31s past the file ⇒ demonstrable failure → warn.
        assert!(
            config_divergence_hint(1_000_000, Some(900_000), Some(1_031_000), poll).is_some(),
            "past the window → warn"
        );
        // Inside the reload-pending window (heartbeat only 10s past the file) ⇒ silent (pending).
        assert!(
            config_divergence_hint(1_000_000, Some(900_000), Some(1_010_000), poll).is_none(),
            "inside the window → silent"
        );
        // Exact boundary: heartbeat exactly `window` (30_000ms) past the file. The implementation
        // uses `< window → silent`, so `== window` falls through to the WARNING — the safer side:
        // surface the divergence the moment the reload-pending window closes, not one tick later.
        assert!(
            config_divergence_hint(1_000_000, Some(900_000), Some(1_030_000), poll).is_some(),
            "exactly at the window boundary → warn (inclusive)"
        );
        // One ms short of the boundary ⇒ still reload-pending → silent.
        assert!(
            config_divergence_hint(1_000_000, Some(900_000), Some(1_029_999), poll).is_none(),
            "one ms inside the window → silent"
        );
        // File equal to recorded ⇒ the collector loaded it → silent.
        assert!(
            config_divergence_hint(1_000_000, Some(1_000_000), Some(9_999_999), poll).is_none(),
            "equal → no warn"
        );
        // File older than recorded ⇒ silent.
        assert!(
            config_divergence_hint(900_000, Some(1_000_000), Some(9_999_999), poll).is_none(),
            "older → no warn"
        );
        // Nothing recorded (row/column absent) ⇒ silent.
        assert!(
            config_divergence_hint(1_000_000, None, Some(9_999_999), poll).is_none(),
            "no recorded config_mtime → no warn"
        );
        // No heartbeat at all (collector never wrote one) ⇒ can't prove it had time → silent.
        assert!(
            config_divergence_hint(1_000_000, Some(900_000), None, poll).is_none(),
            "no heartbeat → no warn"
        );
    }

    #[test]
    fn exe_staleness_hint_only_when_on_disk_build_is_newer() {
        assert!(
            exe_staleness_hint(Some(200), Some(100)).is_some(),
            "rebuilt after start → warn"
        );
        assert!(
            exe_staleness_hint(Some(100), Some(100)).is_none(),
            "same build → no warn"
        );
        assert!(
            exe_staleness_hint(Some(50), Some(100)).is_none(),
            "older on disk → no warn"
        );
        assert!(
            exe_staleness_hint(None, Some(100)).is_none(),
            "exe unreadable now → no warn"
        );
        assert!(
            exe_staleness_hint(Some(200), None).is_none(),
            "nothing recorded → no warn"
        );
    }

    // ── spec 017 §E (acceptance 8): ledger diagnostics ─────────────────────────────────────────
    // `LedgerProvenance`/`SubStatus`/`Subscription` are already in scope via `use super::*` above.

    fn ledger_row(id: &str, status: SubStatus) -> Subscription {
        Subscription {
            id: id.to_string(),
            status,
            purchased: None,
            renews: None,
            cancelled_on: None,
            paid_through: None,
            verified: None,
        }
    }

    #[test]
    fn ledger_status_line_reports_not_configured_when_off() {
        assert_eq!(
            ledger_status_line(None, LedgerProvenance::Off),
            "ledger: not configured"
        );
    }

    #[test]
    fn ledger_status_line_names_the_resolved_path_when_configured() {
        let line = ledger_status_line(
            Some(Path::new("/synthetic/subscriptions.toml")),
            LedgerProvenance::Fresh,
        );
        assert!(
            line.contains("/synthetic/subscriptions.toml"),
            "must name the resolved path: {line}"
        );
    }

    #[test]
    fn ledger_past_dated_flags_past_renews_and_past_paid_through() {
        let today = jiff::civil::date(2026, 7, 18);
        let mut stale_active = ledger_row("claude-alpha", SubStatus::Active);
        stale_active.renews = Some(jiff::civil::date(2026, 7, 10)); // 8 days ago
        let mut stale_cancelled = ledger_row("claude-bravo", SubStatus::Cancelled);
        stale_cancelled.paid_through = Some(jiff::civil::date(2026, 7, 1));
        let mut fresh = ledger_row("claude-charlie", SubStatus::Active);
        fresh.renews = Some(jiff::civil::date(2026, 8, 1)); // future

        let rows = vec![stale_active, stale_cancelled, fresh];
        let past = ledger_past_dated(&rows, today);
        assert_eq!(
            past,
            vec!["claude-alpha".to_string(), "claude-bravo".to_string()],
            "past: {past:?}"
        );
    }

    #[test]
    fn ledger_join_divergence_flags_both_directions() {
        let config_ids = vec!["claude-alpha", "claude-orphan-config"];
        let ledger_rows = vec![
            ledger_row("claude-alpha", SubStatus::Active),
            ledger_row("claude-orphan-ledger", SubStatus::Cancelled),
        ];
        let (config_only, ledger_only) = ledger_join_divergence(&config_ids, &ledger_rows);
        assert_eq!(
            config_only,
            vec!["claude-orphan-config".to_string()],
            "config account with no ledger row: {config_only:?}"
        );
        assert_eq!(
            ledger_only,
            vec!["claude-orphan-ledger".to_string()],
            "ledger row with no matching config account: {ledger_only:?}"
        );
    }

    #[test]
    fn ledger_join_divergence_empty_when_every_id_matches() {
        let config_ids = vec!["claude-alpha"];
        let ledger_rows = vec![ledger_row("claude-alpha", SubStatus::Active)];
        let (config_only, ledger_only) = ledger_join_divergence(&config_ids, &ledger_rows);
        assert!(config_only.is_empty());
        assert!(ledger_only.is_empty());
    }

    // ── spec 018 §D (acceptance 5): per-row verified annotation ────────────────────────────────

    #[test]
    fn verified_annotation_human_entered_when_absent() {
        let sub = ledger_row("claude-alpha", SubStatus::Active);
        let today = jiff::civil::date(2026, 7, 18);
        assert_eq!(
            verified_annotation(&sub, today),
            "human-entered (no verified)"
        );
    }

    #[test]
    fn verified_annotation_current_when_verified_on_or_after_purchased() {
        let mut sub = ledger_row("claude-alpha", SubStatus::Active);
        sub.purchased = Some(jiff::civil::date(2026, 7, 1));
        sub.verified = Some(jiff::civil::date(2026, 7, 18));
        let today = jiff::civil::date(2026, 7, 18);
        assert_eq!(
            verified_annotation(&sub, today),
            "verified 2026-07-18 (current)"
        );
    }

    #[test]
    fn verified_annotation_outdated_when_verified_before_purchased() {
        let mut sub = ledger_row("claude-alpha", SubStatus::Active);
        sub.purchased = Some(jiff::civil::date(2026, 7, 1));
        sub.verified = Some(jiff::civil::date(2026, 6, 1));
        let today = jiff::civil::date(2026, 7, 18);
        assert_eq!(
            verified_annotation(&sub, today),
            "verified 2026-06-01 (outdated — before current period)"
        );
    }

    #[test]
    fn ledger_verified_lines_only_covers_rows_matched_to_a_config_account() {
        let today = jiff::civil::date(2026, 7, 18);
        let mut matched = ledger_row("claude-alpha", SubStatus::Active);
        matched.verified = Some(jiff::civil::date(2026, 7, 18));
        let orphan = ledger_row("claude-orphan-ledger", SubStatus::Active);
        let rows = vec![matched, orphan];
        let config_ids = vec!["claude-alpha"];
        let lines = ledger_verified_lines(&config_ids, &rows, today);
        assert_eq!(
            lines.len(),
            1,
            "only the matched row is annotated: {lines:?}"
        );
        assert!(
            lines[0].contains("claude-alpha") && lines[0].contains("current"),
            "{lines:?}"
        );
    }
}
