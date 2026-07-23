//! The Kimi (Moonshot AI coding plan) provider adapter: limits-only, dormant until a live key
//! exists (spec 023) — the usage lane is always idle, mirroring z.ai (spec 019 §C).
//!
//! Project: Tokenomics — monitor LLM subscription accounts (usage, limits, time-left) in a TUI
//! Module:  src/providers/kimi/mod.rs
//! Deps:    async-trait, jiff; quota (the limits-lane parse + fetch seam)
//! Tested:  inline `#[cfg(test)]` — `collect` is always idle; `resolve_api_key` (env-var lookup)
//!
//! Key responsibilities:
//! - `KimiAdapter`: implement `ProviderAdapter::collect` — always `Ok(None)`. No usage plane exists
//!   for a Kimi coding-plan account; a v1 stays honestly idle rather than fabricating usage (spec
//!   023 §C, mirroring zai's rationale exactly).
//! - `resolve_api_key`: env-var NAME lookup, trim, present/unset/empty cases (mirrors
//!   `zai::resolve_api_key` exactly).
//!
//! Design constraints:
//! - Attribution is `api_key_env` (an env-var NAME), never `config_dir` — Kimi has no directory-
//!   scoped login (spec 023 §A).

pub mod quota;

use async_trait::async_trait;
use jiff::Timestamp;

use crate::domain::{Account, UsageSnapshot};
use crate::error::AppResult;
use crate::providers::ProviderAdapter;

/// The Kimi usage adapter. Zero-sized — no local usage plane to hold state for (limits-only).
#[derive(Debug, Default, Clone, Copy)]
pub struct KimiAdapter;

impl KimiAdapter {
    /// Build a Kimi adapter.
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl ProviderAdapter for KimiAdapter {
    async fn collect(
        &self,
        _account: &Account,
        _now: Timestamp,
    ) -> AppResult<Option<UsageSnapshot>> {
        // Always idle (spec 023 §C) — no invented usage, no fabricated cost.
        Ok(None)
    }
}

/// The Kimi API key for an opted-in account: the `api_key_env`-named env var's value, or
/// `Err(reason)` when `api_key_env` itself is absent (validation-guaranteed present for a kimi
/// account — this is a defensive case), unset, or empty (spec 023 §A/§D). Shared by `doctor`
/// (presence-only reporting) and the collector's overlay fetch (needs the actual value) so the
/// eligibility rule can never drift between the two call sites.
// ponytail: duplicate of zai::resolve_api_key — hoist to providers/mod.rs when a third
// key-attributed provider lands.
pub fn resolve_api_key(account: &Account) -> Result<String, String> {
    let Some(name) = account.api_key_env.as_deref() else {
        return Err("api_key_env not configured".to_string());
    };
    match std::env::var(name) {
        Ok(v) if !v.trim().is_empty() => Ok(v.trim().to_string()),
        Ok(_) => Err(format!("{name} is set but empty")),
        Err(_) => Err(format!("{name} is not set")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::Provider;

    fn account() -> Account {
        Account {
            id: "kimi-k2".to_string(),
            label: "Kimi K2".to_string(),
            provider: Provider::Kimi,
            config_dir: None,
            api_key_env: Some("KIMI_CODING_KEY".to_string()),
            active: true,
            color: None,
            limits_overlay: true,
        }
    }

    #[tokio::test]
    async fn collect_is_always_idle() {
        let snap = KimiAdapter::new()
            .collect(&account(), Timestamp::now())
            .await
            .expect("collect ok");
        assert!(
            snap.is_none(),
            "kimi has no usage lane — always idle (spec 023 §C)"
        );
    }

    /// Guard against an accidental future regression to `PathBuf::from("")`-style placeholders:
    /// `config_dir` is genuinely absent for a kimi account, not merely empty.
    #[test]
    fn kimi_account_carries_no_config_dir() {
        assert!(account().config_dir.is_none());
    }

    // ── spec 023 §A/§D: `resolve_api_key` — each test uses its own env-var name (parallel-safe) ──

    #[test]
    fn resolve_api_key_reads_the_named_env_var() {
        let mut a = account();
        a.api_key_env = Some("TOK_TEST_KIMI_KEY_PRESENT".to_string());
        std::env::set_var("TOK_TEST_KIMI_KEY_PRESENT", "fake-key-never-logged");
        assert_eq!(resolve_api_key(&a).as_deref(), Ok("fake-key-never-logged"));
        std::env::remove_var("TOK_TEST_KIMI_KEY_PRESENT");
    }

    #[test]
    fn resolve_api_key_trims_surrounding_whitespace() {
        // A key exported from a `.env` file often carries a trailing newline/space; an untrimmed
        // value reaches reqwest's bearer_auth as an invalid header value and fails every fetch.
        let mut a = account();
        a.api_key_env = Some("TOK_TEST_KIMI_KEY_WHITESPACE".to_string());
        std::env::set_var("TOK_TEST_KIMI_KEY_WHITESPACE", "  fake-key-never-logged\n");
        assert_eq!(resolve_api_key(&a).as_deref(), Ok("fake-key-never-logged"));
        std::env::remove_var("TOK_TEST_KIMI_KEY_WHITESPACE");
    }

    #[test]
    fn resolve_api_key_errors_when_env_var_unset() {
        let mut a = account();
        a.api_key_env = Some("TOK_TEST_KIMI_KEY_UNSET_DOES_NOT_EXIST".to_string());
        assert!(resolve_api_key(&a).is_err());
    }

    #[test]
    fn resolve_api_key_errors_when_env_var_empty() {
        let mut a = account();
        a.api_key_env = Some("TOK_TEST_KIMI_KEY_EMPTY".to_string());
        std::env::set_var("TOK_TEST_KIMI_KEY_EMPTY", "  ");
        assert!(resolve_api_key(&a).is_err());
        std::env::remove_var("TOK_TEST_KIMI_KEY_EMPTY");
    }

    #[test]
    fn resolve_api_key_errors_when_api_key_env_absent() {
        let mut a = account();
        a.api_key_env = None;
        assert!(resolve_api_key(&a).is_err());
    }
}
