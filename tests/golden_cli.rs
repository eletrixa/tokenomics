//! R16 golden-snapshot gate: freezes `tok accounts` and `tok once --json` byte-for-byte ahead of
//! spec 017 (subscription dates / ledger plane).
//!
//! Project: Tokenomics — monitor LLM subscription accounts
//! Module:  tests/golden_cli.rs
//! Deps:    assert_cmd, tempfile (both already dev-dependencies — nothing new added)
//! Tested:  self — this IS the R16 gate (specs/017-subscription-dates.md §E, acceptance 8)
//!
//! Key responsibilities:
//! - Run the built `tok` binary against a synthetic fixture (`fixtures/golden_accounts.toml` —
//!   made-up ids/dirs, never a real account) and capture `accounts` / `once --json` stdout.
//! - Normalize the one volatile field — the ccusage subprocess error text embedded in `once
//!   --json`'s `"error"` value (its exact wording depends on whether ccusage is installed on this
//!   machine, and which version) — to `<ERR>`, then diff against the committed goldens under
//!   `tests/golden/`. That collapse IS the golden contract for this wave; see [`normalize`].
//! - Run each command twice and assert the two normalized outputs agree with each other before
//!   comparing either to the committed golden, so a golden can never be pinned from a fluke.
//!
//! Design constraints:
//! - Spec 017 §E: `tok accounts` and `tok once --json` are UNCHANGED this wave. If either test
//!   here goes red, spec 017's change touched a frozen CLI surface — update the spec + these
//!   goldens together, deliberately, never as an incidental side effect.
//! - No `~` in the fixture's `config_dir`s: `Config::parse` tilde-expands against `$HOME`, which
//!   would make the fixture (and this golden) machine-dependent. Paths are absolute, synthetic, and
//!   never resolved on disk (`validate`/`load_valid_config` never checks `config_dir` existence —
//!   only `tok validate`'s `validate_environment` does, which neither command under test calls).

use std::path::PathBuf;

use assert_cmd::Command;
use tempfile::TempDir;

/// The synthetic fixture config — see its own header comment for the "never real data" contract.
fn fixture_config_path() -> PathBuf {
    PathBuf::from(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/fixtures/golden_accounts.toml"
    ))
}

/// Read a committed golden file (path relative to the crate root, matching the fixture above).
fn read_golden(name: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden")
        .join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read golden {}: {e}", path.display()))
}

/// A `tok` invocation pinned at the fixture config and a fresh, never-pre-created store path (a
/// stray `collector` run elsewhere must never leak into this golden).
fn tok(db_dir: &TempDir) -> Command {
    let mut cmd = Command::cargo_bin("tok").expect("binary `tok` builds");
    cmd.env("TOKENOMICS_CONFIG", fixture_config_path());
    cmd.env("TOKENOMICS_DB", db_dir.path().join("tokenomics.db"));
    // Belt-and-braces: neither surface under test reads the ledger, but isolate from an ambient
    // `TOKENOMICS_LEDGER` anyway so this golden can never depend on the developer's environment.
    cmd.env_remove("TOKENOMICS_LEDGER");
    cmd
}

/// Run `tok <args>`, capturing stdout regardless of exit code. Neither surface under test is
/// expected to exit nonzero against this fixture today, but the golden's job is to freeze stdout —
/// so this stays robust even if a future change makes per-account collection failures exit nonzero.
fn run(db_dir: &TempDir, args: &[&str]) -> String {
    let output = tok(db_dir).args(args).output().expect("tok runs");
    String::from_utf8(output.stdout).expect("stdout is UTF-8")
}

/// The golden-contract normalization. The only volatile content either surface can produce is the
/// ccusage subprocess error text (`once --json`'s `"error"` field): whether ccusage is installed at
/// all, and its exact version, changes that string's wording. Every line shaped like `"error":
/// "..."` has its value collapsed to `<ERR>`, preserving indentation and any trailing comma.
/// `accounts` has no volatile fields (no timestamps; the fixture's `config_dir`s are literal, never
/// a tempdir), so this is a no-op there — one function covers both surfaces so the contract lives
/// in exactly one place.
fn normalize(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("\"error\": \"") {
            let indent = &line[..line.len() - trimmed.len()];
            let comma = if line.trim_end().ends_with(',') {
                ","
            } else {
                ""
            };
            out.push_str(indent);
            out.push_str("\"error\": \"<ERR>\"");
            out.push_str(comma);
        } else {
            out.push_str(line);
        }
        out.push('\n');
    }
    out
}

#[test]
fn accounts_output_is_frozen() {
    let db_dir = tempfile::tempdir().expect("tempdir");
    let run1 = normalize(&run(&db_dir, &["accounts"]));
    let run2 = normalize(&run(&db_dir, &["accounts"]));
    assert_eq!(
        run1, run2,
        "tok accounts is not byte-identical run-to-run (post-normalization)"
    );
    assert_eq!(
        run1,
        read_golden("accounts.txt"),
        "tok accounts output changed — this CLI surface is FROZEN for spec 017 (R16; see \
         specs/017-subscription-dates.md §E acceptance 8). If this is a deliberate, spec-approved \
         change, update tests/golden/accounts.txt (and the spec) in the same commit."
    );
}

#[test]
fn once_json_output_is_frozen() {
    let db_dir = tempfile::tempdir().expect("tempdir");
    let run1 = normalize(&run(&db_dir, &["once", "--json"]));
    let run2 = normalize(&run(&db_dir, &["once", "--json"]));
    assert_eq!(
        run1, run2,
        "tok once --json is not byte-identical run-to-run (post-normalization)"
    );
    assert_eq!(
        run1,
        read_golden("once.json"),
        "tok once --json output changed — this CLI surface is FROZEN for spec 017 (R16; see \
         specs/017-subscription-dates.md §E acceptance 8). If this is a deliberate, spec-approved \
         change, update tests/golden/once.json (and the spec) in the same commit."
    );
}
