//! Tokenomics — CLI entrypoint and subcommand dispatch.
//!
//! Project: Tokenomics — monitor LLM subscription accounts (usage, limits, time-left) in a TUI
//! Module:  src/main.rs
//! Deps:    config, domain, error, providers, runner; tokio (explicit runtime), jiff, serde_json
//! Tested:  tests/cli.rs (black-box CLI); inline `#[cfg(test)]` for `collect_once`'s inactive skip
//!          (spec 014 §D — doesn't need a real `ccusage`, so it stays a fast unit test here)
//!
//! Key responsibilities:
//! - Parse `tok` subcommands: (default) tui | init | validate | accounts | once | collector | doctor.
//! - `init`: write the embedded starter `tokenomics.example.toml` to the config path (never clobbers).
//! - `once`: collect one snapshot per account (dispatched to the Claude/Codex/Zai/Gemini adapter
//!   by provider) and print it (human/JSON).
//!
//! Design constraints:
//! - `unsafe_code = "forbid"` is crate policy (Cargo `[lints]`); never reach for unsafe.
//! - Dispatch is hand-rolled (house style; no clap). Unimplemented commands exit 2 until their wave lands.
//! - The only I/O the CLI drives is at the command edges; the async runtime is built explicitly here.

mod alerts;
mod collector;
mod config;
mod doctor;
mod domain;
mod error;
mod format;
mod ledger;
mod paths;
mod providers;
mod runner;
mod store;
mod tui;

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use jiff::Timestamp;
use serde::Serialize;

use config::Config;
use domain::{Account, Limit, Provider, UsageSnapshot};
use error::AppResult;
use format::{
    format_cost, format_pct, format_reset, format_tokens, merge_limits, provenance_label,
    severity_label,
};
use providers::claude::ccusage::{derive_session_limit, CcusageInvocation};
use providers::claude::overlay::HttpUsageEndpoint;
use providers::claude::ClaudeAdapter;
use providers::codex::rate_limits::AppServerClient;
use providers::codex::CodexAdapter;
use providers::gemini::GeminiAdapter;
use providers::grok::GrokAdapter;
use providers::zai::quota::HttpQuotaEndpoint;
use providers::zai::ZaiAdapter;
use providers::{ProviderAdapter, ProviderRegistry};
use runner::Exec;
use store::Store;

/// Per-account ccusage timeout for one-shot collection.
const CCUSAGE_TIMEOUT_SECS: u64 = 30;
/// Hard cap on one `codex app-server` rate-limits exchange (spawn → RPC → read). Consistent with the
/// overlay per-fetch budget; the collector's own tick budget backstops it (spec 013 §C/§D).
const CODEX_APP_SERVER_TIMEOUT_SECS: u64 = 10;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--help" | "-h" | "help") => {
            print_help();
            ExitCode::SUCCESS
        }
        Some("--version" | "-V" | "version") => {
            println!("tok {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Some("init") => cmd_init(),
        Some("validate") => cmd_validate(),
        Some("accounts") => cmd_accounts(),
        Some("once") => cmd_once(args.iter().any(|a| a == "--json")),
        Some("collector") => cmd_collector(args.iter().any(|a| a == "--once")),
        Some("doctor") => cmd_doctor(),
        Some(other) => {
            eprintln!("tok: unknown command '{other}' (try `tok --help`)");
            ExitCode::from(2)
        }
        None => cmd_tui(),
    }
}

/// Starter `tokenomics.toml`, embedded from the repo-root example so the file `tok init` writes and
/// the tracked example (and the README's fenced block, which mirrors it) can never drift.
const EXAMPLE_CONFIG: &str = include_str!("../tokenomics.example.toml");

/// Load config, mapping any failure to a printed error + exit code 2. A missing file also earns a
/// `tok init` hint — it's the fresh-machine case, and every config-loading command routes here.
fn load_config() -> Result<Config, ExitCode> {
    Config::load().map_err(|e| {
        eprintln!("tok: {e}");
        if matches!(&e, error::AppError::ConfigRead { source, .. }
            if source.kind() == std::io::ErrorKind::NotFound)
        {
            eprintln!("  run `tok init` to create one");
        }
        ExitCode::from(2)
    })
}

/// `tok init`: write the starter config to the resolved config path (creating its parent dir), or
/// refuse (exit 1) if one already exists so a re-run can never clobber a real config (spec 016).
fn cmd_init() -> ExitCode {
    let path = match paths::config_path() {
        Ok(path) => path,
        Err(e) => {
            eprintln!("tok: {e}");
            return ExitCode::from(2);
        }
    };
    if path.exists() {
        eprintln!(
            "tok: config already exists at {} — refusing to overwrite (edit it, or remove it first)",
            path.display()
        );
        return ExitCode::from(1);
    }
    // `config_path()` never creates the parent (unlike the store path); make it here. A bare relative
    // filename has an empty parent (the cwd already exists) — nothing to create.
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        if let Err(e) = std::fs::create_dir_all(parent) {
            eprintln!("tok: cannot create config dir {}: {e}", parent.display());
            return ExitCode::from(2);
        }
    }
    if let Err(e) = std::fs::write(&path, EXAMPLE_CONFIG) {
        eprintln!("tok: cannot write config {}: {e}", path.display());
        return ExitCode::from(2);
    }
    println!("tok: wrote starter config → {}", path.display());
    println!("edit it to add your accounts, then `tok validate`, `tok collector`, and `tok`.");
    ExitCode::SUCCESS
}

/// Load config AND refuse to run if it has validation errors (e.g. a duplicate account `id`, which
/// is the store primary key and the sole attribution handle — a dup would silently merge two
/// accounts). Used by the commands that act on accounts; `tok validate` reports findings in detail.
fn load_valid_config() -> Result<Config, ExitCode> {
    let cfg = load_config()?;
    let findings = config::validate(&cfg);
    if findings.is_empty() {
        return Ok(cfg);
    }
    for finding in &findings {
        eprintln!("{finding}");
    }
    eprintln!("tok: refusing to run — fix tokenomics.toml (see `tok validate`)");
    Err(ExitCode::from(1))
}

fn cmd_validate() -> ExitCode {
    let cfg = match load_config() {
        Ok(cfg) => cfg,
        Err(code) => return code,
    };
    let mut findings = config::validate(&cfg);
    findings.extend(config::validate_environment(&cfg));

    for finding in &findings {
        println!("{finding}");
    }
    if findings.is_empty() {
        println!("✓ {} account(s), no errors", cfg.accounts.len());
        ExitCode::SUCCESS
    } else {
        eprintln!(
            "✗ {} error(s) across {} account(s)",
            findings.len(),
            cfg.accounts.len()
        );
        ExitCode::from(1)
    }
}

fn cmd_accounts() -> ExitCode {
    let cfg = match load_config() {
        Ok(cfg) => cfg,
        Err(code) => return code,
    };
    for account in &cfg.accounts {
        let overlay = if account.limits_overlay {
            "overlay:on"
        } else {
            "overlay:off"
        };
        let inactive = if account.active { "" } else { "  (inactive)" };
        let config_dir = account
            .config_dir
            .as_deref()
            .map_or_else(|| "-".to_string(), |d| d.display().to_string());
        println!(
            "{}  {}  [{}]  {}  {overlay}{inactive}",
            account.id, account.label, account.provider, config_dir
        );
    }
    ExitCode::SUCCESS
}

/// One account's one-shot collection result (human-printed or serialized for `--json`).
#[derive(Debug, Serialize)]
struct OnceRecord {
    account: String,
    label: String,
    provider: Provider,
    #[serde(skip_serializing_if = "Option::is_none")]
    snapshot: Option<UsageSnapshot>,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_limit: Option<Limit>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// Build the single-threaded async runtime used by `once`/`collector`, or a printed exit code.
fn build_runtime() -> Result<tokio::runtime::Runtime, ExitCode> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| {
            eprintln!("tok: failed to start async runtime: {e}");
            ExitCode::from(2)
        })
}

fn cmd_once(as_json: bool) -> ExitCode {
    let cfg = match load_valid_config() {
        Ok(cfg) => cfg,
        Err(code) => return code,
    };
    let runtime = match build_runtime() {
        Ok(runtime) => runtime,
        Err(code) => return code,
    };
    let records = runtime.block_on(collect_once(&cfg));

    if as_json {
        match serde_json::to_string_pretty(&records) {
            Ok(text) => println!("{text}"),
            Err(e) => {
                eprintln!("tok: failed to serialize snapshots: {e}");
                return ExitCode::from(2);
            }
        }
    } else {
        print_once_human(&records);
    }
    ExitCode::SUCCESS
}

/// Collect every ACTIVE account once, sequentially (a one-shot; latency is not critical here). An
/// inactive account is unmonitored (spec 014 §B/D): it produces no record at all, so `--json` output
/// contains only active accounts.
async fn collect_once(cfg: &Config) -> Vec<OnceRecord> {
    let invocation = CcusageInvocation::from_override(cfg.settings.ccusage_cmd.as_deref());
    let claude = ClaudeAdapter::new(Exec, invocation, Duration::from_secs(CCUSAGE_TIMEOUT_SECS));
    let codex = CodexAdapter::new();
    let zai = ZaiAdapter::new();
    let gemini = GeminiAdapter::new();
    let grok = GrokAdapter::new();
    let now = Timestamp::now();
    let (warn, crit) = (cfg.settings.warn_pct, cfg.settings.crit_pct);
    let mut records = Vec::with_capacity(cfg.accounts.len());
    for account in cfg.accounts.iter().filter(|a| a.active) {
        // All adapters run through the same `collect_record`; a windowless snapshot (Codex/Gemini)
        // or an always-idle one (zai) naturally yields no derived session limit (specs 013, 019
        // §C, 020 §C).
        let record = match account.provider {
            Provider::Claude => collect_record(&claude, account, now, warn, crit).await,
            Provider::Codex => collect_record(&codex, account, now, warn, crit).await,
            Provider::Zai => collect_record(&zai, account, now, warn, crit).await,
            Provider::Gemini => collect_record(&gemini, account, now, warn, crit).await,
            Provider::Grok => collect_record(&grok, account, now, warn, crit).await,
        };
        records.push(record);
    }
    records
}

/// Collect one account into an `OnceRecord`, deriving its session limit when a window is active
/// (Claude); a windowless provider (Codex) yields `None` from `derive_session_limit` unchanged.
async fn collect_record<A: ProviderAdapter>(
    adapter: &A,
    account: &Account,
    now: Timestamp,
    warn_pct: f64,
    crit_pct: f64,
) -> OnceRecord {
    let base = |snapshot, session_limit, error| OnceRecord {
        account: account.id.clone(),
        label: account.label.clone(),
        provider: account.provider,
        snapshot,
        session_limit,
        error,
    };
    match adapter.collect(account, now).await {
        Ok(Some(snapshot)) => {
            let limit = derive_session_limit(&snapshot, now, warn_pct, crit_pct);
            base(Some(snapshot), limit, None)
        }
        Ok(None) => base(None, None, None),
        Err(e) => base(None, None, Some(e.to_string())),
    }
}

fn print_once_human(records: &[OnceRecord]) {
    for record in records {
        println!("{} [{}]", record.label, record.provider);
        if let Some(err) = &record.error {
            println!("  error: {err}");
            continue;
        }
        let Some(snapshot) = &record.snapshot else {
            println!("  idle — no active 5h block");
            continue;
        };
        println!(
            "  tokens: {} total (in {} · out {} · cache-r {} · cache-c {})",
            format_tokens(snapshot.total_tokens),
            format_tokens(snapshot.input),
            format_tokens(snapshot.output),
            format_tokens(snapshot.cache_read),
            format_tokens(snapshot.cache_creation)
        );
        if let Some(cost) = snapshot.cost_notional {
            println!("  cost:   {}", format_cost(cost));
        }
        if let Some(limit) = &record.session_limit {
            println!(
                "  window: session {} [{}] · resets {} · {}",
                format_pct(limit.utilization_pct),
                severity_label(limit.severity),
                format_reset(&limit.resets_at, snapshot.collected_at),
                provenance_label(limit.source),
            );
        }
    }
}

fn cmd_collector(once: bool) -> ExitCode {
    let cfg = match load_valid_config() {
        Ok(cfg) => cfg,
        Err(code) => return code,
    };
    let runtime = match build_runtime() {
        Ok(runtime) => runtime,
        Err(code) => return code,
    };
    if once {
        collector_single_pass(&cfg, &runtime)
    } else {
        collector_daemon(&cfg, &runtime)
    }
}

/// `tok collector --once`: one collection pass into the store, with a printed read-back summary.
fn collector_single_pass(cfg: &Config, runtime: &tokio::runtime::Runtime) -> ExitCode {
    let records = runtime.block_on(collect_once(cfg));
    match persist_and_report(cfg, &records) {
        Ok(path) => {
            println!(
                "collector: wrote {} account(s) → {}",
                records.len(),
                path.display()
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("tok: {e}");
            ExitCode::from(2)
        }
    }
}

/// `tok collector`: run the 24/7 cadence loop until SIGINT/SIGTERM.
fn collector_daemon(cfg: &Config, runtime: &tokio::runtime::Runtime) -> ExitCode {
    let path = match paths::store_path() {
        Ok(path) => path,
        Err(e) => {
            eprintln!("tok: {e}");
            return ExitCode::from(2);
        }
    };
    let result = runtime.block_on(async {
        let store = Store::open(&path)?;
        let invocation = CcusageInvocation::from_override(cfg.settings.ccusage_cmd.as_deref());
        let claude = ClaudeAdapter::new(Exec, invocation, Duration::from_secs(CCUSAGE_TIMEOUT_SECS));
        let adapter = ProviderRegistry {
            claude,
            codex: CodexAdapter::new(),
            zai: ZaiAdapter::new(),
            gemini: GeminiAdapter::new(),
            grok: GrokAdapter::new(),
        };
        let endpoint = HttpUsageEndpoint::new()?;
        let rate_source = AppServerClient::new(Duration::from_secs(CODEX_APP_SERVER_TIMEOUT_SECS));
        let zai_endpoint = HttpQuotaEndpoint::new()?;
        let overlay_on = cfg.accounts.iter().filter(|a| a.limits_overlay).count();
        println!(
            "collector: watching {} account(s) every {}s ({overlay_on} with overlay) → {} (Ctrl-C to stop)",
            cfg.accounts.len(),
            cfg.settings.poll_local_secs,
            path.display()
        );
        collector::run_collector(
            cfg,
            collector::FileConfigSource::new(),
            adapter,
            endpoint,
            rate_source,
            zai_endpoint,
            store,
            collector::shutdown_signal(),
        )
        .await
    });
    match result {
        Ok(()) => {
            println!("collector: stopped");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("tok: {e}");
            ExitCode::from(2)
        }
    }
}

/// Persist each collected record to the store, then print a per-account read-back summary.
/// Returns the store path on success. (The Wave 5 daemon wraps this in a cadence loop.)
fn persist_and_report(cfg: &Config, records: &[OnceRecord]) -> AppResult<PathBuf> {
    let path = paths::store_path()?;
    let store = Store::open(&path)?;
    store.upsert_accounts(&cfg.accounts)?;
    // Stamp the config + binary this pass loaded (subsumes the liveness beat) so `tok doctor` can
    // compare them to the file's/binary's current mtime and flag a divergence/rebuild (spec 015 §B).
    let config_path = paths::config_path().ok().map(|p| p.display().to_string());
    let (exe_path, exe_mtime) = paths::current_exe_stamp();
    store.record_collector_stamp(
        "collector",
        std::process::id(),
        config_path.as_deref(),
        paths::config_mtime_ms(),
        exe_path.as_deref(),
        exe_mtime,
    )?;

    for record in records {
        if let Some(snapshot) = &record.snapshot {
            store.insert_snapshot(snapshot)?;
            if let Some(limit) = &record.session_limit {
                let current = store.latest_limits(&snapshot.account_id)?;
                let merged = merge_limits(current, vec![limit.clone()]);
                store.set_limits(&snapshot.account_id, &merged, snapshot.collected_at)?;
            }
        }
    }
    for record in records {
        report_stored(&store, record)?;
    }
    Ok(path)
}

/// Print one account's last-good snapshot + limit as read back from the store.
fn report_stored(store: &Store, record: &OnceRecord) -> AppResult<()> {
    println!("{} [{}]", record.label, record.provider);
    match store.latest_snapshot(&record.account)? {
        Some(snapshot) => {
            let history = store.burn_history(&record.account, 64)?;
            println!(
                "  stored: {} total · {} point(s) of history",
                format_tokens(snapshot.total_tokens),
                history.len()
            );
            if let Some(limit) = store.latest_limits(&record.account)?.first() {
                println!(
                    "  limit:  session {} [{}] · resets {} · {}",
                    format_pct(limit.utilization_pct),
                    severity_label(limit.severity),
                    format_reset(&limit.resets_at, snapshot.collected_at),
                    provenance_label(limit.source),
                );
            }
        }
        None => println!("  stored: (nothing — idle or collection failed)"),
    }
    if let Some(err) = &record.error {
        println!("  note: {err}");
    }
    Ok(())
}

/// `tok doctor`: read-only diagnostics (config, credentials, ccusage, overlay).
fn cmd_doctor() -> ExitCode {
    let cfg = match load_valid_config() {
        Ok(cfg) => cfg,
        Err(code) => return code,
    };
    let runtime = match build_runtime() {
        Ok(runtime) => runtime,
        Err(code) => return code,
    };
    match runtime.block_on(doctor::run_doctor(&cfg)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("tok: {e}");
            ExitCode::from(2)
        }
    }
}

/// `tok` (no subcommand): launch the dashboard, reading the store the collector writes.
fn cmd_tui() -> ExitCode {
    let cfg = match load_valid_config() {
        Ok(cfg) => cfg,
        Err(code) => return code,
    };
    let path = match paths::store_path() {
        Ok(path) => path,
        Err(e) => {
            eprintln!("tok: {e}");
            return ExitCode::from(2);
        }
    };
    let runtime = match build_runtime() {
        Ok(runtime) => runtime,
        Err(code) => return code,
    };
    match runtime.block_on(tui::run(&cfg, &path)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("tok: {e}");
            ExitCode::from(1)
        }
    }
}

fn print_help() {
    println!(
        "tok — Tokenomics: monitor LLM subscription accounts\n\n\
         USAGE:\n  \
           tok                launch the dashboard TUI\n  \
           tok init           write a starter tokenomics.toml to the config path\n  \
           tok validate       check tokenomics.toml (accounts, config dirs)\n  \
           tok accounts       list configured accounts\n  \
           tok once [--json]  collect one snapshot for every account and print it\n  \
           tok collector      run the background collector (writes the local store)\n  \
           tok doctor         read-only diagnostics (config, credentials, ccusage)\n\n\
         FLAGS:\n  \
           -h, --help         show this help\n  \
           -V, --version      show version\n\n\
         PATHS (cwd-independent):\n  \
           config   $TOKENOMICS_CONFIG, else ~/.config/tokenomics/tokenomics.toml\n  \
           store    $TOKENOMICS_DB, else ~/.local/share/tokenomics/tokenomics.db\n"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account(id: &str, active: bool) -> Account {
        Account {
            id: id.to_string(),
            label: id.to_uppercase(),
            provider: Provider::Claude,
            // A nonexistent dir is fine: `collect_record` isolates the error into the record rather
            // than hanging — the test only cares whether a record is produced at all.
            config_dir: Some(PathBuf::from("/nonexistent-tokenomics-test-dir")),
            api_key_env: None,
            color: None,
            active,
            limits_overlay: false,
        }
    }

    #[tokio::test]
    async fn collect_once_skips_inactive_accounts() {
        // An inactive account is unmonitored: `once` must not even attempt to collect it, so
        // `--json` output (built from `records`) contains only active accounts (spec 014 §D).
        let cfg = Config {
            settings: config::Settings::default(),
            accounts: vec![account("alive", true), account("dead", false)],
        };
        let records = collect_once(&cfg).await;
        assert_eq!(
            records
                .iter()
                .map(|r| r.account.as_str())
                .collect::<Vec<_>>(),
            vec!["alive"],
            "the inactive account must produce no record at all"
        );
    }

    #[tokio::test]
    async fn collect_once_dispatches_codex_as_idle_when_no_sessions() {
        // A Codex account with a nonexistent CODEX_HOME has no `sessions/` dir, so the Codex adapter
        // reports idle (`Ok(None)`) — `once` produces a real record with NO error (and no derived
        // session), proving the old "not wired yet" placeholder is gone (spec 013 §D).
        let mut codex = account("codex", true);
        codex.provider = Provider::Codex;
        let cfg = Config {
            settings: config::Settings::default(),
            accounts: vec![codex],
        };
        let records = collect_once(&cfg).await;
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].provider, Provider::Codex);
        assert!(
            records[0].error.is_none(),
            "missing sessions/ is idle, not an error record"
        );
        assert!(records[0].snapshot.is_none(), "no sessions ⇒ no snapshot");
        assert!(
            records[0].session_limit.is_none(),
            "no derived session limit for Codex"
        );
    }

    #[tokio::test]
    async fn collect_once_dispatches_zai_as_always_idle() {
        // A zai account has no config_dir and no local usage lane this wave (spec 019 §C): `once`
        // produces a real record with NO error, no snapshot, and no derived session limit.
        let mut zai = account("zai", true);
        zai.provider = Provider::Zai;
        zai.config_dir = None;
        zai.api_key_env = Some("Z_AI_TEST_KEY_MAIN".to_string());
        let cfg = Config {
            settings: config::Settings::default(),
            accounts: vec![zai],
        };
        let records = collect_once(&cfg).await;
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].provider, Provider::Zai);
        assert!(records[0].error.is_none(), "always idle, never an error");
        assert!(records[0].snapshot.is_none(), "no usage lane ⇒ no snapshot");
        assert!(records[0].session_limit.is_none());
    }

    #[tokio::test]
    async fn collect_once_dispatches_gemini_as_idle_when_no_tmp_dir() {
        // A Gemini account with a nonexistent GEMINI_CLI_HOME has no `tmp/` dir, so the Gemini
        // adapter reports idle (`Ok(None)`) — `once` produces a real record with NO error and no
        // derived session limit (spec 020 §D — `Provider::Gemini` dispatches, not "unknown").
        let mut gemini = account("gemini", true);
        gemini.provider = Provider::Gemini;
        let cfg = Config {
            settings: config::Settings::default(),
            accounts: vec![gemini],
        };
        let records = collect_once(&cfg).await;
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].provider, Provider::Gemini);
        assert!(
            records[0].error.is_none(),
            "missing tmp/ is idle, not an error record"
        );
        assert!(records[0].snapshot.is_none(), "no tmp/ ⇒ no snapshot");
        assert!(
            records[0].session_limit.is_none(),
            "no derived session limit for Gemini — no limits surface exists (spec 020 §C)"
        );
    }
}
