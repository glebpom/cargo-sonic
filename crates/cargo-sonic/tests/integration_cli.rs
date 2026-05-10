//! End-to-end CLI tests that exercise the `cargo-sonic` binary as a child
//! process. These cover argv parsing, help text, and the early-exit error
//! paths that would otherwise live behind a real cargo build (e.g. the
//! Linux-target enforcement and the `cannot identify package` rejection).
//!
//! These tests do not invoke a real cargo build; they exit before reaching
//! the codegen pipeline. That keeps them fast and OS-agnostic — they pass on
//! macOS / Linux / non-x86 hosts alike.

use assert_cmd::Command;
use predicates::prelude::*;

fn cargo_sonic() -> Command {
    Command::cargo_bin("cargo-sonic").expect("cargo-sonic binary built by cargo test")
}

#[test]
fn root_help_lists_sonic_subcommand() {
    cargo_sonic()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("sonic"))
        .stdout(predicate::str::contains("Usage: cargo-sonic"));
}

#[test]
fn root_help_short_flag_matches_long() {
    let long = cargo_sonic()
        .arg("--help")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let short = cargo_sonic()
        .arg("-h")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    assert_eq!(long, short, "-h and --help should agree");
}

#[test]
fn no_args_prints_usage_with_exit_2() {
    cargo_sonic()
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("Usage: cargo-sonic"));
}

#[test]
fn unknown_top_level_subcommand_exits_2() {
    cargo_sonic()
        .arg("not-a-subcommand")
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("unrecognized subcommand"));
}

#[test]
fn unsupported_version_flag_exits_2() {
    // We never opted into clap's automatic --version, so it must surface as
    // an unrecognized flag rather than printing a version string.
    cargo_sonic()
        .arg("--version")
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("unexpected argument"));
}

#[test]
fn sonic_help_advertises_three_phases() {
    cargo_sonic()
        .args(["sonic", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("build"))
        .stdout(predicate::str::contains("probe"))
        .stdout(predicate::str::contains("score"));
}

#[test]
fn sonic_help_lists_compression_and_loader_enums() {
    cargo_sonic()
        .args(["sonic", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--compress"))
        .stdout(predicate::str::contains("--loader"))
        .stdout(predicate::str::contains("--target-cpus"))
        .stdout(predicate::str::contains("--parallelism"))
        .stdout(predicate::str::contains("--auditable"));
}

#[test]
fn sonic_with_no_subcommand_exits_2_with_usage() {
    cargo_sonic()
        .arg("sonic")
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("Usage: cargo-sonic sonic"));
}

#[test]
fn sonic_build_help_renders_cargo_args_arg() {
    cargo_sonic()
        .args(["sonic", "build", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("CARGO_ARGS"));
}

#[test]
fn sonic_probe_help_renders_cargo_args_arg() {
    cargo_sonic()
        .args(["sonic", "probe", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("CARGO_ARGS"));
}

#[test]
fn sonic_score_help_renders_cargo_args_arg() {
    cargo_sonic()
        .args(["sonic", "score", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("CARGO_ARGS"));
}

#[test]
fn invalid_compression_value_lists_possible_values() {
    cargo_sonic()
        .args(["sonic", "--compress", "definitely-not-a-codec", "build"])
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("invalid value"))
        .stderr(predicate::str::contains("none"))
        .stderr(predicate::str::contains("zstd"));
}

#[test]
fn invalid_loader_strategy_lists_possible_values() {
    cargo_sonic()
        .args(["sonic", "--loader", "tape", "build"])
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("invalid value"))
        .stderr(predicate::str::contains("embedded"))
        .stderr(predicate::str::contains("bundle"));
}

#[test]
fn invalid_parallelism_value_is_rejected_by_clap() {
    cargo_sonic()
        .args(["sonic", "--parallelism", "abc", "build"])
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("invalid value"));
}

#[test]
fn score_runs_through_the_anyhow_error_pipeline() {
    // Score with no extra args takes the early-exit path inside
    // `cargo_sonic::score` (either `linux-only target` on non-Linux, or
    // `cannot identify package` on Linux without a real Cargo workspace).
    // Either way, the binary must exit with status 1 (error from anyhow,
    // not 2 from clap argument parsing).
    let assertion = cargo_sonic().args(["sonic", "score"]).assert().failure();
    let output = assertion.get_output();
    assert_ne!(
        output.status.code(),
        Some(2),
        "score must not be a clap arg error"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.starts_with("Error: "), "unexpected stderr: {stderr}");
}

#[test]
fn build_without_target_cpus_takes_runtime_error_path() {
    // Same shape as score: must not be a clap parse error, must surface
    // an anyhow `Error:`-prefixed line on stderr.
    let assertion = cargo_sonic()
        .args(["sonic", "--target-cpus", "haswell", "build"])
        .assert()
        .failure();
    let output = assertion.get_output();
    assert_ne!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.starts_with("Error: "), "unexpected stderr: {stderr}");
}

#[test]
fn probe_takes_runtime_error_path() {
    let assertion = cargo_sonic()
        .args(["sonic", "--target-cpus", "haswell", "probe"])
        .assert()
        .failure();
    let output = assertion.get_output();
    assert_ne!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.starts_with("Error: "), "unexpected stderr: {stderr}");
}

#[test]
fn target_cpus_value_delimiter_splits_on_commas() {
    // Comma-separated list parses without rejection at the clap layer; the
    // failure (when it comes) is from cargo metadata, not arg parsing.
    let assertion = cargo_sonic()
        .args([
            "sonic",
            "--target-cpus",
            "haswell,znver5,neoverse-n1",
            "probe",
        ])
        .assert()
        .failure();
    assert_ne!(assertion.get_output().status.code(), Some(2));
}

#[test]
fn auditable_flag_is_accepted_for_build() {
    let assertion = cargo_sonic()
        .args(["sonic", "--target-cpus", "haswell", "--auditable", "build"])
        .assert()
        .failure();
    // Reaches anyhow path, not clap path.
    assert_ne!(assertion.get_output().status.code(), Some(2));
}
