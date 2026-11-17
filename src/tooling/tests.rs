//! Utility CLI argument and output failure boundaries.
//!
//! References:
//! - <https://doc.rust-lang.org/std/io/trait.Write.html>
use super::*;

pub(super) struct Broken;
impl Write for Broken {
    fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
        Err(io::Error::other("injected"))
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Installs an executable file at `dst` that is safe to `exec` from this threaded process.
///
/// `exec` fails with `ETXTBSY` while any process holds the file open for writing. A child
/// forked by another test thread between this process opening `dst` and closing it inherits
/// the write descriptor until its own `exec`, so writing the fixture in-process races every
/// concurrent spawn. `/usr/bin/install` writes it from a short-lived child instead: this
/// process never owns a writable descriptor for `dst`, and the child is reaped before return.
pub(super) fn install_executable(src: &Path, dst: &Path) {
    let status = std::process::Command::new("/usr/bin/install")
        .args(["-m", "0755"])
        .arg(src)
        .arg(dst)
        .status()
        .expect("install");
    assert!(status.success(), "install `{}`", dst.display());
}

/// Installs a `/bin/sh` script named `name` under `dir` through [`install_executable`].
pub(super) fn install_script(dir: &Path, name: &str, body: &str) -> std::path::PathBuf {
    let source = tempfile::NamedTempFile::new_in(dir).expect("script source");
    std::fs::write(source.path(), format!("#!/bin/sh\n{body}\n")).expect("script");
    let path = dir.join(name);
    install_executable(source.path(), &path);
    path
}

/// Regression: fixtures stay executable while other threads fork children (`ETXTBSY`).
#[test]
fn installed_fixtures_exec_while_other_threads_fork() {
    use std::sync::atomic::{AtomicBool, Ordering};
    /// Releases the forker threads even when an assertion unwinds.
    struct Stop<'a>(&'a AtomicBool);
    impl Drop for Stop<'_> {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Relaxed);
        }
    }
    let stop = AtomicBool::new(false);
    let temp = tempfile::tempdir().expect("temp");
    std::thread::scope(|scope| {
        for _ in 0..2 {
            scope.spawn(|| {
                while !stop.load(Ordering::Relaxed) {
                    let _ = std::process::Command::new("/usr/bin/true").status();
                }
            });
        }
        let _release = Stop(&stop);
        for index in 0..300 {
            let script = install_script(temp.path(), &format!("probe{index}"), "exit 0");
            let status = std::process::Command::new(&script).status();
            assert!(status.expect("exec fixture").success());
            let copy = temp.path().join(format!("copy{index}"));
            install_executable(&script, &copy);
            assert!(
                std::process::Command::new(&copy)
                    .status()
                    .expect("exec copy")
                    .success()
            );
        }
    });
}

fn args(values: &[&str]) -> Vec<OsString> {
    values.iter().map(OsString::from).collect()
}

#[test]
fn arguments_errors_and_stream_failures() {
    for values in [
        vec![],
        vec!["unknown"],
        vec!["usbip"],
        vec!["release-tag", "bad"],
        vec!["soak"],
    ] {
        assert!(dispatch(&args(&values), &mut Vec::new(), &mut std::io::empty()).is_err());
    }
    let mut err = Vec::new();
    assert_eq!(
        run(
            args(&["unknown"]),
            &mut Vec::new(),
            &mut err,
            &mut std::io::empty()
        ),
        ExitCode::FAILURE
    );
    assert_eq!(err, b"invalid utility arguments\n");
    assert_eq!(
        run(
            args(&["unknown"]),
            &mut Vec::new(),
            &mut Broken,
            &mut std::io::empty()
        ),
        ExitCode::FAILURE
    );
    assert_eq!(
        run(
            args(&["make-help"]),
            &mut Vec::new(),
            &mut Vec::new(),
            &mut std::io::empty()
        ),
        ExitCode::SUCCESS
    );
    assert!(
        dispatch(
            &args(&["usbip", "bad"]),
            &mut Vec::new(),
            &mut std::io::empty()
        )
        .is_err()
    );
    assert!(Broken.flush().is_ok());
    assert!(release_tag(OsStr::new(concat!("v", env!("CARGO_PKG_VERSION")))).is_ok());
    assert!(release_tag(OsStr::new("bad")).is_err());
    let mut output = Vec::new();
    make_help(
        "ignored\nnot-a-rule ## ignored\ntest: ## Run tests\n",
        &mut output,
    )
    .expect("help");
    assert!(
        String::from_utf8(output)
            .expect("utf8")
            .contains("Run tests")
    );
    assert!(make_help("test: ## Run tests", &mut Broken).is_err());
    assert!(
        dispatch(
            &args(&["profile-summary", "missing", "missing", "-", "-", "missing"]),
            &mut Vec::new(),
            &mut std::io::empty()
        )
        .is_err()
    );
}

#[test]
fn owned_server_input_and_coverage_output_are_explicit_cli_dependencies() {
    dispatch(
        &args(&["serve-owned", "/usr/bin/true", "/synthetic/config", "-"]),
        &mut Vec::new(),
        &mut std::io::empty(),
    )
    .expect("owned helper");
    assert!(
        dispatch(
            &args(&["coverage-gate"]),
            &mut Broken,
            &mut std::io::empty()
        )
        .is_err()
    );
}
