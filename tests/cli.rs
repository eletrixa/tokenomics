//! CLI black-box tests for the `tok` binary.
//!
//! Project: Tokenomics — monitor LLM subscription accounts
//! Module:  tests/cli.rs
//! Deps:    assert_cmd, predicates, tempfile
//! Tested:  self (this IS the test surface for the CLI dispatch)
//!
//! Key responsibilities:
//! - `--help`/`--version` succeed; unknown commands exit 2.
//! - `validate` accepts a good config (exit 0) and flags errors (exit 1); `accounts` lists accounts.
//! - spec 014: `accounts` marks inactive accounts; `once --json` omits them; `doctor` labels them
//!   and skips their overlay probe.
//! - spec 015: `doctor` stays silent about config divergence when the store is absent, and also when
//!   a one-shot collector's frozen heartbeat can't prove it had time to reload the later edit (the
//!   false-positive gate — the true-positive warn is covered by `doctor`'s pure unit tests, which a
//!   black-box CLI run can't set up: it needs a live daemon still heartbeating past the edit).

use std::fs;

use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;

fn tok() -> Command {
    Command::cargo_bin("tok").expect("binary `tok` builds")
}

/// Write a `tokenomics.toml` into a fresh temp dir. `{DIR}` in `body` is replaced with the temp dir
/// path, so an account's `config_dir` points at an existing directory.
fn with_config(body: &str) -> TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    let toml = body.replace("{DIR}", dir.path().to_str().expect("utf-8 temp path"));
    fs::write(dir.path().join("tokenomics.toml"), toml).expect("write config");
    dir
}

/// A `tok` command pointed at the temp dir's config via `$TOKENOMICS_CONFIG` (path resolution is
/// cwd-independent, so tests set the override rather than relying on the working directory).
fn tok_with(dir: &TempDir) -> Command {
    let mut cmd = tok();
    cmd.env("TOKENOMICS_CONFIG", dir.path().join("tokenomics.toml"));
    // Isolate from the ambient environment: a developer machine with a real `TOKENOMICS_LEDGER`
    // exported would otherwise make `doctor_reports_ledger_not_configured_when_unset` fail, and
    // every other doctor test would silently read the developer's real ledger file.
    cmd.env_remove("TOKENOMICS_LEDGER");
    cmd
}

#[test]
fn help_succeeds_and_lists_usage() {
    tok()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("USAGE:").and(predicate::str::contains("tok validate")));
}

#[test]
fn version_prints_crate_version() {
    tok()
        .arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::contains("tok "));
}

#[test]
fn unknown_command_exits_two() {
    tok()
        .arg("wat")
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("unknown command"));
}

#[test]
fn validate_accepts_a_good_config() {
    let dir = with_config(
        "\
[[account]]
id = \"a\"
label = \"Account A\"
provider = \"claude\"
config_dir = \"{DIR}\"
",
    );
    tok_with(&dir)
        .arg("validate")
        .assert()
        .success()
        .stdout(predicate::str::contains("no errors"));
}

#[test]
fn validate_flags_duplicate_ids_with_exit_one() {
    let dir = with_config(
        "\
[[account]]
id = \"dup\"
label = \"One\"
provider = \"claude\"
config_dir = \"{DIR}\"
[[account]]
id = \"dup\"
label = \"Two\"
provider = \"claude\"
config_dir = \"{DIR}\"
",
    );
    tok_with(&dir)
        .arg("validate")
        .assert()
        .failure()
        .code(1)
        .stdout(predicate::str::contains("duplicate account id"));
}

#[test]
fn accounts_lists_configured_accounts() {
    let dir = with_config(
        "\
[[account]]
id = \"mine\"
label = \"Mine\"
provider = \"claude\"
config_dir = \"{DIR}\"
limits_overlay = true
",
    );
    tok_with(&dir)
        .arg("accounts")
        .assert()
        .success()
        .stdout(predicate::str::contains("mine").and(predicate::str::contains("overlay:on")));
}

#[test]
fn accounts_marks_inactive_accounts() {
    let dir = with_config(
        "\
[[account]]
id = \"alive\"
label = \"Alive\"
provider = \"claude\"
config_dir = \"{DIR}\"
[[account]]
id = \"dead\"
label = \"Dead\"
provider = \"claude\"
config_dir = \"{DIR}\"
active = false
",
    );
    // The inactive marker sits on "dead"'s line specifically, not "alive"'s.
    let output = tok_with(&dir).arg("accounts").output().expect("run");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let alive_line = stdout
        .lines()
        .find(|l| l.starts_with("alive"))
        .expect("alive line");
    let dead_line = stdout
        .lines()
        .find(|l| l.starts_with("dead"))
        .expect("dead line");
    assert!(!alive_line.contains("(inactive)"), "alive: {alive_line}");
    assert!(dead_line.contains("(inactive)"), "dead: {dead_line}");
}

#[test]
fn once_json_omits_inactive_accounts() {
    // ccusage need not be installed for this: an inactive account must produce no JSON record at
    // all (spec 014 §D), regardless of whether the active account's collection itself succeeds.
    let dir = with_config(
        "\
[[account]]
id = \"alive\"
label = \"Alive\"
provider = \"claude\"
config_dir = \"{DIR}\"
[[account]]
id = \"dead\"
label = \"Dead\"
provider = \"claude\"
config_dir = \"{DIR}\"
active = false
",
    );
    let output = tok_with(&dir)
        .args(["once", "--json"])
        .output()
        .expect("run");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let records: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("valid JSON output");
    let records = records.as_array().expect("a JSON array");
    assert_eq!(records.len(), 1, "records: {records:?}");
    assert_eq!(records[0]["account"], "alive");
}

#[test]
fn doctor_labels_inactive_and_skips_its_overlay_probe() {
    let dir = with_config(
        "\
[[account]]
id = \"dead\"
label = \"Dead\"
provider = \"claude\"
config_dir = \"{DIR}\"
active = false
limits_overlay = true
",
    );
    tok_with(&dir).arg("doctor").assert().success().stdout(
        predicate::str::contains("INACTIVE")
            .and(predicate::str::contains("skipped — account inactive")),
    );
}

#[test]
fn doctor_silent_about_divergence_when_collector_not_demonstrably_failing() {
    // Spec 015 §B / acceptance 5 (false-positive gate): a one-shot `collector --once` stamps the
    // config load AND its heartbeat at the same instant, then exits. A later edit makes the file
    // newer than the recorded load — but the frozen heartbeat never advances past the edit, so the
    // collector is NOT demonstrably failing to reload (it isn't even running). Doctor must stay
    // silent. (The true-positive warn needs a live daemon still heartbeating past the edit, which a
    // black-box CLI run can't set up; it's covered by doctor's pure `config_divergence_hint` tests.)
    let dir = with_config(
        "\
[[account]]
id = \"a\"
label = \"Account A\"
provider = \"claude\"
config_dir = \"{DIR}\"
",
    );
    let db = dir.path().join("collector.db");
    tok_with(&dir)
        .env("TOKENOMICS_DB", &db)
        .args(["collector", "--once"])
        .assert()
        .success();

    // A later edit: strictly newer mtime than the recorded load (>1s guards the ms-granular stamp).
    std::thread::sleep(std::time::Duration::from_millis(1100));
    let body = format!(
        "[[account]]\nid = \"a\"\nlabel = \"Account A\"\nprovider = \"claude\"\nconfig_dir = \"{}\"\n",
        dir.path().to_str().expect("utf-8 temp path")
    );
    fs::write(dir.path().join("tokenomics.toml"), body).expect("touch config");

    tok_with(&dir)
        .env("TOKENOMICS_DB", &db)
        .arg("doctor")
        .assert()
        .success()
        .stdout(predicate::str::contains("config divergence").not());
}

#[test]
fn doctor_is_silent_about_divergence_when_the_store_is_absent() {
    // Spec 015 §B / acceptance 5: with no store recorded, doctor prints nothing new (and must not
    // create the store just to diagnose).
    let dir = with_config(
        "\
[[account]]
id = \"a\"
label = \"Account A\"
provider = \"claude\"
config_dir = \"{DIR}\"
",
    );
    let absent_db = dir.path().join("absent.db");
    tok_with(&dir)
        .env("TOKENOMICS_DB", &absent_db)
        .arg("doctor")
        .assert()
        .success()
        .stdout(predicate::str::contains("config divergence").not());
    assert!(!absent_db.exists(), "doctor must not create the store file");
}

#[test]
fn doctor_reports_ledger_provenance_freshness_parse_errors_and_join_divergence() {
    // Spec 017 §E / acceptance 8: `tok doctor`'s ledger section, exercised end-to-end through the
    // real CLI (path resolution → poll → parse → print), not just the pure helpers unit-tested in
    // `doctor.rs`. All ids/dates are synthetic (`claude-*`, made-up dates) per spec 017 §F.
    let dir = with_config(
        "\
[[account]]
id = \"claude-alpha\"
label = \"Alpha\"
provider = \"claude\"
config_dir = \"{DIR}\"

[[account]]
id = \"claude-orphan-config\"
label = \"Orphan Config\"
provider = \"claude\"
config_dir = \"{DIR}\"
",
    );
    let ledger_path = dir.path().join("subscriptions.toml");
    fs::write(
        &ledger_path,
        "\
[[subscription]]
id = \"claude-alpha\"
status = \"active\"
renews = 2000-01-01

[[subscription]]
id = \"claude-bravo\"
status = \"canceled\"

[[subscription]]
id = \"claude-orphan-ledger\"
status = \"active\"
",
    )
    .expect("write ledger fixture");

    tok_with(&dir)
        .env("TOKENOMICS_LEDGER", &ledger_path)
        .arg("doctor")
        .assert()
        .success()
        .stdout(
            predicate::str::contains("ledger: ")
                .and(predicate::str::contains("[fresh]"))
                .and(predicate::str::contains("ledger past-dated: claude-alpha"))
                .and(predicate::str::contains(
                    "ledger parse error (claude-bravo): unknown status 'canceled'",
                ))
                .and(predicate::str::contains(
                    "ledger divergence: config account(s) with no ledger row: claude-orphan-config",
                ))
                .and(predicate::str::contains(
                    "ledger divergence: ledger row(s) with no matching config account: \
                     claude-orphan-ledger",
                )),
        );
}

#[test]
fn doctor_reports_the_reason_a_wholly_unparseable_ledger_is_stale() {
    // A blanked row is diagnosable via "ledger parse error (id): reason"; a wholly unparseable
    // FILE must be diagnosable too — `[stale]` alone names no reason (adversarial-review finding).
    let dir = with_config(
        "\
[[account]]
id = \"claude-alpha\"
label = \"Alpha\"
provider = \"claude\"
config_dir = \"{DIR}\"
",
    );
    let ledger_path = dir.path().join("subscriptions.toml");
    fs::write(&ledger_path, "this is [[[ not toml at all").expect("write malformed ledger");

    tok_with(&dir)
        .env("TOKENOMICS_LEDGER", &ledger_path)
        .arg("doctor")
        .assert()
        .success()
        .stdout(
            predicate::str::contains("[stale]")
                .and(predicate::str::contains("ledger stale reason:")),
        );
}

#[test]
fn doctor_reports_ledger_not_configured_when_unset() {
    // Spec 017 §B/§E: both `TOKENOMICS_LEDGER` and `[settings] ledger_path` unset ⇒ `Off`, and
    // `doctor` says so plainly rather than staying silent about the plane's existence.
    let dir = with_config(
        "\
[[account]]
id = \"a\"
label = \"Account A\"
provider = \"claude\"
config_dir = \"{DIR}\"
",
    );
    tok_with(&dir)
        .arg("doctor")
        .assert()
        .success()
        .stdout(predicate::str::contains("ledger: not configured"));
}

#[test]
fn resolution_is_cwd_independent_and_ignores_a_decoy_in_the_working_dir() {
    // The real config lives in `cfg_dir`. The process runs in an unrelated `run_dir` that even
    // holds a DECOY `tokenomics.toml` — proof the old cwd pickup is gone: only the env override
    // is honored, so `tok` behaves the same from any directory (including inside the repo).
    let cfg_dir = with_config(
        "\
[[account]]
id = \"real\"
label = \"Real\"
provider = \"claude\"
config_dir = \"{DIR}\"
",
    );
    let run_dir = tempfile::tempdir().expect("run tempdir");
    fs::write(
        run_dir.path().join("tokenomics.toml"),
        "[[account]]\nid = \"decoy\"\nlabel = \"Decoy\"\nprovider = \"claude\"\nconfig_dir = \"/tmp\"\n",
    )
    .expect("write decoy config");
    tok_with(&cfg_dir)
        .current_dir(run_dir.path())
        .arg("accounts")
        .assert()
        .success()
        .stdout(predicate::str::contains("real").and(predicate::str::contains("decoy").not()));
}

#[test]
fn init_creates_a_config_that_validates_clean() {
    // spec 016 §A: `tok init` writes a starter config (creating the parent dir) at the resolved
    // path, and what it writes parses AND validates cleanly. HOME is pointed at a temp dir with a
    // real `.claude` so the `~/.claude` account's config_dir passes the environment check too.
    let home = tempfile::tempdir().expect("home");
    fs::create_dir(home.path().join(".claude")).expect("mk ~/.claude");
    let cfg = home.path().join("cfg").join("tokenomics.toml"); // parent "cfg" does not exist yet

    tok()
        .env("HOME", home.path())
        .env("TOKENOMICS_CONFIG", &cfg)
        .arg("init")
        .assert()
        .success()
        .stdout(predicate::str::contains(cfg.to_str().expect("utf-8 path")));
    assert!(
        cfg.exists(),
        "init created the config file (and its parent dir)"
    );

    tok()
        .env("HOME", home.path())
        .env("TOKENOMICS_CONFIG", &cfg)
        .arg("validate")
        .assert()
        .success()
        .stdout(predicate::str::contains("no errors"));
}

#[test]
fn init_refuses_to_overwrite_existing_config() {
    // spec 016 §B: a second init at an occupied path exits 1, names the path, says it exists, and
    // leaves the original bytes untouched.
    let dir = tempfile::tempdir().expect("dir");
    let cfg = dir.path().join("tokenomics.toml");
    tok()
        .env("TOKENOMICS_CONFIG", &cfg)
        .arg("init")
        .assert()
        .success();
    let original = fs::read(&cfg).expect("read after first init");

    tok()
        .env("TOKENOMICS_CONFIG", &cfg)
        .arg("init")
        .assert()
        .failure()
        .code(1)
        .stderr(
            predicate::str::contains("exists")
                .and(predicate::str::contains(cfg.to_str().expect("utf-8 path"))),
        );
    assert_eq!(
        fs::read(&cfg).expect("read after refusal"),
        original,
        "init must not overwrite"
    );
}

#[test]
fn init_config_has_overlay_off() {
    // spec 016 §A: the starter ships the overlay opt-in OFF.
    let dir = tempfile::tempdir().expect("dir");
    let cfg = dir.path().join("tokenomics.toml");
    tok()
        .env("TOKENOMICS_CONFIG", &cfg)
        .arg("init")
        .assert()
        .success();
    let text = fs::read_to_string(&cfg).expect("read config");
    assert!(
        text.contains("limits_overlay = false"),
        "overlay off:\n{text}"
    );
}

#[test]
fn missing_config_suggests_tok_init() {
    // spec 016 §C: a config-loading command pointed at a nonexistent path hints at `tok init`.
    let dir = tempfile::tempdir().expect("dir");
    let absent = dir.path().join("nope.toml");
    tok()
        .env("TOKENOMICS_CONFIG", &absent)
        .arg("validate")
        .assert()
        .failure()
        .stderr(predicate::str::contains("run `tok init`"));
}
