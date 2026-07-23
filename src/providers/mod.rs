//! The provider seam: one trait every provider implements, so new providers are additive.
//!
//! Project: Tokenomics — monitor LLM subscription accounts (usage, limits, time-left) in a TUI
//! Module:  src/providers/mod.rs
//! Deps:    async-trait, jiff; claude/codex/zai/gemini/grok adapters; runner (Runner seam)
//! Tested:  via providers/claude (ClaudeAdapter::collect) + providers/codex + providers/zai +
//!          providers/gemini + providers/grok inline tests
//!
//! Key responsibilities:
//! - `ProviderAdapter`: collect a normalized snapshot for one account, plus `collect_local_limits`
//!   — limits knowable from local data alone, default none (spec 022 §B; only grok overrides it).
//! - `ProviderRegistry`: dispatch both methods to the right adapter by `account.provider`, so the
//!   collector loop stays generic over ONE `ProviderAdapter` (spec 013 §D).
//!
//! Design constraints:
//! - A new provider slots in by adding a `providers/<x>/` module implementing this trait plus a
//!   `Provider` enum variant and a registry arm — the collector, store, and TUI stay untouched
//!   (grok, specs 021/022, is the most recent example of this seam holding).

use async_trait::async_trait;
use jiff::Timestamp;

use crate::domain::{Account, Limit, Provider, UsageSnapshot};
use crate::error::AppResult;
use crate::runner::Runner;

pub mod claude;
pub mod codex;
pub mod gemini;
pub mod grok;
pub mod zai;

use claude::ClaudeAdapter;
use codex::CodexAdapter;
use gemini::GeminiAdapter;
use grok::GrokAdapter;
use zai::ZaiAdapter;

/// One provider's collection behavior. `collect` returns `None` when the account is idle
/// (no active usage block) — a valid state, not an error.
#[async_trait]
pub trait ProviderAdapter {
    /// Collect one account's current usage snapshot. `now` is injected for deterministic reduction.
    async fn collect(&self, account: &Account, now: Timestamp) -> AppResult<Option<UsageSnapshot>>;

    /// Limits knowable from LOCAL data alone (no network, no opt-in) — e.g. Grok's weekly quota
    /// from its billing log (spec 022 §B). Default: none, so existing adapters are untouched.
    /// Runs independently of usage idleness: a quota stays live while no inference runs.
    async fn collect_local_limits(
        &self,
        _account: &Account,
        _now: Timestamp,
        _warn_pct: f64,
        _crit_pct: f64,
    ) -> AppResult<Vec<Limit>> {
        Ok(Vec::new())
    }
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
    /// The Gemini usage adapter (chats-JSONL under each account's `GEMINI_CLI_HOME`, spec 020 §B).
    pub gemini: GeminiAdapter,
    /// The Grok usage adapter (unified.jsonl under each account's `GROK_HOME`, spec 021 §B).
    pub grok: GrokAdapter,
}

#[async_trait]
impl<R: Runner> ProviderAdapter for ProviderRegistry<R> {
    async fn collect(&self, account: &Account, now: Timestamp) -> AppResult<Option<UsageSnapshot>> {
        match account.provider {
            Provider::Claude => self.claude.collect(account, now).await,
            Provider::Codex => self.codex.collect(account, now).await,
            Provider::Zai => self.zai.collect(account, now).await,
            Provider::Gemini => self.gemini.collect(account, now).await,
            Provider::Grok => self.grok.collect(account, now).await,
        }
    }

    async fn collect_local_limits(
        &self,
        account: &Account,
        now: Timestamp,
        warn_pct: f64,
        crit_pct: f64,
    ) -> AppResult<Vec<Limit>> {
        match account.provider {
            // Only grok has a local limits lane today (spec 022 §B); everyone else inherits the
            // empty default explicitly, so a future lane is an additive arm here.
            Provider::Grok => {
                self.grok
                    .collect_local_limits(account, now, warn_pct, crit_pct)
                    .await
            }
            Provider::Claude | Provider::Codex | Provider::Zai | Provider::Gemini => Ok(Vec::new()),
        }
    }
}
