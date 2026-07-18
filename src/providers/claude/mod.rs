//! The Claude provider adapter: run ccusage per account, reduce to a normalized snapshot.
//!
//! Project: Tokenomics — monitor LLM subscription accounts (usage, limits, time-left) in a TUI
//! Module:  src/providers/claude/mod.rs
//! Deps:    async-trait, jiff; runner (Exec/Runner seam); ccusage (pure core)
//! Tested:  inline `#[cfg(test)]` — collect via CannedRunner asserts CLAUDE_CONFIG_DIR in env
//!
//! Key responsibilities:
//! - `ClaudeAdapter<R>`: hold a `Runner` + ccusage invocation + timeout; implement `ProviderAdapter`.
//! - `collect`: build the pinned-`CLAUDE_CONFIG_DIR` argv → run → parse → reduce.
//!
//! Design constraints:
//! - The `Runner` is injected so `collect` is tested with canned bytes (no process spawn).
//! - Attribution is the account's `config_dir` via `CLAUDE_CONFIG_DIR`, never the logs.

pub mod ccusage;
pub mod creds;
pub mod overlay;

use std::time::Duration;

use async_trait::async_trait;
use jiff::Timestamp;

use crate::domain::{Account, UsageSnapshot};
use crate::error::{AppError, AppResult};
use crate::providers::ProviderAdapter;
use crate::runner::Runner;

use ccusage::{ccusage_command_spec, parse_ccusage_blocks, reduce_snapshot, CcusageInvocation};

/// Collect Claude usage by shelling out to ccusage under each account's `CLAUDE_CONFIG_DIR`.
#[derive(Debug)]
pub struct ClaudeAdapter<R: Runner> {
    runner: R,
    invocation: CcusageInvocation,
    timeout: Duration,
}

impl<R: Runner> ClaudeAdapter<R> {
    /// Build an adapter from a runner, a ccusage invocation, and a per-call timeout.
    pub fn new(runner: R, invocation: CcusageInvocation, timeout: Duration) -> Self {
        Self {
            runner,
            invocation,
            timeout,
        }
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
        let spec = ccusage_command_spec(&self.invocation, config_dir, self.timeout);
        let bytes = self.runner.run(&spec).await?;
        let parsed = parse_ccusage_blocks(&bytes)?;
        Ok(reduce_snapshot(&parsed, &account.id, account.provider, now))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::Provider;
    use crate::runner::CannedRunner;
    use std::path::PathBuf;

    const ACTIVE: &[u8] = include_bytes!("../../../fixtures/blocks_active.json");

    fn account() -> Account {
        Account {
            id: "work".to_string(),
            label: "Work".to_string(),
            provider: Provider::Claude,
            config_dir: Some(PathBuf::from("/home/user/.claude-work")),
            api_key_env: None,
            color: None,
            active: true,
            limits_overlay: false,
        }
    }

    #[tokio::test]
    async fn collect_reduces_and_pins_config_dir() {
        let runner = CannedRunner::new(ACTIVE);
        let adapter =
            ClaudeAdapter::new(runner, CcusageInvocation::default(), Duration::from_secs(5));
        let now: Timestamp = "2026-07-04T10:00:00Z".parse().unwrap();

        let snap = adapter
            .collect(&account(), now)
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
}
