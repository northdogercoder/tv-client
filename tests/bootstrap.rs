//! Bootstrap smoke tests for the `tv-client` binary and library.

use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_tv-client");

#[test]
fn version_flag_exits_zero_and_reports_name_and_version() {
    let mut command = Command::new(BIN);
    command.arg("--version");
    let output = command.output().expect("failed to run tv-client");
    let status = output.status;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(status.success(), "stderr: {stderr}");
    assert!(stdout.contains("tv-client"), "stdout: {stdout:?}");
    assert!(
        stdout.contains(env!("CARGO_PKG_VERSION")),
        "stdout: {stdout:?}"
    );
}

#[test]
fn help_flag_exits_zero_and_prints_usage() {
    let mut command = Command::new(BIN);
    command.arg("--help");
    let output = command.output().expect("failed to run tv-client");
    let status = output.status;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(status.success(), "stderr: {stderr}");
    assert!(stdout.contains("Usage"), "stdout: {stdout:?}");
}

#[test]
fn library_reports_package_version() {
    assert_eq!(tv_client::version(), env!("CARGO_PKG_VERSION"));
}
