//! The Claude provider adapter: native transcript scan (default) or ccusage (opt-in) per account.
//!
//! Project: Tokenomics — monitor LLM subscription accounts (usage, limits, time-left) in a TUI
//! Module:  src/providers/claude/mod.rs
//! Deps:    async-trait, jiff; runner (Exec/Runner seam); transcripts (native), ccusage (pure core)
//! Tested:  inline `#[cfg(test)]` — ccusage lane via CannedRunner (CLAUDE_CONFIG_DIR pinned);
//!          native lane via a tempdir tree (no subprocess is ever spawned)
//!
//! Key responsibilities:
//! - `ClaudeAdapter<R>`: hold a `Runner` + the selected `Lane`; implement `ProviderAdapter`.
//! - `Lane::Native` (spec 026): `<config_dir>/projects/**/*.jsonl` → `TranscriptScanner` → reduce.
//! - `Lane::Ccusage` (spec 002): pinned-`CLAUDE_CONFIG_DIR` argv → run → parse → reduce. Selected
//!   only when `[settings] ccusage_cmd` is set.
//!
//! Design constraints:
//! - The `Runner` is injected so the ccusage lane is tested with canned bytes (no process spawn).
//! - Attribution is the account's `config_dir` (its `projects/` tree, or `CLAUDE_CONFIG_DIR`),
//!   never the logs' content.

pub mod ccusage;
pub mod creds;
pub mod identity;
pub mod overlay;
pub mod transcripts;

use std::time::Duration;

use async_trait::async_trait;
use jiff::Timestamp;

use crate::domain::{Account, UsageSnapshot};
use crate::error::{AppError, AppResult};
use crate::providers::ProviderAdapter;
use crate::runner::Runner;

use ccusage::{ccusage_command_spec, parse_ccusage_blocks, reduce_snapshot, CcusageInvocation};
use transcripts::TranscriptScanner;

/// Which local-usage lane an adapter runs.
#[derive(Debug)]
pub enum Lane {
    /// In-process scanner over the account's `projects/` tree (default, spec 026).
    Native(TranscriptScanner),
    /// `ccusage blocks --json` subprocess under `CLAUDE_CONFIG_DIR` (opt-in via `ccusage_cmd`).
    Ccusage {
        /// How to invoke ccusage.
        invocation: CcusageInvocation,
        /// Hard per-call timeout.
        timeout: Duration,
    },
}

/// Collect Claude usage for one account through the selected `Lane`.
#[derive(Debug)]
pub struct ClaudeAdapter<R: Runner> {
    runner: R,
    lane: Lane,
}

impl<R: Runner> ClaudeAdapter<R> {
    /// The lane `[settings] ccusage_cmd` selects: set (non-empty) ⇒ ccusage subprocess, else native.
    pub fn from_settings(runner: R, ccusage_cmd: Option<&[String]>, timeout: Duration) -> Self {
        match ccusage_cmd {
            Some(cmd) if !cmd.is_empty() => {
                Self::with_ccusage(runner, CcusageInvocation::from_override(Some(cmd)), timeout)
            }
            _ => Self::native(runner),
        }
    }

    /// The native transcript scanner lane.
    pub fn native(runner: R) -> Self {
        Self {
            runner,
            lane: Lane::Native(TranscriptScanner::new()),
        }
    }

    /// The ccusage subprocess lane with an explicit invocation and per-call timeout.
    pub fn with_ccusage(runner: R, invocation: CcusageInvocation, timeout: Duration) -> Self {
        Self {
            runner,
            lane: Lane::Ccusage {
                invocation,
                timeout,
            },
        }
    }

    /// Human label for diagnostics (`tok doctor`).
    pub fn lane_name(&self) -> &'static str {
        match self.lane {
            Lane::Native(_) => "native transcript scanner",
            Lane::Ccusage { .. } => "ccusage subprocess",
        }
    }

    /// Whether this adapter shells out to ccusage.
    pub fn uses_ccusage(&self) -> bool {
        matches!(self.lane, Lane::Ccusage { .. })
    }
}

#[async_trait]
impl<R: Runner> ProviderAdapter for ClaudeAdapter<R> {
    async fn collect(&self, account: &Account, now: Timestamp) -> AppResult<Option<UsageSnapshot>> {
        // Validation (spec 019 §A) guarantees a claude account always carries a config_dir; this is
        // a defensive early return, never a panic, if that guarantee is ever violated upstream.
        let Some(config_dir) = account.config_dir.as_deref() else {
            return Err(AppError::Credentials(format!(
                "account '{}': claude requires config_dir",
                account.id
            )));
        };
        match &self.lane {
            Lane::Native(scanner) => {
                let Some(events) = scanner.events(&config_dir.join("projects"), now).await? else {
                    return Ok(None);
                };
                Ok(transcripts::reduce_snapshot(&events, &account.id, now))
            }
            Lane::Ccusage {
                invocation,
                timeout,
            } => {
                let spec = ccusage_command_spec(invocation, config_dir, *timeout);
                let bytes = self.runner.run(&spec).await?;
                let parsed = parse_ccusage_blocks(&bytes)?;
                Ok(reduce_snapshot(&parsed, &account.id, account.provider, now))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::Provider;
    use crate::runner::CannedRunner;
    use std::path::PathBuf;

    const ACTIVE: &[u8] = include_bytes!("../../../fixtures/blocks_active.json");

    fn account(config_dir: PathBuf) -> Account {
        Account {
            id: "work".to_string(),
            label: "Work".to_string(),
            provider: Provider::Claude,
            config_dir: Some(config_dir),
            api_key_env: None,
            color: None,
            active: true,
            limits_overlay: false,
        }
    }

    #[tokio::test]
    async fn ccusage_lane_reduces_and_pins_config_dir() {
        let runner = CannedRunner::new(ACTIVE);
        let adapter = ClaudeAdapter::with_ccusage(
            runner,
            CcusageInvocation::default(),
            Duration::from_secs(5),
        );
        let now: Timestamp = "2026-07-04T10:00:00Z".parse().unwrap();

        let snap = adapter
            .collect(&account(PathBuf::from("/home/user/.claude-work")), now)
            .await
            .expect("collect ok")
            .expect("active block present");
        assert_eq!(snap.account_id, "work");
        assert_eq!(snap.total_tokens, 244_820_890);

        // The runner was handed CLAUDE_CONFIG_DIR pinned to this account's config dir.
        let spec = adapter.runner.last_spec().expect("a spec was run");
        assert!(spec
            .env
            .iter()
            .any(|(k, v)| k == "CLAUDE_CONFIG_DIR" && v == "/home/user/.claude-work"));
    }

    #[tokio::test]
    async fn native_lane_reads_the_projects_tree_and_never_spawns() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let now = Timestamp::now();
        let at = (now - Duration::from_secs(60)).to_string();
        let file = tmp.path().join("projects/slug/s.jsonl");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(
            &file,
            format!(
                r#"{{"type":"assistant","timestamp":"{at}","requestId":"r1","message":{{"id":"m1","model":"claude-opus-5","usage":{{"input_tokens":3,"output_tokens":4}}}}}}
"#
            ),
        )
        .unwrap();

        let adapter = ClaudeAdapter::native(CannedRunner::new(b"unused"));
        let snap = adapter
            .collect(&account(tmp.path().to_path_buf()), now)
            .await
            .expect("collect ok")
            .expect("active block present");
        assert_eq!(snap.total_tokens, 7);
        assert!(
            adapter.runner.last_spec().is_none(),
            "no subprocess in the native lane"
        );
        assert!(!adapter.uses_ccusage());
    }

    #[tokio::test]
    async fn native_lane_without_a_projects_tree_is_idle() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let adapter = ClaudeAdapter::native(CannedRunner::new(b"unused"));
        let got = adapter
            .collect(&account(tmp.path().to_path_buf()), Timestamp::now())
            .await
            .expect("collect ok");
        assert!(got.is_none());
    }

    #[test]
    fn settings_select_the_lane() {
        let native =
            ClaudeAdapter::from_settings(CannedRunner::new(b""), None, Duration::from_secs(1));
        assert!(!native.uses_ccusage());
        let empty =
            ClaudeAdapter::from_settings(CannedRunner::new(b""), Some(&[]), Duration::from_secs(1));
        assert!(!empty.uses_ccusage());
        let cmd = ["npx".to_string(), "ccusage".to_string()];
        let cc = ClaudeAdapter::from_settings(
            CannedRunner::new(b""),
            Some(&cmd),
            Duration::from_secs(1),
        );
        assert!(cc.uses_ccusage());
        assert_eq!(cc.lane_name(), "ccusage subprocess");
    }
}
