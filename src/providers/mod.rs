//! The provider seam: one trait every provider implements, so new providers are additive.
//!
//! Project: Tokenomics — monitor LLM subscription accounts (usage, limits, time-left) in a TUI
//! Module:  src/providers/mod.rs
//! Deps:    async-trait, jiff; claude/codex/zai adapters; runner (Runner seam)
//! Tested:  via providers/claude (ClaudeAdapter::collect) + providers/codex + providers/zai inline
//!          tests
//!
//! Key responsibilities:
//! - `ProviderAdapter`: collect a normalized snapshot for one account.
//! - `ProviderRegistry`: dispatch `collect` to the right adapter by `account.provider`, so the
//!   collector loop stays generic over ONE `ProviderAdapter` (spec 013 §D).
//!
//! Design constraints:
//! - Gemini/Grok slot in by adding a `providers/<x>/` module implementing this trait plus a
//!   `Provider` enum variant and a registry arm — the collector, store, and TUI stay untouched
//!   (zai, spec 019, is the most recent example of this seam holding).

use async_trait::async_trait;
use jiff::Timestamp;

use crate::domain::{Account, Provider, UsageSnapshot};
use crate::error::AppResult;
use crate::runner::Runner;

pub mod claude;
pub mod codex;
pub mod zai;

use claude::ClaudeAdapter;
use codex::CodexAdapter;
use zai::ZaiAdapter;

/// One provider's collection behavior. `collect` returns `None` when the account is idle
/// (no active usage block) — a valid state, not an error.
#[async_trait]
pub trait ProviderAdapter {
    /// Collect one account's current usage snapshot. `now` is injected for deterministic reduction.
    async fn collect(&self, account: &Account, now: Timestamp) -> AppResult<Option<UsageSnapshot>>;
}

/// Routes `collect` to the per-provider adapter by `account.provider`. Holds one adapter per
/// provider so the collector loop (and `tok once`) can be generic over a single `ProviderAdapter`
/// while each account still runs against its own provider's usage plane. The Claude adapter carries
/// the injected `Runner` (ccusage subprocess); the Codex adapter is filesystem-only.
#[derive(Debug)]
pub struct ProviderRegistry<R: Runner> {
    /// The Claude usage adapter (ccusage under each account's `CLAUDE_CONFIG_DIR`).
    pub claude: ClaudeAdapter<R>,
    /// The Codex usage adapter (sessions-JSONL under each account's `CODEX_HOME`).
    pub codex: CodexAdapter,
    /// The z.ai usage adapter (always idle this wave — limits-only, spec 019 §C).
    pub zai: ZaiAdapter,
}

#[async_trait]
impl<R: Runner> ProviderAdapter for ProviderRegistry<R> {
    async fn collect(&self, account: &Account, now: Timestamp) -> AppResult<Option<UsageSnapshot>> {
        match account.provider {
            Provider::Claude => self.claude.collect(account, now).await,
            Provider::Codex => self.codex.collect(account, now).await,
            Provider::Zai => self.zai.collect(account, now).await,
        }
    }
}
