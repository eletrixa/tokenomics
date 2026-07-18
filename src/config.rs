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
//! - The account list is the single source of truth; each account owns its `CLAUDE_CONFIG_DIR`.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::domain::Account;
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
            account.config_dir = expand_tilde(&account.config_dir, home.as_deref());
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

/// Filesystem checks kept separate so [`validate`] stays pure: each `config_dir` must exist.
pub fn validate_environment(cfg: &Config) -> Vec<Finding> {
    cfg.accounts
        .iter()
        .filter(|a| !a.config_dir.is_dir())
        .map(|a| {
            Finding::error(format!(
                "account '{}': config_dir {} does not exist",
                a.id,
                a.config_dir.display()
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
}
