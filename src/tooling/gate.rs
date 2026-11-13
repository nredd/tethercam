//! Thin native cargo-llvm-cov invocation. No coverage data is parsed here.
//!
//! References:
//! - <https://github.com/taiki-e/cargo-llvm-cov#command-line-interface>

use std::ffi::OsString;
use std::io::Write;
use std::path::Path;
use std::process::Command;

use super::Error;

/// Escape a filesystem path for LLVM's regular expression filter.
fn escape(path: &Path) -> Result<String, Error> {
    let path = path
        .to_str()
        .ok_or(Error::Invalid("non-UTF-8 coverage source root"))?;
    let mut escaped = String::new();
    for ch in path.chars() {
        if "\\.^$|?*+()[]{}".contains(ch) {
            escaped.push('\\');
        }
        escaped.push(ch);
    }
    Ok(escaped)
}

/// Only top-level non-source trees and toolchain sources are outside the SRC contract.
fn arguments(root: &Path, cargo_home: &Path, local: bool) -> Result<Vec<OsString>, Error> {
    let root = escape(root)?;
    let cargo_home = escape(cargo_home)?;
    let outside = format!(
        "^{root}/(tests|examples|benches)/|^{root}/src/[^/]+/\\.\\./\\.\\./(tests|examples|benches)/|^/rustc/|^{cargo_home}/registry/"
    );
    let ignore = if local {
        format!("{outside}|^{root}/src/transport/usb/nusb_backend\\.rs$")
    } else {
        outside
    };
    Ok([
        "llvm-cov",
        "report",
        "--no-default-ignore-filename-regex",
        "--failure-mode",
        "any",
        "--fail-uncovered-lines",
        "0",
        "--fail-uncovered-functions",
        "0",
        "--fail-uncovered-regions",
        "0",
        "--fail-under-lines",
        "100",
        "--fail-under-functions",
        "100",
        "--fail-under-regions",
        "100",
        "--show-missing-lines",
        "--json",
        "--output-path",
        "coverage/coverage.json",
        "--ignore-filename-regex",
        &ignore,
    ]
    .map(OsString::from)
    .to_vec())
}

fn local(ci_present: bool, vhci: bool) -> bool {
    !ci_present && !vhci
}

fn cargo_home(
    cargo: Option<OsString>, home: Option<OsString>,
) -> Result<std::path::PathBuf, Error> {
    match (cargo, home) {
        (Some(cargo), _) => Ok(cargo.into()),
        (None, Some(home)) => Ok(Path::new(&home).join(".cargo")),
        (None, None) => Err(Error::Invalid("missing Cargo home")),
    }
}

fn invoke(
    root: &Path, cargo_home: &Path, cargo: &OsString, local: bool, out: &mut dyn Write,
) -> Result<(), Error> {
    if local {
        writeln!(
            out,
            "LOCAL ONLY: src/transport/usb/nusb_backend.rs threshold is not enforced (no VHCI)"
        )?;
    }
    std::fs::create_dir_all(root.join("coverage"))?;
    let args = arguments(root, cargo_home, local)?;
    let status = Command::new(cargo).args(args).current_dir(root).status()?;
    if !status.success() {
        return Err(Error::Invalid("native coverage gate failed"));
    }
    Ok(())
}

fn environment(
    root: std::io::Result<std::path::PathBuf>, home: Result<std::path::PathBuf, Error>,
    cargo: &OsString, local: bool, out: &mut dyn Write,
) -> Result<(), Error> {
    let root = root?.canonicalize()?;
    let home = home?.canonicalize()?;
    invoke(&root, &home, cargo, local, out)
}

pub(super) fn check(out: &mut dyn Write) -> Result<(), Error> {
    writeln!(
        out,
        "SRC: native line/function/region coverage; source tests included"
    )?;
    let local = local(
        std::env::var_os("CI").is_some(),
        Path::new("/sys/devices/platform/vhci_hcd.0").is_dir(),
    );
    environment(
        std::env::current_dir(),
        cargo_home(std::env::var_os("CARGO_HOME"), std::env::var_os("HOME")),
        &std::env::var_os("CARGO").unwrap_or(OsString::from("cargo")),
        local,
        out,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters_are_root_anchored_and_never_exclude_source_test_names() {
        for local in [false, true] {
            let args = arguments(
                Path::new("/synthetic/repo.with[chars]"),
                Path::new("/synthetic/cargo"),
                local,
            )
            .expect("args");
            let args: Vec<_> = args.iter().map(|s| s.to_str().expect("utf8")).collect();
            assert!(args.contains(&"--no-default-ignore-filename-regex"));
            for flag in [
                "--fail-uncovered-lines",
                "--fail-uncovered-functions",
                "--fail-uncovered-regions",
            ] {
                let position = args.iter().position(|s| *s == flag).expect("flag");
                assert_eq!(args.get(position + 1), Some(&"0"));
            }
            let filter = args.last().expect("filter");
            assert!(
                filter.starts_with("^/synthetic/repo\\.with\\[chars\\]/(tests|examples|benches)/")
            );
            assert_eq!(filter.contains("nusb_backend"), local);
            assert!(!filter.contains("tests\\.rs"));
        }
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            assert!(
                arguments(
                    Path::new(std::ffi::OsStr::from_bytes(&[255])),
                    Path::new("/synthetic/cargo"),
                    false
                )
                .is_err()
            );
        }
    }
    #[test]
    fn local_exemption_is_exact_and_native_commands_control_the_result() {
        assert!(local(false, false));
        assert!(!local(false, true));
        assert!(!local(true, false));
        assert!(!local(true, true));
        assert_eq!(
            cargo_home(Some("/cargo".into()), None).expect("cargo"),
            Path::new("/cargo")
        );
        assert_eq!(
            cargo_home(None, Some("/home".into())).expect("home"),
            Path::new("/home/.cargo")
        );
        assert!(cargo_home(None, None).is_err());
        let temp = tempfile::tempdir().expect("temp");
        let home = Path::new("/synthetic/cargo");
        let mut output = Vec::new();
        invoke(
            temp.path(),
            home,
            &"/usr/bin/true".into(),
            true,
            &mut output,
        )
        .expect("native result");
        assert!(
            String::from_utf8(output)
                .expect("text")
                .contains("LOCAL ONLY")
        );
        assert!(
            invoke(
                temp.path(),
                home,
                &"/usr/bin/true".into(),
                true,
                &mut crate::tests::Broken
            )
            .is_err()
        );
        assert!(
            invoke(
                temp.path(),
                home,
                &"/usr/bin/false".into(),
                false,
                &mut Vec::new()
            )
            .is_err()
        );
        assert!(
            invoke(
                temp.path(),
                home,
                &"/missing-fixture".into(),
                false,
                &mut Vec::new()
            )
            .is_err()
        );
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            assert!(
                arguments(
                    temp.path(),
                    Path::new(std::ffi::OsStr::from_bytes(&[255])),
                    false
                )
                .is_err()
            );
        }
    }
    #[test]
    fn filesystem_and_environment_failures_fail_before_native_reporting() {
        let temp = tempfile::tempdir().expect("temp");
        let cargo = OsString::from("/usr/bin/true");
        let error = || Error::Invalid("injected");
        assert!(
            environment(
                Err(std::io::Error::other("cwd")),
                Err(error()),
                &cargo,
                false,
                &mut Vec::new()
            )
            .is_err()
        );
        assert!(
            environment(
                Ok(temp.path().join("missing")),
                Err(error()),
                &cargo,
                false,
                &mut Vec::new()
            )
            .is_err()
        );
        assert!(
            environment(
                Ok(temp.path().into()),
                Err(error()),
                &cargo,
                false,
                &mut Vec::new()
            )
            .is_err()
        );
        assert!(
            environment(
                Ok(temp.path().into()),
                Ok(temp.path().join("missing")),
                &cargo,
                false,
                &mut Vec::new()
            )
            .is_err()
        );
        environment(
            Ok(temp.path().into()),
            Ok(temp.path().into()),
            &cargo,
            false,
            &mut Vec::new(),
        )
        .expect("native");
        std::fs::remove_dir(temp.path().join("coverage")).expect("remove directory");
        std::fs::write(temp.path().join("coverage"), b"not a directory").expect("file");
        assert!(invoke(temp.path(), temp.path(), &cargo, false, &mut Vec::new()).is_err());
        std::fs::remove_file(temp.path().join("coverage")).expect("remove file");
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            assert!(
                invoke(
                    temp.path(),
                    Path::new(std::ffi::OsStr::from_bytes(&[255])),
                    &cargo,
                    false,
                    &mut Vec::new()
                )
                .is_err()
            );
        }
    }
}
