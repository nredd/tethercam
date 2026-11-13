//! Thin native cargo-llvm-cov invocation. No coverage data is parsed here.
//!
//! References:
//! - <https://github.com/taiki-e/cargo-llvm-cov#command-line-interface>

use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
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
/// Standard-library sources live under `/rustc/<hash>/` unless the toolchain ships `rust-src`,
/// in which case rustc maps them into the sysroot, so both locations are excluded.
fn ignore(root: &Path, cargo_home: &Path, sysroot: &Path, local: bool) -> Result<String, Error> {
    let root = escape(root)?;
    let cargo_home = escape(cargo_home)?;
    let sysroot = escape(sysroot)?;
    let outside = format!(
        "^{root}/(tests|examples|benches)/|^{root}/src/[^/]+/\\.\\./\\.\\./(tests|examples|benches)/|^/rustc/|^{sysroot}/lib/rustlib/src/|^{cargo_home}/registry/"
    );
    Ok(if local {
        format!("{outside}|^{root}/src/transport/usb/nusb_backend\\.rs$")
    } else {
        outside
    })
}

fn arguments(ignore: &str) -> Vec<OsString> {
    [
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
        ignore,
    ]
    .map(OsString::from)
    .to_vec()
}

/// The same filter as a text report, where `^0` under a line marks an unexecuted region.
fn text_arguments(ignore: &str) -> Vec<OsString> {
    [
        "llvm-cov",
        "report",
        "--no-default-ignore-filename-regex",
        "--text",
        "--output-path",
        TEXT_REPORT,
        "--ignore-filename-regex",
        ignore,
    ]
    .map(OsString::from)
    .to_vec()
}

const TEXT_REPORT: &str = "coverage/uncovered.txt";

/// The compiler that instrumented the tests. Region accounting changes between rustc releases.
struct Toolchain {
    release: String,
    sysroot: PathBuf,
}

fn rustc_output(rustc: &OsString, args: [&str; 2]) -> Result<String, Error> {
    let output = Command::new(rustc).args(args).output()?;
    if !output.status.success() {
        return Err(Error::Invalid("cannot query the Rust toolchain"));
    }
    String::from_utf8(output.stdout).map_err(|_| Error::Invalid("non-UTF-8 Rust toolchain output"))
}

fn toolchain(rustc: &OsString) -> Result<Toolchain, Error> {
    let version = rustc_output(rustc, ["--version", "--verbose"])?;
    let release = version.lines().next().unwrap_or_default().to_owned();
    let sysroot = rustc_output(rustc, ["--print", "sysroot"])?;
    Ok(Toolchain {
        release,
        sysroot: PathBuf::from(sysroot.trim_end()).canonicalize()?,
    })
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
    root: &Path, cargo_home: &Path, toolchain: &Toolchain, cargo: &OsString, local: bool,
    out: &mut dyn Write,
) -> Result<(), Error> {
    if local {
        writeln!(
            out,
            "LOCAL ONLY: src/transport/usb/nusb_backend.rs threshold is not enforced (no VHCI)"
        )?;
    }
    writeln!(out, "TOOLCHAIN: {}", toolchain.release)?;
    std::fs::create_dir_all(root.join("coverage"))?;
    let ignore = ignore(root, cargo_home, &toolchain.sysroot, local)?;
    let status = Command::new(cargo)
        .args(arguments(&ignore))
        .current_dir(root)
        .status()?;
    if !status.success() {
        report_failure(root, cargo, &ignore, out)?;
        return Err(Error::Invalid("native coverage gate failed"));
    }
    Ok(())
}

/// The native summary lists uncovered lines only, so a region-only failure shows no location.
fn report_failure(
    root: &Path, cargo: &OsString, ignore: &str, out: &mut dyn Write,
) -> Result<(), Error> {
    let text = Command::new(cargo)
        .args(text_arguments(ignore))
        .current_dir(root)
        .status();
    if text.is_ok_and(|status| status.success()) {
        writeln!(
            out,
            "Only lines are listed above. Regions and functions: grep -n -B1 '\\^0' {TEXT_REPORT}"
        )?;
    }
    writeln!(
        out,
        "Region accounting differs between rustc releases and CI floats on `stable`: compare the TOOLCHAIN line with the CI log, `rustup update stable` if older"
    )?;
    Ok(())
}

fn environment(
    root: std::io::Result<std::path::PathBuf>, home: Result<std::path::PathBuf, Error>,
    toolchain: Result<Toolchain, Error>, cargo: &OsString, local: bool, out: &mut dyn Write,
) -> Result<(), Error> {
    let root = root?.canonicalize()?;
    let home = home?.canonicalize()?;
    invoke(&root, &home, &toolchain?, cargo, local, out)
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
        toolchain(&std::env::var_os("RUSTC").unwrap_or(OsString::from("rustc"))),
        &std::env::var_os("CARGO").unwrap_or(OsString::from("cargo")),
        local,
        out,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_toolchain() -> Toolchain {
        Toolchain {
            release: "rustc 0.0.0 (synthetic)".into(),
            sysroot: "/synthetic/sysroot".into(),
        }
    }

    /// Accepts this many complete lines, then fails.
    struct FailAfterLines(usize);
    impl Write for FailAfterLines {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if self.0 == 0 {
                return Err(std::io::Error::other("synthetic output failure"));
            }
            self.0 -= usize::from(bytes.contains(&b'\n'));
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[cfg(unix)]
    fn script(dir: &Path, name: &str, body: &str) -> OsString {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("script");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("mode");
        path.into()
    }

    #[test]
    fn filters_are_root_anchored_and_never_exclude_source_test_names() {
        for local in [false, true] {
            let filter = ignore(
                Path::new("/synthetic/repo.with[chars]"),
                Path::new("/synthetic/cargo"),
                Path::new("/synthetic/sys.root"),
                local,
            )
            .expect("filter");
            assert!(
                filter.starts_with("^/synthetic/repo\\.with\\[chars\\]/(tests|examples|benches)/")
            );
            assert!(filter.contains("|^/rustc/|^/synthetic/sys\\.root/lib/rustlib/src/|"));
            assert_eq!(filter.contains("nusb_backend"), local);
            assert!(!filter.contains("tests\\.rs"));
            let args = arguments(&filter);
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
            assert_eq!(args.last(), Some(&filter.as_str()));
            let text = text_arguments(&filter);
            assert!(text.contains(&OsString::from("--text")));
            assert_eq!(text.last(), Some(&OsString::from(&filter)));
        }
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            let bad = Path::new(std::ffi::OsStr::from_bytes(&[255]));
            let good = Path::new("/synthetic");
            assert!(ignore(bad, good, good, false).is_err());
            assert!(ignore(good, bad, good, false).is_err());
            assert!(ignore(good, good, bad, false).is_err());
        }
    }

    #[test]
    fn local_exemption_is_exact() {
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
    }

    #[cfg(unix)]
    #[test]
    fn toolchain_reports_release_and_canonical_sysroot_or_fails() {
        let temp = tempfile::tempdir().expect("temp");
        let sysroot = temp.path().canonicalize().expect("canonical");
        let rustc = script(
            temp.path(),
            "rustc",
            &format!(
                "case \"$1\" in --print) echo '{}';; *) echo 'rustc 1.2.3 (synthetic)'; echo 'host: x';; esac",
                sysroot.display()
            ),
        );
        let found = toolchain(&rustc).expect("toolchain");
        assert_eq!(found.release, "rustc 1.2.3 (synthetic)");
        assert_eq!(found.sysroot, sysroot);
        let invalid = script(temp.path(), "invalid", "printf '\\377'");
        assert!(toolchain(&invalid).is_err());
        let no_sysroot = script(
            temp.path(),
            "no-sysroot",
            "case \"$1\" in --print) exit 1;; *) echo 'rustc 1.2.3';; esac",
        );
        assert!(toolchain(&no_sysroot).is_err());
        assert!(toolchain(&"/usr/bin/false".into()).is_err());
        assert!(toolchain(&"/missing-fixture".into()).is_err());
        // An empty sysroot cannot be canonicalized.
        assert!(toolchain(&"/usr/bin/true".into()).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn native_commands_control_the_result_and_failures_name_regions_and_toolchain() {
        use std::os::unix::ffi::OsStrExt;
        let temp = tempfile::tempdir().expect("temp");
        let home = Path::new("/synthetic/cargo");
        let tc = fake_toolchain();
        let mut output = Vec::new();
        invoke(
            temp.path(),
            home,
            &tc,
            &"/usr/bin/true".into(),
            true,
            &mut output,
        )
        .expect("native result");
        let text = String::from_utf8(output).expect("text");
        assert!(text.contains("LOCAL ONLY"));
        assert!(text.contains("TOOLCHAIN: rustc 0.0.0 (synthetic)"));
        for local in [true, false] {
            assert!(
                invoke(
                    temp.path(),
                    home,
                    &tc,
                    &"/usr/bin/true".into(),
                    local,
                    &mut crate::tests::Broken
                )
                .is_err()
            );
        }
        // The gate fails and the text report succeeds: both hints, in order.
        let text_ok = script(
            temp.path(),
            "cargo-text-ok",
            "case \"$*\" in *--fail-under-lines*) exit 1;; *) exit 0;; esac",
        );
        let mut output = Vec::new();
        assert!(invoke(temp.path(), home, &tc, &text_ok, false, &mut output).is_err());
        let text = String::from_utf8(output).expect("text");
        assert!(text.contains("grep -n -B1 '\\^0' coverage/uncovered.txt"));
        assert!(text.contains("CI floats on `stable`"));
        // Both native commands fail: only the toolchain hint is printed.
        let mut output = Vec::new();
        let false_cargo = OsString::from("/usr/bin/false");
        assert!(invoke(temp.path(), home, &tc, &false_cargo, false, &mut output).is_err());
        let text = String::from_utf8(output).expect("text");
        assert!(!text.contains("grep"));
        assert!(text.contains("CI floats on `stable`"));
        // Operator output failures surface from each hint.
        assert!(
            invoke(
                temp.path(),
                home,
                &tc,
                &text_ok,
                false,
                &mut FailAfterLines(1)
            )
            .is_err()
        );
        FailAfterLines(0).flush().expect("flush");
        let ignore = "x";
        assert!(report_failure(temp.path(), &text_ok, ignore, &mut crate::tests::Broken).is_err());
        assert!(
            report_failure(temp.path(), &false_cargo, ignore, &mut crate::tests::Broken).is_err()
        );
        assert!(
            invoke(
                temp.path(),
                home,
                &tc,
                &"/missing-fixture".into(),
                false,
                &mut Vec::new()
            )
            .is_err()
        );
        assert!(
            invoke(
                temp.path(),
                Path::new(std::ffi::OsStr::from_bytes(&[255])),
                &tc,
                &"/usr/bin/true".into(),
                false,
                &mut Vec::new()
            )
            .is_err()
        );
    }

    #[test]
    fn filesystem_and_environment_failures_fail_before_native_reporting() {
        let temp = tempfile::tempdir().expect("temp");
        let cargo = OsString::from("/usr/bin/true");
        let error = || Error::Invalid("injected");
        let run = |root, home, toolchain| {
            environment(root, home, toolchain, &cargo, false, &mut Vec::new())
        };
        assert!(
            run(
                Err(std::io::Error::other("cwd")),
                Err(error()),
                Ok(fake_toolchain())
            )
            .is_err()
        );
        assert!(
            run(
                Ok(temp.path().join("missing")),
                Err(error()),
                Ok(fake_toolchain())
            )
            .is_err()
        );
        assert!(run(Ok(temp.path().into()), Err(error()), Ok(fake_toolchain())).is_err());
        assert!(
            run(
                Ok(temp.path().into()),
                Ok(temp.path().join("missing")),
                Ok(fake_toolchain())
            )
            .is_err()
        );
        assert!(run(Ok(temp.path().into()), Ok(temp.path().into()), Err(error())).is_err());
        run(
            Ok(temp.path().into()),
            Ok(temp.path().into()),
            Ok(fake_toolchain()),
        )
        .expect("native");
        std::fs::remove_dir(temp.path().join("coverage")).expect("remove directory");
        std::fs::write(temp.path().join("coverage"), b"not a directory").expect("file");
        assert!(
            invoke(
                temp.path(),
                temp.path(),
                &fake_toolchain(),
                &cargo,
                false,
                &mut Vec::new()
            )
            .is_err()
        );
        std::fs::remove_file(temp.path().join("coverage")).expect("remove file");
    }
}
