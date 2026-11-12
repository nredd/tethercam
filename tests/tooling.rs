//! Actual utility boundaries and native coverage acceptance. No report validator fixtures.
#![cfg(feature = "repo-tools")]

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

fn tool() -> Command {
    Command::new(env!("CARGO_BIN_EXE_tethercam-tools"))
}

#[test]
fn actual_cli_fails_closed_and_preserves_normal_application() {
    for args in [
        vec![],
        vec!["unknown"],
        vec!["release-tag", "invalid"],
        vec!["usbip", "attach"],
        vec!["coverage-check", "a", "b"],
    ] {
        let output = tool()
            .args(args)
            .env_remove("GITHUB_ACTIONS")
            .output()
            .expect("process");
        assert!(!output.status.success());
        assert_ne!(output.stderr, [] as [u8; 0]);
    }
    assert!(
        tool()
            .args(["release-tag", concat!("v", env!("CARGO_PKG_VERSION"))])
            .status()
            .expect("tag")
            .success()
    );
    assert!(
        tool()
            .arg("make-help")
            .output()
            .expect("help")
            .status
            .success()
    );
}

fn isolated(program: &std::ffi::OsStr) -> Command {
    let mut cmd = Command::new(program);
    for (name, _) in std::env::vars_os() {
        if name.to_string_lossy().contains("CARGO_LLVM_COV")
            || matches!(
                name.to_str(),
                Some("RUSTC_WRAPPER" | "RUSTC_WORKSPACE_WRAPPER")
            )
        {
            cmd.env_remove(name);
        }
    }
    cmd
}

fn native(root: &Path, args: &[&str]) -> std::io::Result<Output> {
    isolated(std::ffi::OsStr::new(env!("CARGO")))
        .args(args)
        .current_dir(root)
        .env("CARGO_TARGET_DIR", root.join("target"))
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        .env_remove("RUSTFLAGS")
        .env_remove("LLVM_PROFILE_FILE")
        .env_remove("CARGO_LLVM_COV")
        .output()
}

fn gate(root: &Path) -> std::io::Result<Output> {
    isolated(std::ffi::OsStr::new(env!("CARGO_BIN_EXE_tethercam-tools")))
        .arg("coverage-gate")
        .current_dir(root)
        .env("CI", "true")
        .env("CARGO_TARGET_DIR", root.join("target"))
        .env_remove("CARGO_LLVM_COV")
        .output()
}

#[test]
fn native_gate_rejects_uncovered_nested_tests_support_empty_and_stale_profiles() {
    let temp = tempfile::tempdir().expect("fixture");
    let root = temp.path();
    fs::create_dir_all(root.join("src/tests")).expect("directories");
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname='coverage-acceptance'\nversion='0.0.0'\nedition='2024'\n",
    )
    .expect("manifest");
    let source = "pub fn used() -> u32 { 1 }\n#[cfg(test)] mod tests;\n";
    fs::write(root.join("src/lib.rs"), source).expect("source");
    fs::write(
        root.join("src/tests/mod.rs"),
        "#[test] fn exercised() { assert_eq!(crate::used(), 1); }\n",
    )
    .expect("tests");
    let collect = || {
        let output = native(root, &["llvm-cov", "clean", "--workspace"]).expect("native clean");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let output = native(root, &["llvm-cov", "--no-report"]).expect("collect");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    collect();
    let output = gate(root).expect("gate process");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    for (path, content) in [
        ("src/lib.rs", format!("{source}pub fn uncovered() -> u32 {{ 42 }}\n")),
        ("src/tests/mod.rs", "#[test] fn exercised() { assert_eq!(crate::used(), 1); }\nfn uncovered_test_helper() -> u32 { 42 }\n".into()),
        ("src/test_support.rs", "pub fn uncovered_support() -> u32 { 42 }\n".into()),
    ] {
        fs::write(root.join("src/lib.rs"), if path == "src/test_support.rs" { format!("{source}pub mod test_support;\n") } else { source.into() }).expect("reset");
        fs::write(root.join("src/tests/mod.rs"), "#[test] fn exercised() { assert_eq!(crate::used(), 1); }\n").expect("reset tests");
        fs::write(root.join(path), content).expect("mutation");
        collect();
        assert!(!gate(root).expect("gate process").status.success(), "accepted uncovered {path}");
    }
    // Cleaning removes old green profiles. An empty collection cannot reuse their success.
    assert!(
        native(root, &["llvm-cov", "clean", "--workspace"])
            .expect("clean")
            .status
            .success()
    );
    assert!(!gate(root).expect("gate process").status.success());
    // A malformed raw profile must fail native merge, even alongside otherwise valid inputs.
    collect();
    let raw = root.join("target/llvm-cov-target");
    fs::write(raw.join("malformed.profraw"), b"not a raw profile").expect("malformed profile");
    assert!(!gate(root).expect("gate process").status.success());
    fs::write(root.join("src/lib.rs"), "// No executable source\n").expect("empty aggregate");
    collect();
    assert!(!gate(root).expect("empty native gate").status.success());
}

#[test]
fn actual_verifier_uses_pipe_owned_server_and_never_opens_a_camera() {
    let temp = tempfile::tempdir().expect("temp");
    let program = temp.path().join("verification-fixture");
    assert!(
        Command::new("rustc")
            .args(["--edition=2024", "-D", "warnings"])
            .arg(
                Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/verification_client.rs")
            )
            .arg("-o")
            .arg(&program)
            .status()
            .expect("fixture compiler")
            .success()
    );
    let cfg_path = temp.path().join("cfg.toml");
    let mut cfg = tethercam::config::Config::generate().expect("cfg");
    cfg.rtsp_address = "127.0.0.1".parse().expect("ip");
    cfg.onvif_address = "127.0.0.1".parse().expect("ip");
    cfg.create(&cfg_path).expect("protected config");
    let output = temp.path().join("verification");
    let result = tool()
        .arg("verify")
        .arg("snapshot")
        .arg(&program)
        .arg(&cfg_path)
        .arg(&output)
        .args([&program, &program, &program])
        .arg("-")
        .output()
        .expect("actual CLI");
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let metrics = fs::read_to_string(output.join("profile-server.metrics"))
        .expect("native resource accounting");
    assert!(metrics.contains("real"));
    assert!(output.join("still.jpg.inspection.json").is_file());
    let result = tool()
        .args(["usbip-local", "attach"])
        .env("CI", "true")
        .output()
        .expect("guard");
    assert!(!result.status.success());
}

#[test]
fn actual_cli_broken_output_fails_before_native_gate_or_filesystem_mutation() {
    for cmd in ["coverage-gate", "make-help"] {
        let (reader, writer) = std::io::pipe().expect("pipe");
        drop(reader);
        let result = tool()
            .arg(cmd)
            .stdout(writer)
            .output()
            .expect("closed output CLI");
        assert!(!result.status.success());
        assert!(String::from_utf8_lossy(&result.stderr).contains("utility I/O failed"));
    }
}
