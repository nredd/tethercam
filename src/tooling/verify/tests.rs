//! Owned process boundaries without physical hardware.
use super::*;
#[test]
fn child_success_failure_deadline_and_graceful_cleanup() {
    assert!(Owned::start(Path::new("/missing-program-fixture"), &[]).is_err());
    Owned::start(Path::new("/usr/bin/true"), &[])
        .expect("true")
        .wait(Duration::from_secs(1))
        .expect("success");
    assert!(
        Owned::start(Path::new("/usr/bin/false"), &[])
            .expect("false")
            .wait(Duration::from_secs(1))
            .is_err()
    );
    let mut child = Owned::start(Path::new("/bin/sleep"), &args(&["10"])).expect("sleep");
    assert!(child.wait(Duration::ZERO).is_err());
    child.stop().expect("stop");
    child.stop().expect("already stopped");
    let temp = tempfile::tempdir().expect("temp");
    capture(
        Path::new("/usr/bin/true"),
        &[],
        &temp.path().join("output"),
        Duration::from_secs(1),
    )
    .expect("capture");
    assert!(
        capture(
            Path::new("/usr/bin/true"),
            &[],
            &temp.path().join("output"),
            Duration::from_secs(1)
        )
        .is_err()
    );
    assert!(
        capture(
            Path::new("/missing-fixture"),
            &[],
            &temp.path().join("missing"),
            Duration::from_secs(1)
        )
        .is_err()
    );
    assert!(run(&[]).is_err());
    let tools = Tools {
        server: timed::TimedServer::start,
        helper: Path::new("/usr/bin/true"),
        bin: Path::new("relative"),
        ffmpeg: Path::new("/usr/bin/true"),
        ffprobe: Path::new("/usr/bin/true"),
        curl: Path::new("/usr/bin/true"),
    };
    assert!(verify(OsStr::new("bad"), &tools, temp.path(), temp.path(), None).is_err());
    assert!(verify(OsStr::new("native"), &tools, temp.path(), temp.path(), None).is_err());
}

#[test]
fn every_verification_plan_uses_owned_children_and_protected_files() {
    let (temp, program, cfg) = fixture();
    let tools = Tools {
        server: timed::TimedServer::start,
        helper: &program,
        bin: &program,
        ffmpeg: &program,
        ffprobe: &program,
        curl: &program,
    };
    for mode in ["native", "streams", "snapshot", "profile"] {
        let output = temp.path().join(mode);
        verify(
            OsStr::new(mode),
            &tools,
            &cfg,
            &output,
            Some(Path::new("/synthetic.jpg")),
        )
        .expect("verify");
        assert!(output.join("clients/http-client.conf").is_file());
    }
    for (name, field, mode) in [
        ("bad401", "curl", "snapshot"),
        ("bad200", "curl", "snapshot"),
        ("fail", "ffmpeg", "streams"),
        ("fail", "ffprobe", "profile"),
        ("fail", "ffprobe", "snapshot"),
    ] {
        let path = temp.path().join(name);
        fs::copy(&program, &path).expect("copy fixture");
        let failing = Tools {
            server: timed::TimedServer::start,
            helper: &program,
            bin: &program,
            ffmpeg: if field == "ffmpeg" { &path } else { &program },
            ffprobe: if field == "ffprobe" { &path } else { &program },
            curl: if field == "curl" { &path } else { &program },
        };
        assert!(
            verify(
                OsStr::new(mode),
                &failing,
                &cfg,
                &temp.path().join(format!("{name}-{field}-{mode}")),
                None
            )
            .is_err()
        );
    }
    let values = vec![
        OsString::from("snapshot"),
        program.clone().into(),
        cfg.clone().into(),
        temp.path().join("dispatch").into(),
        program.clone().into(),
        program.clone().into(),
        program.clone().into(),
        OsString::from("-"),
    ];
    run(&values).expect("run");
    let mut command = vec![OsString::from("verify")];
    command.extend(values);
    assert!(super::super::dispatch(&command, &mut Vec::new(), &mut std::io::empty()).is_err());
    assert!(
        super::super::dispatch(
            &[
                OsString::from("prepare-clients"),
                cfg.into(),
                temp.path().join("prepared-cli").into()
            ],
            &mut Vec::new(),
            &mut std::io::empty()
        )
        .is_ok()
    );
}

#[test]
fn native_process_failure_seams_keep_cleanup_on_the_same_owned_child() {
    let mut child = Owned::start(Path::new("/bin/sleep"), &args(&["10"])).expect("sleep");
    child.ops.poll = |_| Err(std::io::Error::other("poll"));
    assert!(child.wait(Duration::from_secs(1)).is_err());
    assert!(child.stop().is_err());
    child.ops = Processes {
        signal: |_| Err(std::io::Error::other("signal")),
        ..Processes::default()
    };
    assert!(child.stop().is_err());
    child.ops.signal = |_| Ok(false);
    assert!(child.stop().is_err());
    child.ops = Processes {
        reap: |_| Err(std::io::Error::other("reap")),
        ..Processes::default()
    };
    assert!(child.stop().is_err());
    child.ops = Processes::default();
    child.stop().expect("cleanup after injected failures");
}

#[test]
#[cfg(unix)]
fn an_exit_between_poll_and_signal_is_still_joined() {
    use std::os::unix::process::ExitStatusExt;
    thread_local! { static POLLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }
    let mut child = Owned::start(Path::new("/bin/sleep"), &args(&["10"])).expect("sleep");
    child.ops.poll = |_| {
        POLLS.with(|count| {
            let value = count.get();
            count.set(value + 1);
            Ok(if value == 0 {
                None
            } else {
                Some(std::process::ExitStatus::from_raw(0))
            })
        })
    };
    child.ops.signal = |pid| signal(pid).map(|_| false);
    child.stop().expect("reap after raced exit");
}

#[test]
fn every_verifier_io_failure_returns_a_failure_instead_of_a_partial_success() {
    let (temp, program, cfg) = fixture();
    let tools = Tools {
        server: timed::TimedServer::start,
        helper: &program,
        bin: &program,
        ffmpeg: &program,
        ffprobe: &program,
        curl: &program,
    };
    assert!(
        verify(
            OsStr::new("snapshot"),
            &tools,
            &temp.path().join("missing-cfg"),
            &temp.path().join("missing-config"),
            None
        )
        .is_err()
    );
    let failing = Tools {
        server: |_, _, _, _, _| Err(Error::Invalid("server startup")),
        ..tools
    };
    assert!(
        verify(
            OsStr::new("snapshot"),
            &failing,
            &cfg,
            &temp.path().join("server-startup"),
            None
        )
        .is_err()
    );
    for (mode, name) in [
        ("streams", "missing-stream-client"),
        ("snapshot", "missing-decoder"),
    ] {
        let failing = Tools {
            ffmpeg: Path::new("/missing-ffmpeg-fixture"),
            ..tools
        };
        assert!(
            verify(
                OsStr::new(mode),
                &failing,
                &cfg,
                &temp.path().join(name),
                None
            )
            .is_err()
        );
    }
    let failed = temp.path().join("fail");
    fs::copy(&program, &failed).expect("copy");
    let failing = Tools {
        ffmpeg: &failed,
        ..tools
    };
    assert!(
        verify(
            OsStr::new("snapshot"),
            &failing,
            &cfg,
            &temp.path().join("decoder-failure"),
            None
        )
        .is_err()
    );
    for (name, status_name) in [
        ("remove401", "unauthenticated-status.txt"),
        ("remove200", "authenticated-status.txt"),
    ] {
        let remover = temp.path().join(name);
        fs::copy(&program, &remover).expect("copy");
        let output = temp.path().join(format!("missing-{name}"));
        fs::write(
            remover.with_extension("remove"),
            output.join(status_name).as_os_str().as_encoded_bytes(),
        )
        .expect("removal instruction");
        let failing = Tools {
            curl: &remover,
            ..tools
        };
        assert!(verify(OsStr::new("snapshot"), &failing, &cfg, &output, None).is_err());
    }
    let values = vec![
        OsString::from("snapshot"),
        program.clone().into(),
        cfg.into(),
        temp.path().join("exe-failure").into(),
        program.clone().into(),
        program.clone().into(),
        program.into(),
        OsString::from("-"),
    ];
    assert!(run_with(&values, Err(std::io::Error::other("current executable"))).is_err());
}

fn fixture() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
    let temp = tempfile::tempdir().expect("temp");
    let program = temp.path().join("good");
    let status = Command::new("rustc")
        .args(["--edition=2024", "-D", "warnings"])
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/verification_client.rs"))
        .arg("-o")
        .arg(&program)
        .status()
        .expect("fixture compiler");
    assert!(status.success());
    let cfg = temp.path().join("config.toml");
    let mut valid = tethercam::config::Config::generate().expect("generate");
    valid.rtsp_address = "127.0.0.1".parse().expect("ip");
    valid.onvif_address = "127.0.0.1".parse().expect("ip");
    valid.create(&cfg).expect("cfg");

    (temp, program, cfg)
}

#[test]
fn simulation_cli_and_second_poll_failures_are_explicit() {
    let (temp, program, cfg) = fixture();
    let values = vec![
        OsString::from("snapshot"),
        program.clone().into(),
        cfg.into(),
        temp.path().join("simulated-cli").into(),
        program.clone().into(),
        program.clone().into(),
        program.clone().into(),
        OsString::from("/synthetic.jpg"),
    ];
    run_with(&values, Ok(program)).expect("simulated CLI");
    thread_local! { static POLLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }
    let mut child = Owned::start(Path::new("/bin/sleep"), &args(&["10"])).expect("sleep");
    child.ops.signal = |_| Ok(false);
    child.ops.poll = |_| {
        POLLS.with(|count| {
            let value = count.get();
            count.set(value + 1);
            if value == 0 {
                Ok(None)
            } else {
                Err(std::io::Error::other("second poll"))
            }
        })
    };
    assert!(child.stop().is_err());
    child.ops = Processes::default();
    child.stop().expect("cleanup");
}

#[test]
fn unauthenticated_and_authenticated_curl_process_failures_are_fatal() {
    let (temp, program, cfg) = fixture();
    for name in ["fail", "fail-auth"] {
        let curl = temp.path().join(name);
        fs::copy(&program, &curl).expect("fixture");
        let tools = Tools {
            server: timed::TimedServer::start,
            helper: &program,
            bin: &program,
            ffmpeg: &program,
            ffprobe: &program,
            curl: &curl,
        };
        assert!(
            verify(
                OsStr::new("snapshot"),
                &tools,
                &cfg,
                &temp.path().join(format!("{name}-output")),
                None
            )
            .is_err()
        );
    }
}

#[test]
fn relative_simulation_inputs_fail_before_creating_measurement_directories() {
    let (temp, program, cfg) = fixture();
    let tools = Tools {
        server: timed::TimedServer::start,
        helper: &program,
        bin: &program,
        ffmpeg: &program,
        ffprobe: &program,
        curl: &program,
    };
    let output = temp.path().join("relative-input");
    assert!(
        verify(
            OsStr::new("snapshot"),
            &tools,
            &cfg,
            &output,
            Some(Path::new("relative.jpg"))
        )
        .is_err()
    );
    assert!(!output.exists());
}
