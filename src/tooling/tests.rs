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
