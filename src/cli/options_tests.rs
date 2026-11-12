//! Argument syntax and arbitrary-byte totality tests.
//!
//! References:
//! - `docs/architecture.md`, CLI and supervisor contract

use super::options::*;
use proptest::prelude::*;
use std::ffi::OsString;

fn parse(args: &[&str]) -> Result<Options, ParseError> {
    Options::parse(args.iter().map(OsString::from))
}

#[test]
fn commands_and_paths_are_explicit() {
    for (name, command) in [
        ("setup", Command::Setup),
        ("probe", Command::Probe),
        ("run", Command::Run),
        ("doctor", Command::Doctor),
        ("--help", Command::Help),
        ("-h", Command::Help),
        ("--version", Command::Version),
        ("-V", Command::Version),
    ] {
        assert_eq!(parse(&[name]).unwrap().command, command);
    }
    assert_eq!(parse(&[]).unwrap().command, Command::Version);
    let options = parse(&[
        "run",
        "--config",
        "/synthetic/config.toml",
        "--simulate",
        "synthetic.jpg",
        "--no-discovery",
    ])
    .unwrap();
    assert_eq!(options.config, Some("/synthetic/config.toml".into()));
    assert_eq!(options.simulate, Some("synthetic.jpg".into()));
    assert!(options.no_discovery);
}

#[test]
fn rejects_ambiguous_and_duplicate_syntax() {
    for args in [
        vec!["unknown"],
        vec!["run", "unknown"],
        vec!["run", "--config"],
        vec!["run", "--simulate"],
        vec!["run", "--config", ""],
        vec!["run", "--config", "--help"],
        vec!["run", "--config", "a", "--config", "b"],
        vec!["run", "--simulate", "a", "--simulate", "b"],
        vec!["run", "--no-discovery", "--no-discovery"],
        vec!["--help", "--config", "a"],
        vec!["--version", "--simulate", "a"],
        vec!["--version", "--no-discovery"],
        vec!["setup", "--simulate", "a"],
        vec!["setup", "--no-discovery"],
    ] {
        assert!(parse(&args).is_err(), "{args:?}");
    }
    assert!(Options::parse(std::iter::repeat_with(|| OsString::from("run"))).is_err());
}

#[cfg(unix)]
#[test]
fn non_utf8_commands_fail_and_paths_survive() {
    use std::os::unix::ffi::OsStringExt;
    let invalid = OsString::from_vec(vec![0xff]);
    assert_eq!(Options::parse([invalid.clone()]), Err(ParseError::Invalid));
    assert_eq!(
        Options::parse(["run".into(), invalid.clone()]),
        Err(ParseError::Invalid)
    );
    assert_eq!(
        Options::parse(["run".into(), "--config".into(), invalid.clone()])
            .unwrap()
            .config,
        Some(invalid.into())
    );
}

proptest! {
    #[test]
    fn arbitrary_arguments_never_panic(args in proptest::collection::vec(".*", 0..32)) {
        let _ = Options::parse(args.into_iter().map(OsString::from));
    }
}
