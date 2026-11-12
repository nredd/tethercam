//! Runs the built `tethercam` binary.

use std::process::Command;

#[test]
fn binary_prints_the_banner_and_succeeds() {
    let output = Command::new(env!("CARGO_BIN_EXE_tethercam"))
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(output.stdout, tethercam::cli::BANNER.as_bytes());
    assert_eq!(output.stderr, b"");
}
