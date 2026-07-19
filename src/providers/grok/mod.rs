//! The Grok provider adapter: reduce `unified.jsonl` per-inference usage under a `GROK_HOME`.
//!
//! Project: Tokenomics — monitor LLM subscription accounts (usage, limits, time-left) in a TUI
//! Module:  src/providers/grok/mod.rs
//! Deps:    async-trait, jiff; logs (pure core)
//! Tested:  inline `#[cfg(test)]` in logs.rs; adapter test on a fixture file below (spec 021 §B)
//!
//! Key responsibilities:
//! - `GrokAdapter`: implement `ProviderAdapter::collect` over the account's `GROK_HOME`
//!   (`config_dir`), reading the single append-only `logs/unified.jsonl` — unlike Gemini's
//!   fan-out across N project-hash dirs, a Grok account's usage is one global log (spec 021 §B).
//!
//! Design constraints:
//! - Attribution is the account's `config_dir` (its `GROK_HOME`), never the logs.
//! - No limits/overlay plane exists for Grok's subscription quota (spec 021 §C) — this adapter only
//!   ever produces a usage snapshot or idle; it never fetches anything over the network.

pub mod logs;

use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use jiff::Timestamp;

use crate::domain::{Account, UsageSnapshot};
use crate::error::{AppError, AppResult};
use crate::providers::ProviderAdapter;

use logs::{parse_inference_events, reduce_grok_snapshot};

/// Cheap mtime prune bound for the log read: the reduce window (5h) plus slack for clock skew /
/// buffered writes — mirrors Gemini's `MTIME_LOOKBACK`. Not a windowing claim;
/// `reduce_grok_snapshot` enforces the exact 5h cutoff per event.
const MTIME_LOOKBACK: Duration = Duration::from_mins(330);

/// The Grok usage adapter. Zero-sized — no injected runner: the usage plane is pure filesystem
/// I/O, no subprocess (mirrors `GeminiAdapter`).
#[derive(Debug, Default, Clone, Copy)]
pub struct GrokAdapter;

impl GrokAdapter {
    /// Build a Grok adapter.
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl ProviderAdapter for GrokAdapter {
    async fn collect(&self, account: &Account, now: Timestamp) -> AppResult<Option<UsageSnapshot>> {
        // Validation (spec 021 §A) guarantees a grok account always carries a config_dir; this is a
        // defensive early return, never a panic, if that guarantee is ever violated upstream.
        let Some(config_dir) = account.config_dir.as_deref() else {
            return Err(AppError::SessionsScan(format!(
                "account '{}': grok requires config_dir",
                account.id
            )));
        };
        let cutoff: SystemTime = (now - MTIME_LOOKBACK).into();
        // ponytail: whole-file read + ts-filter each poll. Grok Build has no log rotation, so this
        // grows unbounded; upgrade to a byte-offset tail read if unified.jsonl ever reaches MBs.
        let log_path = config_dir.join("logs").join("unified.jsonl");
        let account_id = account.id.clone();

        // The file read is synchronous filesystem I/O; run it on the blocking pool so a large log
        // can never stall the collector's current-thread runtime (rules/rust/async-tokio.md).
        tokio::task::spawn_blocking(move || {
            // mtime prune: if the whole log is untouched beyond the lookback, it holds no in-window
            // event — skip the read entirely (missing file lands here too ⇒ idle, not an error).
            let fresh_enough = std::fs::metadata(&log_path)
                .and_then(|m| m.modified())
                .is_ok_and(|mtime| mtime >= cutoff);
            if !fresh_enough {
                return None;
            }
            let bytes = std::fs::read(&log_path).ok()?;
            let events = parse_inference_events(&bytes);
            reduce_grok_snapshot(&events, &account_id, now)
        })
        .await
        .map_err(|e| AppError::SessionsScan(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::Provider;
    use std::path::PathBuf;
    use std::time::{Duration, SystemTime};

    fn account(config_dir: PathBuf) -> Account {
        Account {
            id: "grok-heavy".to_string(),
            label: "Grok".to_string(),
            provider: Provider::Grok,
            config_dir: Some(config_dir),
            api_key_env: None,
            active: true,
            color: None,
            limits_overlay: false,
        }
    }

    /// One synthetic `inference_done` line matching the real observed shape (spec 021 §B) — never
    /// real log content, all values invented for this test.
    fn event_line(timestamp: Timestamp, prompt: u64, cached: u64, completion: u64) -> String {
        format!(
            r#"{{"ts":"{timestamp}","src":"shell","sid":"synthetic","msg":"shell.turn.inference_done","ctx":{{"loop_index":1,"prompt_tokens":{prompt},"cached_prompt_tokens":{cached},"completion_tokens":{completion},"reasoning_tokens":0}}}}"#
        )
    }

    fn write_log(dir: &std::path::Path, contents: &str) -> PathBuf {
        let logs = dir.join("logs");
        std::fs::create_dir_all(&logs).expect("mkdir -p logs");
        let path = logs.join("unified.jsonl");
        std::fs::write(&path, contents).expect("write log");
        path
    }

    #[tokio::test]
    async fn missing_log_is_idle() {
        let dir = tempfile::tempdir().expect("tempdir");
        let snap = GrokAdapter::new()
            .collect(&account(dir.path().to_path_buf()), Timestamp::now())
            .await
            .expect("collect ok");
        assert!(snap.is_none());
    }

    #[tokio::test]
    async fn collect_reduces_in_window_events() {
        let now = Timestamp::now();
        let dir = tempfile::tempdir().expect("tempdir");
        let recent = now - Duration::from_hours(1);
        let contents = format!(
            "{}\n{}\n",
            event_line(recent, 42_316, 5_504, 92),
            event_line(recent, 1_000, 200, 50),
        );
        write_log(dir.path(), &contents);

        let snap = GrokAdapter::new()
            .collect(&account(dir.path().to_path_buf()), now)
            .await
            .expect("collect ok")
            .expect("in-window events must yield a snapshot, not idle");
        assert_eq!(snap.account_id, "grok-heavy");
        assert_eq!(snap.provider, Provider::Grok);
        assert_eq!(snap.input, 37_612); // (42316-5504) + (1000-200)
        assert_eq!(snap.cache_read, 5_704);
        assert_eq!(snap.output, 142);
        assert_eq!(snap.total_tokens, 43_458);
        assert_eq!(snap.cost_notional, None);
        assert!(snap.window.is_none());
    }

    #[tokio::test]
    async fn a_stale_log_is_pruned_by_mtime_before_reduction() {
        let now = Timestamp::now();
        let dir = tempfile::tempdir().expect("tempdir");
        let recent = now - Duration::from_hours(1); // the event's OWN ts is in-window
        let path = write_log(
            dir.path(),
            &format!("{}\n", event_line(recent, 1_000, 200, 50)),
        );
        let file = std::fs::File::open(&path).expect("reopen for mtime");
        file.set_modified(SystemTime::now() - Duration::from_hours(24))
            .expect("backdate mtime"); // only the FILE's mtime is stale

        let snap = GrokAdapter::new()
            .collect(&account(dir.path().to_path_buf()), now)
            .await
            .expect("collect ok");
        assert!(
            snap.is_none(),
            "a log pruned by mtime must never contribute, even with an in-window event inside it"
        );
    }

    #[tokio::test]
    async fn account_without_config_dir_yields_an_error_not_a_panic() {
        let mut a = account(PathBuf::new());
        a.config_dir = None;
        let result = GrokAdapter::new().collect(&a, Timestamp::now()).await;
        assert!(
            result.is_err(),
            "a grok account with no config_dir must error, never panic: {result:?}"
        );
    }
}
