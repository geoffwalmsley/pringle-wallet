//! Black-box CLI smoke tests for the offline command paths (no network access).
//!
//! These exercise output discipline (bare address, JSON envelopes, quiet mode) and basic
//! wiring via the real binary, without ever touching mainnet.

use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::tempdir;

/// A `pringle` command with isolated key/state files under `dir`.
fn pringle(dir: &std::path::Path) -> Command {
    let mut cmd = Command::cargo_bin("pringle").unwrap();
    cmd.arg("--key-file")
        .arg(dir.join("k.hex"))
        .arg("--state-file")
        .arg(dir.join("s.json"));
    cmd
}

#[test]
fn help_lists_commands() {
    Command::cargo_bin("pringle")
        .unwrap()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("Usage"))
        .stdout(predicate::str::contains("xch"))
        .stdout(predicate::str::contains("nft"))
        .stdout(predicate::str::contains("option"))
        .stdout(predicate::str::contains("p2-singleton").not());
}

#[test]
fn xch_help_lists_wallet_subcommands() {
    Command::cargo_bin("pringle")
        .unwrap()
        .args(["xch", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("address"))
        .stdout(predicate::str::contains("coins"))
        .stdout(predicate::str::contains("coin"))
        .stdout(predicate::str::contains("consolidate"))
        .stdout(predicate::str::contains("send"));
}

#[test]
fn xch_send_help_documents_amount_and_all() {
    Command::cargo_bin("pringle")
        .unwrap()
        .args(["xch", "send", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--amount"))
        .stdout(predicate::str::contains("--all"))
        .stdout(predicate::str::contains("--fee"));
}

#[test]
fn xch_send_requires_an_amount_or_all() {
    let dir = tempdir().unwrap();
    pringle(dir.path()).arg("init").assert().success();

    // Clap rejects this before any key, state, or network access.
    pringle(dir.path())
        .args(["xch", "send", "xch1qqqqqq"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("--amount"));
}

#[test]
fn xch_send_rejects_amount_together_with_all() {
    let dir = tempdir().unwrap();
    pringle(dir.path()).arg("init").assert().success();

    pringle(dir.path())
        .args(["xch", "send", "xch1qqqqqq", "--amount", "1000", "--all"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("cannot be used with"));
}

#[test]
fn nft_help_lists_mint_address_and_sweep_without_fund() {
    Command::cargo_bin("pringle")
        .unwrap()
        .args(["nft", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("mint"))
        .stdout(predicate::str::contains("address"))
        .stdout(predicate::str::contains("sweep"))
        .stdout(predicate::str::contains("fund").not());
}

#[test]
fn option_show_all_help_exposes_history_and_cached_flags() {
    Command::cargo_bin("pringle")
        .unwrap()
        .args(["option", "show-all", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--include-closed"))
        .stdout(predicate::str::contains("--cached"));
}

#[test]
fn option_inspect_help_documents_the_lookup_and_prompt() {
    Command::cargo_bin("pringle")
        .unwrap()
        .args(["option", "inspect", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--fee"))
        .stdout(predicate::str::contains("p2"))
        .stdout(predicate::str::contains("take"));
}

#[test]
fn option_inspect_rejects_a_file_that_is_not_an_offer() {
    let dir = tempdir().unwrap();
    pringle(dir.path()).arg("init").assert().success();

    let offer_file = dir.path().join("not.offer");
    std::fs::write(&offer_file, "definitely not an offer\n").unwrap();

    // Decoding fails before any network access, so this stays offline.
    pringle(dir.path())
        .args(["option", "inspect"])
        .arg(&offer_file)
        .assert()
        .failure()
        .stderr(predicate::str::contains("failed to decode offer file"));
}

#[test]
fn option_clawback_help_documents_creator_only_expiry_flow() {
    Command::cargo_bin("pringle")
        .unwrap()
        .args(["option", "clawback", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--launcher"))
        .stdout(predicate::str::contains("--address"))
        .stdout(predicate::str::contains("--fee"))
        .stdout(predicate::str::contains("expired"));
}

#[test]
fn init_creates_key_and_state() {
    let dir = tempdir().unwrap();
    pringle(dir.path())
        .arg("init")
        .assert()
        .success()
        .stdout(predicate::str::contains("Initialized new wallet"));
    assert!(dir.path().join("k.hex").exists());
    assert!(dir.path().join("s.json").exists());
}

#[test]
fn address_prints_only_the_address_by_default() {
    let dir = tempdir().unwrap();
    pringle(dir.path()).arg("init").assert().success();

    let out = pringle(dir.path())
        .args(["xch", "address"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).unwrap();
    let lines: Vec<&str> = stdout.lines().collect();
    // Exactly one line, and it is the bech32m address.
    assert_eq!(
        lines.len(),
        1,
        "expected a single bare address line, got {stdout:?}"
    );
    assert!(lines[0].starts_with("xch1"));
}

#[test]
fn address_verbose_includes_puzzle_hash() {
    let dir = tempdir().unwrap();
    pringle(dir.path()).arg("init").assert().success();

    pringle(dir.path())
        .args(["xch", "address"])
        .arg("--verbose")
        .assert()
        .success()
        .stdout(predicate::str::contains("puzzle hash:"));
}

#[test]
fn address_json_is_a_valid_envelope() {
    let dir = tempdir().unwrap();
    pringle(dir.path()).arg("init").assert().success();

    let out = pringle(dir.path())
        .arg("--json")
        .args(["xch", "address"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(value["schema_version"], 1);
    assert_eq!(value["ok"], true);
    assert_eq!(value["kind"], "address");
    assert!(value["address"].as_str().unwrap().starts_with("xch1"));
    assert!(value["puzzle_hash"].as_str().unwrap().starts_with("0x"));
}

#[test]
fn status_cached_json_is_empty_but_valid() {
    let dir = tempdir().unwrap();
    pringle(dir.path()).arg("init").assert().success();

    let out = pringle(dir.path())
        .arg("--json")
        .arg("status")
        .arg("--cached")
        .output()
        .unwrap();
    assert!(out.status.success());
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(value["kind"], "status");
    assert!(value["nfts"].as_array().unwrap().is_empty());
    assert!(value["options"].as_array().unwrap().is_empty());
    assert!(value["p2_singletons"].as_array().unwrap().is_empty());
}

#[test]
fn missing_key_gives_a_clear_error() {
    let dir = tempdir().unwrap();
    // No `init`: address should fail with a helpful message and a non-zero exit code.
    pringle(dir.path())
        .args(["xch", "address"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("no key file"));
}
