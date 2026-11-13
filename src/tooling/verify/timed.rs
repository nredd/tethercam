//! Native resource accounting with pipe-owned shutdown, never PID discovery.
//!
//! `time` waits for a Rust controller, which owns and joins the actual server. Closing its
//! stdin requests graceful child shutdown even if the verifier exits early. Accounting includes
//! that small controller and its waited-for server, but never the separate media clients.
//!
//! References:
//! - <https://www.gnu.org/software/time/>
//! - <https://man.freebsd.org/cgi/man.cgi?query=time&sektion=1>

use super::{Error, Owned, Processes, args};
use std::ffi::OsString;
use std::io::Read;
use std::path::Path;
use std::process::{ChildStdin, Command, Stdio};

pub(super) struct TimedServer {
    time: Owned,
    input: Option<ChildStdin>,
}
impl TimedServer {
    pub(super) fn start(
        helper: &Path, bin: &Path, cfg: &Path, output: &Path, simulation: Option<&Path>,
    ) -> Result<Self, Error> {
        Self::start_with(helper, bin, cfg, output, simulation, Command::spawn)
    }

    fn start_with(
        helper: &Path, bin: &Path, cfg: &Path, output: &Path, simulation: Option<&Path>,
        spawn: fn(&mut Command) -> std::io::Result<std::process::Child>,
    ) -> Result<Self, Error> {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let metrics = opts.open(output.join("profile-server.metrics"))?;
        let mut command = Command::new("/usr/bin/time");
        #[cfg(target_os = "macos")]
        command.arg("-lp");
        #[cfg(not(target_os = "macos"))]
        command.args([
            "-f",
            "real %e\nuser %U\nsys %S\n%M maximum resident set size kib",
        ]);
        command
            .arg(helper)
            .arg("serve-owned")
            .arg(bin)
            .arg(cfg)
            .arg(simulation.unwrap_or(Path::new("-")))
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(metrics);
        let mut child = spawn(&mut command)?;
        let input = child.stdin.take();
        Ok(Self {
            time: Owned::joined(child),
            input,
        })
    }
    pub(super) fn stop(&mut self) -> Result<(), Error> {
        // EOF, not a signal or process-name lookup, asks the controller to stop its own child.
        self.input.take();
        let status = (self.time.ops.reap)(&mut self.time.child)?;
        if !status.success() {
            return Err(Error::Invalid("profile server/controller failed"));
        }
        Ok(())
    }
}
impl Drop for TimedServer {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

pub(crate) fn serve(values: &[OsString], input: &mut dyn Read) -> Result<(), Error> {
    serve_with(values, input, Processes::default())
}

fn serve_with(
    values: &[OsString], input: &mut dyn Read, processes: Processes,
) -> Result<(), Error> {
    let [bin, cfg, simulation] = values else {
        return Err(Error::Usage);
    };
    if !Path::new(bin).is_absolute()
        || !Path::new(cfg).is_absolute()
        || (simulation != "-" && !Path::new(simulation).is_absolute())
    {
        return Err(Error::Usage);
    }
    let mut server_args = args(&["run", "--config"]);
    server_args.push(cfg.clone());
    server_args.push("--no-discovery".into());
    if simulation != "-" {
        server_args.extend(["--simulate".into(), simulation.clone()]);
    }
    let mut server = Owned::start(Path::new(bin), &server_args)?;
    server.termination = super::Termination::JoinedServer;
    server.ops = processes;
    // Bounded pipe protocol. Ordinary runs send EOF; any data is an explicit invalid request.
    let mut command = Vec::new();
    let read = input.take(1).read_to_end(&mut command);
    let stop = server.stop();
    read?;
    stop?;
    if !command.is_empty() {
        return Err(Error::Invalid("invalid owned-server control input"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pipe_shutdown_is_bounded_and_always_joins_the_owned_child() {
        assert!(serve(&[], &mut std::io::empty()).is_err());
        let values = args(&["/usr/bin/true", "/synthetic/config", "-"]);
        serve(&values, &mut std::io::empty()).expect("EOF");
        let values = args(&["/usr/bin/true", "/synthetic/config", "/synthetic.jpg"]);
        assert!(serve(&values, &mut &b"bad"[..]).is_err());
        let values = args(&["/missing-program-fixture", "/synthetic/config", "-"]);
        assert!(serve(&values, &mut std::io::empty()).is_err());
    }
    struct BrokenRead;
    impl Read for BrokenRead {
        fn read(&mut self, _bytes: &mut [u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("read"))
        }
    }

    #[test]
    fn startup_read_and_reap_failures_preserve_owned_cleanup() {
        let temp = tempfile::tempdir().expect("temp");
        let program = Path::new("/usr/bin/true");
        assert!(
            TimedServer::start(
                program,
                program,
                program,
                &temp.path().join("missing"),
                None
            )
            .is_err()
        );
        assert!(
            TimedServer::start_with(program, program, program, temp.path(), None, |_| Err(
                std::io::Error::other("spawn")
            ))
            .is_err()
        );
        let child = Command::new("/usr/bin/true").spawn().expect("true");
        let mut server = TimedServer {
            time: Owned::new(child),
            input: None,
        };
        server.time.ops.reap = |_| Err(std::io::Error::other("reap"));
        assert!(server.stop().is_err());
        server.time.ops = Processes::default();
        server.stop().expect("reap");
        let mut failed = Owned::start(Path::new("/usr/bin/false"), &[]).expect("false");
        assert!(failed.wait(std::time::Duration::from_secs(1)).is_err());
        let mut server = TimedServer {
            time: failed,
            input: None,
        };
        assert!(server.stop().is_err());
        let values = args(&["/usr/bin/true", "/synthetic/config", "-"]);
        assert!(serve(&values, &mut BrokenRead).is_err());
        assert!(
            serve_with(
                &values,
                &mut std::io::empty(),
                Processes {
                    poll: |_| Err(std::io::Error::other("poll")),
                    ..Processes::default()
                }
            )
            .is_err()
        );
    }
    #[test]
    fn internal_controller_requires_absolute_explicit_paths() {
        for values in [
            args(&["relative", "/synthetic/cfg", "-"]),
            args(&["/usr/bin/true", "relative", "-"]),
            args(&["/usr/bin/true", "/synthetic/cfg", "relative.jpg"]),
        ] {
            assert!(serve(&values, &mut std::io::empty()).is_err());
        }
    }
}
