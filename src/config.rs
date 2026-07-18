//! `tokenomics.toml` parsing and validation.
//!
//! Project: Tokenomics — monitor LLM subscription accounts (usage, limits, time-left) in a TUI
//! Module:  src/config.rs
//! Deps:    serde, toml (path resolution lives in `crate::paths`)
//! Tested:  inline `#[cfg(test)]` below + tests/cli.rs (validate/accounts); `ledger_path` parsing
//!          (spec 017 §B)
//!
//! Key responsibilities:
//! - `Config`/`Settings` schema with `deny_unknown_fields` (typos are errors).
//! - `parse` (pure, with `~` expansion) + `load` (path resolution + read).
//! - `validate` (pure findings) and `validate_environment` (filesystem existence).
//!
//! Design constraints:
//! - `validate` performs no I/O so the whole rule set is table-testable.
//! - The account list is the single source of truth; each account owns its `CLAUDE_CONFIG_DIR` /
//!   `CODEX_HOME` / `GEMINI_CLI_HOME` (claude/codex/gemini) or `api_key_env` (zai) — `validate`
//!   enforces which per provider (spec 019 §A, spec 020 §A).

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::domain::{Account, Provider};
use crate::error::{AppError, AppResult};

/// Parsed `tokenomics.toml`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Global settings (cadences, thresholds).
    #[serde(default)]
    pub settings: Settings,
    /// The monitored accounts (`[[account]]` blocks).
    #[serde(default, rename = "account")]
    pub accounts: Vec<Account>,
}

/// Global settings block, all fields optional with sensible defaults.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    /// ccusage (local) poll cadence in seconds.
    #[serde(default = "default_poll_local")]
    pub poll_local_secs: u64,
    /// Overlay poll cadence in seconds.
    #[serde(default = "default_poll_overlay")]
    pub poll_overlay_secs: u64,
    /// Utilization % at which a window is a warning.
    #[serde(default = "default_warn")]
    pub warn_pct: f64,
    /// Utilization % at which a window is critical.
    #[serde(default = "default_crit")]
    pub crit_pct: f64,
    /// Optional launcher for ccusage (argv[0] + prefix args). Absent/empty ⇒ a bare `ccusage` on
    /// `PATH`; set to e.g. `["npx", "ccusage"]` on machines without a global install.
    #[serde(default)]
    pub ccusage_cmd: Option<Vec<String>>,
    /// Optional path to the subscription ledger (spec 017 §B) — beaten by `$TOKENOMICS_LEDGER`;
    /// absent (and no env override) ⇒ the ledger plane is `Off` (no rendering, no warning).
    #[serde(default)]
    pub ledger_path: Option<String>,
}

const POLL_LOCAL_FLOOR: u64 = 5;
const POLL_OVERLAY_FLOOR: u64 = 60;

fn default_poll_local() -> u64 {
    10
}
fn default_poll_overlay() -> u64 {
    300
}
fn default_warn() -> f64 {
    75.0
}
fn default_crit() -> f64 {
    90.0
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            poll_local_secs: default_poll_local(),
            poll_overlay_secs: default_poll_overlay(),
            warn_pct: default_warn(),
            crit_pct: default_crit(),
            ccusage_cmd: None,
            ledger_path: None,
        }
    }
}

/// One validation finding. Every finding is currently an error (a reason `validate` failed); the
/// warning tier is reintroduced by the first wave that needs a non-fatal advisory.
#[derive(Debug, Clone)]
pub struct Finding {
    pub message: String,
}

impl Finding {
    fn error(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for Finding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "✗ {}", self.message)
    }
}

impl Config {
    /// Parse config text, expanding a leading `~` in each `config_dir` against `$HOME`.
    pub fn parse(text: &str) -> AppResult<Self> {
        let mut cfg: Self =
            toml::from_str(text).map_err(|e| AppError::ConfigParse(e.to_string()))?;
        let home = std::env::var_os("HOME").map(PathBuf::from);
        for account in &mut cfg.accounts {
            if let Some(dir) = &account.config_dir {
                account.config_dir = Some(expand_tilde(dir, home.as_deref()));
            }
        }
        Ok(cfg)
    }

    /// Load config from `$TOKENOMICS_CONFIG`, else the XDG config path (cwd-independent — see
    /// [`crate::paths::config_path`]).
    pub fn load() -> AppResult<Self> {
        let path = crate::paths::config_path()?;
        let text = std::fs::read_to_string(&path).map_err(|source| AppError::ConfigRead {
            path: path.clone(),
            source,
        })?;
        Self::parse(&text)
    }
}

/// Expand a leading `~` to `home`. Pure (home passed in) so it is table-testable.
fn expand_tilde(path: &Path, home: Option<&Path>) -> PathBuf {
    let Some(home) = home else {
        return path.to_path_buf();
    };
    match path.strip_prefix("~") {
        Ok(rest) => home.join(rest),
        Err(_) => path.to_path_buf(),
    }
}

/// True when `color` is a named ratatui color or a `#rrggbb` hex string.
fn is_valid_color(color: &str) -> bool {
    const NAMED: &[&str] = &[
        "black",
        "red",
        "green",
        "yellow",
        "blue",
        "magenta",
        "cyan",
        "gray",
        "grey",
        "darkgray",
        "darkgrey",
        "white",
        "lightred",
        "lightgreen",
        "lightyellow",
        "lightblue",
        "lightmagenta",
        "lightcyan",
    ];
    let c = color.trim().to_ascii_lowercase();
    if NAMED.contains(&c.as_str()) {
        return true;
    }
    c.strip_prefix('#')
        .is_some_and(|hex| hex.len() == 6 && hex.chars().all(|ch| ch.is_ascii_hexdigit()))
}

/// Pure validation of a parsed config. No I/O — see [`validate_environment`] for filesystem checks.
pub fn validate(cfg: &Config) -> Vec<Finding> {
    let mut findings = Vec::new();
    if cfg.accounts.is_empty() {
        findings.push(Finding::error("no accounts configured"));
    }
    let mut seen: HashSet<&str> = HashSet::new();
    for account in &cfg.accounts {
        if account.id.trim().is_empty() {
            findings.push(Finding::error("an account has an empty id"));
        } else if !seen.insert(account.id.as_str()) {
            findings.push(Finding::error(format!(
                "duplicate account id '{}'",
                account.id
            )));
        }
        if account.label.trim().is_empty() {
            findings.push(Finding::error(format!(
                "account '{}' has an empty label",
                account.id
            )));
        }
        if let Some(color) = &account.color {
            if !is_valid_color(color) {
                findings.push(Finding::error(format!(
                    "account '{}' has an invalid color '{color}'",
                    account.id
                )));
            }
        }
        findings.extend(validate_provider_fields(account));
    }
    let s = &cfg.settings;
    if s.crit_pct <= s.warn_pct {
        findings.push(Finding::error(format!(
            "crit_pct ({}) must be greater than warn_pct ({})",
            s.crit_pct, s.warn_pct
        )));
    }
    if s.poll_local_secs < POLL_LOCAL_FLOOR {
        findings.push(Finding::error(format!(
            "poll_local_secs ({}) is below the floor of {POLL_LOCAL_FLOOR}s",
            s.poll_local_secs
        )));
    }
    if s.poll_overlay_secs < POLL_OVERLAY_FLOOR {
        findings.push(Finding::error(format!(
            "poll_overlay_secs ({}) is below the floor of {POLL_OVERLAY_FLOOR}s",
            s.poll_overlay_secs
        )));
    }
    findings
}

/// Per-provider account-field validation (spec 019 §A, spec 020 §A): `claude`/`codex`/`gemini`
/// require `config_dir` and reject `api_key_env`; `zai` requires `api_key_env` (a non-empty
/// env-var NAME) and leaves `config_dir` optional (accepted but unused this wave). Pure — no I/O,
/// no env-var reads.
fn validate_provider_fields(account: &Account) -> Vec<Finding> {
    let mut findings = Vec::new();
    match account.provider {
        Provider::Claude | Provider::Codex | Provider::Gemini => {
            if account.config_dir.is_none() {
                findings.push(Finding::error(format!(
                    "account '{}': config_dir is required for provider '{}'",
                    account.id, account.provider
                )));
            }
            if account.api_key_env.is_some() {
                findings.push(Finding::error(format!(
                    "account '{}': api_key_env is not used by provider '{}'",
                    account.id, account.provider
                )));
            }
        }
        Provider::Zai => {
            let missing = account
                .api_key_env
                .as_deref()
                .is_none_or(|v| v.trim().is_empty());
            if missing {
                findings.push(Finding::error(format!(
                    "account '{}': api_key_env is required for provider 'zai'",
                    account.id
                )));
            }
        }
    }
    findings
}

/// Filesystem checks kept separate so [`validate`] stays pure: each account whose provider
/// requires a `config_dir` (claude/codex/gemini — see [`validate_provider_fields`]) must have one
/// that exists. A zai account's `config_dir` is accepted but unused this wave (spec 019 §A), so it
/// is never existence-checked here even when provided — a placeholder value for a future GLM lane
/// must not hard-fail environment validation for a field nothing reads.
pub fn validate_environment(cfg: &Config) -> Vec<Finding> {
    cfg.accounts
        .iter()
        .filter(|a| {
            matches!(
                a.provider,
                Provider::Claude | Provider::Codex | Provider::Gemini
            )
        })
        .filter_map(|a| a.config_dir.as_ref().map(|dir| (a, dir)))
        .filter(|(_, dir)| !dir.is_dir())
        .map(|(a, dir)| {
            Finding::error(format!(
                "account '{}': config_dir {} does not exist",
                a.id,
                dir.display()
            ))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::Provider;

    const ONE: &str = "\
[[account]]
id = \"a\"
label = \"A\"
provider = \"claude\"
config_dir = \"/tmp\"
";

    #[test]
    fn parses_defaults_and_one_account() {
        let cfg = Config::parse(ONE).expect("parses");
        assert_eq!(cfg.settings.poll_local_secs, 10);
        assert_eq!(cfg.settings.poll_overlay_secs, 300);
        assert_eq!(cfg.accounts.len(), 1);
        assert_eq!(cfg.accounts[0].provider, Provider::Claude);
        assert!(!cfg.accounts[0].limits_overlay);
    }

    #[test]
    fn rejects_unknown_top_level_field() {
        assert!(Config::parse("wat = 1\n").is_err());
    }

    #[test]
    fn parses_codex_provider() {
        let toml = ONE.replace("claude", "codex");
        let cfg = Config::parse(&toml).expect("parses");
        assert_eq!(cfg.accounts[0].provider, Provider::Codex);
        assert_eq!(Provider::parse("codex"), Some(Provider::Codex));
        assert_eq!(Provider::Codex.as_str(), "codex");
    }

    #[test]
    fn active_defaults_true_and_parses_false() {
        let cfg = Config::parse(ONE).expect("parses");
        assert!(cfg.accounts[0].active);
        let toml = format!("{ONE}active = false\n");
        let cfg = Config::parse(&toml).expect("parses");
        assert!(!cfg.accounts[0].active);
    }

    #[test]
    fn rejects_unknown_provider() {
        let toml = ONE.replace("claude", "grok");
        assert!(Config::parse(&toml).is_err());
    }

    #[test]
    fn expands_tilde_with_explicit_home() {
        let home = PathBuf::from("/home/x");
        assert_eq!(
            expand_tilde(Path::new("~/.claude"), Some(&home)),
            PathBuf::from("/home/x/.claude")
        );
        assert_eq!(expand_tilde(Path::new("~"), Some(&home)), home);
        assert_eq!(
            expand_tilde(Path::new("/abs/path"), Some(&home)),
            PathBuf::from("/abs/path")
        );
    }

    #[test]
    fn colors_named_and_hex() {
        assert!(is_valid_color("cyan"));
        assert!(is_valid_color("  LightBlue "));
        assert!(is_valid_color("#00ffcc"));
        assert!(!is_valid_color("#00ff"));
        assert!(!is_valid_color("burple"));
    }

    #[test]
    fn validate_clean_config_has_no_findings() {
        let cfg = Config::parse(ONE).expect("parses");
        assert!(validate(&cfg).is_empty());
    }

    #[test]
    fn validate_flags_duplicate_ids() {
        let toml = "\
[[account]]
id = \"dup\"
label = \"One\"
provider = \"claude\"
config_dir = \"/tmp\"
[[account]]
id = \"dup\"
label = \"Two\"
provider = \"claude\"
config_dir = \"/tmp\"
";
        let cfg = Config::parse(toml).expect("parses");
        assert!(validate(&cfg)
            .iter()
            .any(|f| f.message.contains("duplicate account id")));
    }

    #[test]
    fn validate_flags_threshold_floor_and_empties() {
        let toml = "\
[settings]
poll_local_secs = 1
poll_overlay_secs = 10
warn_pct = 90.0
crit_pct = 80.0

[[account]]
id = \"a\"
label = \"\"
provider = \"claude\"
config_dir = \"/tmp\"
color = \"burple\"
";
        let cfg = Config::parse(toml).expect("parses");
        let msgs: Vec<String> = validate(&cfg).into_iter().map(|f| f.message).collect();
        assert!(msgs.iter().any(|m| m.contains("crit_pct")));
        assert!(msgs.iter().any(|m| m.contains("poll_local_secs")));
        assert!(msgs.iter().any(|m| m.contains("poll_overlay_secs")));
        assert!(msgs.iter().any(|m| m.contains("empty label")));
        assert!(msgs.iter().any(|m| m.contains("invalid color")));
    }

    #[test]
    fn validate_flags_no_accounts() {
        let cfg = Config::parse("").expect("parses empty");
        assert!(validate(&cfg)
            .iter()
            .any(|f| f.message.contains("no accounts")));
    }

    // ── spec 017 §B: `[settings] ledger_path` ──────────────────────────────────────────────────

    #[test]
    fn ledger_path_absent_by_default_and_existing_configs_still_parse() {
        // A config with no `ledger_path` at all (every config written before spec 017) must keep
        // parsing exactly as before — the field is optional and defaults to `None`.
        let cfg = Config::parse(ONE).expect("pre-spec-017 config still parses");
        assert_eq!(cfg.settings.ledger_path, None);
    }

    #[test]
    fn ledger_path_parses_when_present() {
        let toml = "\
[settings]
ledger_path = \"/home/example/ledger/subscriptions.toml\"

[[account]]
id = \"a\"
label = \"A\"
provider = \"claude\"
config_dir = \"/tmp\"
";
        let cfg = Config::parse(toml).expect("parses");
        assert_eq!(
            cfg.settings.ledger_path.as_deref(),
            Some("/home/example/ledger/subscriptions.toml")
        );
    }

    // ── spec 019 §A (AC1): "zai" round-trips; per-provider config_dir/api_key_env validation ────

    const ZAI: &str = "\
[[account]]
id = \"zai-lite\"
label = \"z.ai GLM Lite\"
provider = \"zai\"
api_key_env = \"Z_AI_CODING_KEY\"
";

    #[test]
    fn zai_round_trips_provider_id() {
        assert_eq!(Provider::parse("zai"), Some(Provider::Zai));
        assert_eq!(Provider::Zai.as_str(), "zai");
    }

    #[test]
    fn zai_account_parses_with_no_config_dir_and_a_named_api_key_env() {
        let cfg = Config::parse(ZAI).expect("parses");
        assert_eq!(cfg.accounts[0].provider, Provider::Zai);
        assert_eq!(cfg.accounts[0].config_dir, None);
        assert_eq!(
            cfg.accounts[0].api_key_env.as_deref(),
            Some("Z_AI_CODING_KEY")
        );
        assert!(
            validate(&cfg).is_empty(),
            "a well-formed zai account must validate clean: {:?}",
            validate(&cfg)
        );
    }

    #[test]
    fn zai_account_accepts_an_optional_unused_config_dir() {
        let toml = format!("{ZAI}config_dir = \"/tmp\"\n");
        let cfg = Config::parse(&toml).expect("parses");
        assert_eq!(
            cfg.accounts[0].config_dir.as_deref(),
            Some(Path::new("/tmp"))
        );
        assert!(validate(&cfg).is_empty());
    }

    #[test]
    fn zai_account_without_api_key_env_fails_validation_naming_the_account() {
        let toml = "\
[[account]]
id = \"zai-lite\"
label = \"z.ai GLM Lite\"
provider = \"zai\"
";
        let cfg = Config::parse(toml).expect("parses");
        let msgs: Vec<String> = validate(&cfg).into_iter().map(|f| f.message).collect();
        assert!(
            msgs.iter()
                .any(|m| m.contains("zai-lite") && m.contains("api_key_env")),
            "must name the account and the missing field: {msgs:?}"
        );
    }

    #[test]
    fn zai_account_with_empty_api_key_env_fails_validation() {
        let toml = format!("{}\n", ZAI.replace("Z_AI_CODING_KEY", "   "));
        let cfg = Config::parse(&toml).expect("parses");
        assert!(validate(&cfg)
            .iter()
            .any(|f| f.message.contains("api_key_env")));
    }

    #[test]
    fn claude_account_without_config_dir_fails_validation_naming_the_account() {
        let toml = "\
[[account]]
id = \"claude-work\"
label = \"Work\"
provider = \"claude\"
";
        let cfg = Config::parse(toml).expect("parses");
        let msgs: Vec<String> = validate(&cfg).into_iter().map(|f| f.message).collect();
        assert!(
            msgs.iter()
                .any(|m| m.contains("claude-work") && m.contains("config_dir")),
            "must name the account and the missing field: {msgs:?}"
        );
    }

    #[test]
    fn codex_account_without_config_dir_fails_validation() {
        let toml = "\
[[account]]
id = \"codex-work\"
label = \"Work\"
provider = \"codex\"
";
        let cfg = Config::parse(toml).expect("parses");
        assert!(validate(&cfg)
            .iter()
            .any(|f| f.message.contains("config_dir")));
    }

    #[test]
    fn claude_account_with_api_key_env_fails_validation() {
        let toml = "\
[[account]]
id = \"claude-work\"
label = \"Work\"
provider = \"claude\"
config_dir = \"/tmp\"
api_key_env = \"SOME_KEY\"
";
        let cfg = Config::parse(toml).expect("parses");
        assert!(validate(&cfg)
            .iter()
            .any(|f| f.message.contains("api_key_env")));
    }

    #[test]
    fn codex_account_with_api_key_env_fails_validation() {
        let toml = "\
[[account]]
id = \"codex-work\"
label = \"Work\"
provider = \"codex\"
config_dir = \"/tmp\"
api_key_env = \"SOME_KEY\"
";
        let cfg = Config::parse(toml).expect("parses");
        assert!(validate(&cfg)
            .iter()
            .any(|f| f.message.contains("api_key_env")));
    }

    #[test]
    fn validate_environment_never_checks_a_zai_accounts_absent_config_dir() {
        // A zai account with no config_dir at all must not be flagged as a "missing directory" —
        // that filesystem check only applies to providers that require the field (claude/codex).
        let cfg = Config::parse(ZAI).expect("parses");
        assert!(validate_environment(&cfg).is_empty());
    }

    #[test]
    fn validate_environment_never_checks_a_zai_accounts_provided_but_nonexistent_config_dir() {
        // zai's config_dir is accepted-but-unused this wave — a placeholder value that happens not
        // to exist must not hard-fail environment validation for a field nothing reads.
        let toml = format!("{ZAI}config_dir = \"/nonexistent/path/for/a/future/glm/lane\"\n");
        let cfg = Config::parse(&toml).expect("parses");
        assert!(validate_environment(&cfg).is_empty());
    }

    #[test]
    fn existing_claude_and_codex_accounts_still_validate_and_round_trip_unchanged() {
        // Existing tests already cover parsing; this pins the validation side post-scaffolding —
        // a pre-spec-019 config (Some(config_dir), no api_key_env) must validate clean for both.
        let cfg = Config::parse(ONE).expect("parses");
        assert!(validate(&cfg).is_empty());
        let codex_toml = ONE.replace("claude", "codex");
        let codex_cfg = Config::parse(&codex_toml).expect("parses");
        assert!(validate(&codex_cfg).is_empty());
    }

    // ── spec 020 §A (AC1): "gemini" round-trips; config_dir required, api_key_env rejected,
    // limits_overlay accepted-but-ignored ──────────────────────────────────────────────────────

    const GEMINI: &str = "\
[[account]]
id = \"gemini-personal\"
label = \"Gemini Personal\"
provider = \"gemini\"
config_dir = \"/home/example/.gemini\"
";

    #[test]
    fn gemini_round_trips_provider_id() {
        assert_eq!(Provider::parse("gemini"), Some(Provider::Gemini));
        assert_eq!(Provider::Gemini.as_str(), "gemini");
    }

    #[test]
    fn gemini_account_parses_with_config_dir_and_validates_clean() {
        let cfg = Config::parse(GEMINI).expect("parses");
        assert_eq!(cfg.accounts[0].provider, Provider::Gemini);
        assert_eq!(
            cfg.accounts[0].config_dir.as_deref(),
            Some(Path::new("/home/example/.gemini"))
        );
        assert_eq!(cfg.accounts[0].api_key_env, None);
        assert!(
            validate(&cfg).is_empty(),
            "a well-formed gemini account must validate clean: {:?}",
            validate(&cfg)
        );
    }

    #[test]
    fn gemini_account_without_config_dir_fails_validation_naming_the_account() {
        let toml = "\
[[account]]
id = \"gemini-personal\"
label = \"Gemini Personal\"
provider = \"gemini\"
";
        let cfg = Config::parse(toml).expect("parses");
        let msgs: Vec<String> = validate(&cfg).into_iter().map(|f| f.message).collect();
        assert!(
            msgs.iter()
                .any(|m| m.contains("gemini-personal") && m.contains("config_dir")),
            "must name the account and the missing field: {msgs:?}"
        );
    }

    #[test]
    fn gemini_account_with_api_key_env_fails_validation_naming_the_account() {
        let toml = format!("{GEMINI}api_key_env = \"GEMINI_API_KEY\"\n");
        let cfg = Config::parse(&toml).expect("parses");
        let msgs: Vec<String> = validate(&cfg).into_iter().map(|f| f.message).collect();
        assert!(
            msgs.iter()
                .any(|m| m.contains("gemini-personal") && m.contains("api_key_env")),
            "an API-key gemini setup is PAYG, not a subscription — must be rejected: {msgs:?}"
        );
    }

    #[test]
    fn gemini_account_accepts_limits_overlay_true_without_a_validation_error() {
        // spec 020 §A: limits_overlay is accepted but ignored for gemini (no limits surface) —
        // setting it must never fail validation; `doctor` is where the ignored-flag note lives.
        let toml = format!("{GEMINI}limits_overlay = true\n");
        let cfg = Config::parse(&toml).expect("parses");
        assert!(cfg.accounts[0].limits_overlay);
        assert!(validate(&cfg).is_empty());
    }

    #[test]
    fn validate_environment_flags_a_gemini_accounts_missing_config_dir() {
        let toml = "\
[[account]]
id = \"gemini-personal\"
label = \"Gemini Personal\"
provider = \"gemini\"
config_dir = \"/nonexistent/gemini/home\"
";
        let cfg = Config::parse(toml).expect("parses");
        assert!(
            validate_environment(&cfg)
                .iter()
                .any(|f| f.message.contains("gemini-personal")
                    && f.message.contains("does not exist"))
        );
    }

    #[test]
    fn existing_claude_codex_zai_accounts_still_validate_after_gemini_added() {
        // Adding the Gemini variant must not perturb existing per-provider validation branches.
        let cfg = Config::parse(ONE).expect("parses");
        assert!(validate(&cfg).is_empty());
        let zai_cfg = Config::parse(ZAI).expect("parses");
        assert!(validate(&zai_cfg).is_empty());
    }
}
