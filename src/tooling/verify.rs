//! Explicit, bounded loopback stream/snapshot profiling with owned-child cleanup.
//!
//! References:
//! - <https://ffmpeg.org/ffmpeg.html>
//! - <https://curl.se/docs/manpage.html>
//! - <https://doc.rust-lang.org/std/process/struct.Child.html>

mod soak;
mod timed;
use super::{Error, clients};
pub(super) use soak::run as soak;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
pub(super) use timed::serve;

/// A process owned by this invocation. Never signal by name or discover unrelated PIDs.
struct Owned {
    child: Child,
    ops: Processes,
    requested: bool,
    termination: Termination,
    /// Set by a signal handler: a wait ends at once and the drop reaps this exact child.
    interrupt: Option<Arc<AtomicBool>>,
}

/// Only camera/server ownership may wait indefinitely for graceful session cleanup.
#[derive(Clone, Copy)]
enum Termination {
    Client(Duration),
    JoinedServer,
}

/// Narrow native process seam; fake failures never signal unrelated processes.
#[derive(Clone, Copy)]
struct Processes {
    poll: fn(&mut Child) -> std::io::Result<Option<std::process::ExitStatus>>,
    signal: fn(u32) -> std::io::Result<bool>,
    reap: fn(&mut Child) -> std::io::Result<std::process::ExitStatus>,
    kill: fn(&mut Child) -> std::io::Result<()>,
}
impl Default for Processes {
    fn default() -> Self {
        Self {
            poll: Child::try_wait,
            signal,
            reap: Child::wait,
            kill: Child::kill,
        }
    }
}
fn signal(pid: u32) -> std::io::Result<bool> {
    Command::new("/bin/kill")
        .args([OsString::from("-TERM"), OsString::from(pid.to_string())])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
}
impl Owned {
    fn new(child: Child) -> Self {
        Self {
            child,
            ops: Processes::default(),
            requested: false,
            termination: Termination::Client(Duration::from_secs(1)),
            interrupt: None,
        }
    }
    fn joined(child: Child) -> Self {
        let mut owned = Self::new(child);
        owned.termination = Termination::JoinedServer;
        owned
    }
    fn start(program: &Path, args: &[OsString]) -> Result<Self, Error> {
        Ok(Self::new(
            Command::new(program)
                .args(args)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()?,
        ))
    }
    fn wait(&mut self, budget: Duration) -> Result<(), Error> {
        if !self.wait_exit(budget)?.success() {
            return Err(Error::Invalid("verification subprocess failed"));
        }
        Ok(())
    }
    fn wait_exit(&mut self, budget: Duration) -> Result<std::process::ExitStatus, Error> {
        let deadline = Instant::now() + budget;
        loop {
            if let Some(status) = (self.ops.poll)(&mut self.child)? {
                return Ok(status);
            }
            if self
                .interrupt
                .as_ref()
                .is_some_and(|flag| flag.load(Ordering::Relaxed))
            {
                return Err(Error::Invalid("interrupted"));
            }
            if Instant::now() >= deadline {
                return Err(Error::Invalid("verification subprocess deadline exceeded"));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    fn stop(&mut self) -> Result<(), Error> {
        if let Some(status) = (self.ops.poll)(&mut self.child)? {
            return if status.success() || self.requested {
                Ok(())
            } else {
                Err(Error::Invalid(
                    "owned verification child exited unsuccessfully",
                ))
            };
        }
        // Native argv, targeted only at the exact child PID. No shell or process-name search.
        let signalled = (self.ops.signal)(self.child.id())?;
        self.requested |= signalled;
        if !signalled && (self.ops.poll)(&mut self.child)?.is_none() {
            return Err(Error::Invalid(
                "owned verification child did not accept shutdown",
            ));
        }
        // Media clients have no camera session to preserve: a TERM-resistant client must
        // not prevent the later server shutdown barrier. Escalate only those exact children.
        if let Termination::Client(grace) = self.termination
            && self.wait_exit(grace).is_err()
        {
            (self.ops.kill)(&mut self.child)?;
        }
        // Camera/controller shutdown remains joined, without a force-kill deadline.
        (self.ops.reap)(&mut self.child)?;
        Ok(())
    }
}
impl Drop for Owned {
    fn drop(&mut self) {
        self.ops = Processes::default();
        let _ = self.stop();
    }
}

fn args(values: &[&str]) -> Vec<OsString> {
    values.iter().map(OsString::from).collect()
}

type StartServer =
    fn(&Path, &Path, &Path, &Path, Option<&Path>) -> Result<timed::TimedServer, Error>;

struct Tools<'a> {
    bin: &'a Path,
    helper: &'a Path,
    server: StartServer,
    ffmpeg: &'a Path,
    ffprobe: &'a Path,
    curl: &'a Path,
}

fn capture(
    program: &Path, args: &[OsString], output: &Path, budget: Duration,
) -> Result<(), Error> {
    capture_until(program, args, output, budget, None)
}

/// [`capture`] that also ends, killing and reaping its own child, once `interrupt` is set.
fn capture_until(
    program: &Path, args: &[OsString], output: &Path, budget: Duration,
    interrupt: Option<&Arc<AtomicBool>>,
) -> Result<(), Error> {
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let file = opts.open(output)?;
    let mut child = Owned::new(
        Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(file)
            .stderr(Stdio::null())
            .spawn()?,
    );
    child.interrupt = interrupt.cloned();
    child.wait(budget)
}

fn verify(
    mode: &OsStr, tools: &Tools<'_>, cfg: &Path, output: &Path, simulation: Option<&Path>,
) -> Result<(), Error> {
    let seconds = match mode.to_str() {
        Some("native" | "streams" | "snapshot") => "3",
        Some("profile") => "30",
        _ => return Err(Error::Usage),
    };
    for path in [
        tools.bin,
        tools.ffmpeg,
        tools.ffprobe,
        tools.curl,
        cfg,
        output,
    ] {
        if !path.is_absolute() {
            return Err(Error::Invalid("verification paths must be absolute"));
        }
    }
    if simulation.is_some_and(|path| !path.is_absolute()) {
        return Err(Error::Invalid("simulation path must be absolute"));
    }
    let mut directory = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        directory.mode(0o700);
    }
    directory.create(output)?;
    let client_dir = output.join("clients");
    let base = clients::prepare(cfg, &client_dir)?;
    let mut server = (tools.server)(tools.helper, tools.bin, cfg, output, simulation)?;
    let result = (|| {
        let status_file = output.join("unauthenticated-status.txt");
        let mut curl = args(&[
            "--silent",
            "--retry",
            "12",
            "--retry-connrefused",
            "--retry-delay",
            "1",
            "--max-time",
            "5",
            "-o",
            "/dev/null",
            "-w",
            "%{http_code}",
        ]);
        curl.push(format!("{base}/onvif/snapshot/main").into());
        capture(tools.curl, &curl, &status_file, Duration::from_secs(75))?;
        if fs::read_to_string(status_file)? != "401" {
            return Err(Error::Invalid("unauthenticated snapshot was not rejected"));
        }
        let mut children = streams(mode, tools, &client_dir, output, seconds)?;
        profile_packets(mode, tools, &client_dir, output)?;
        if mode == "native" || mode == "snapshot" {
            let mut cmd = args(&["--config"]);
            cmd.push(client_dir.join("http-client.conf").into());
            cmd.extend([
                "-o".into(),
                output.join("still.jpg").into(),
                "-w".into(),
                "%{http_code}".into(),
                format!("{base}/onvif/snapshot/main").into(),
            ]);
            let status = output.join("authenticated-status.txt");
            capture(tools.curl, &cmd, &status, Duration::from_secs(365))?;
            if fs::read_to_string(status)? != "200" {
                return Err(Error::Invalid("authenticated snapshot failed"));
            }
        }
        for child in &mut children {
            child.wait(Duration::from_secs(50))?;
        }
        inspect(mode, tools, output)?;
        Ok(())
    })();
    let stopped = server.stop();
    result?;
    stopped
}

/// Start both authenticated stream clients concurrently.
fn streams(
    mode: &OsStr, tools: &Tools<'_>, client_dir: &Path, output: &Path, seconds: &str,
) -> Result<Vec<Owned>, Error> {
    let mut children = Vec::new();
    if mode != "snapshot" {
        for (mount, format, name) in [
            ("main", "h264", "profile-h264.h264"),
            (
                "mjpeg",
                if mode == "profile" {
                    "framehash"
                } else {
                    "mjpeg"
                },
                if mode == "profile" {
                    "profile-jpeg.framehash"
                } else {
                    "live-mjpeg.mjpeg"
                },
            ),
        ] {
            let mut cmd = args(&[
                "-v",
                "quiet",
                "-nostdin",
                "-protocol_whitelist",
                "file,tcp,udp,rtp",
                "-f",
                "concat",
                "-safe",
                "0",
                "-i",
            ]);
            cmd.push(client_dir.join(format!("rtsp-{mount}.ffconcat")).into());
            cmd.extend(args(&[
                "-t", seconds, "-map", "0:v:0", "-an", "-c:v", "copy", "-f", format,
            ]));
            if format == "framehash" {
                cmd.extend(args(&["-hash", "sha256"]));
            }
            cmd.extend(["-y".into(), output.join(name).into()]);
            children.push(Owned::start(tools.ffmpeg, &cmd)?);
        }
    }

    Ok(children)
}

/// Record packet arrival timestamps independently of stream copying.
fn profile_packets(
    mode: &OsStr, tools: &Tools<'_>, client_dir: &Path, output: &Path,
) -> Result<(), Error> {
    if mode == "profile" {
        let mut cmd = args(&[
            "-v",
            "quiet",
            "-rtsp_transport",
            "tcp",
            "-timeout",
            "15000000",
            "-use_wallclock_as_timestamps",
            "1",
            "-read_intervals",
            "%+30",
            "-show_packets",
            "-show_entries",
            "packet=pts_time,size,flags",
            "-of",
            "json",
            "-/i",
        ]);
        cmd.push(client_dir.join("rtsp-mjpeg.url").into());
        capture(
            tools.ffprobe,
            &cmd,
            &output.join("profile-jpeg-packets.json"),
            Duration::from_secs(50),
        )?;
    }

    Ok(())
}

/// Independently inspect and fully decode every recorded media artifact.
fn inspect(mode: &OsStr, tools: &Tools<'_>, output: &Path) -> Result<(), Error> {
    for name in if mode == "snapshot" {
        vec!["still.jpg"]
    } else if mode == "native" {
        vec!["profile-h264.h264", "live-mjpeg.mjpeg", "still.jpg"]
    } else if mode == "profile" {
        vec!["profile-h264.h264"]
    } else {
        vec!["profile-h264.h264", "live-mjpeg.mjpeg"]
    } {
        let mut cmd = args(&[
            "-v",
            "error",
            "-count_frames",
            "-show_entries",
            "stream=codec_name,width,height,nb_read_frames",
            "-of",
            "json",
        ]);
        cmd.push(output.join(name).into());
        capture(
            tools.ffprobe,
            &cmd,
            &output.join(format!("{name}.inspection.json")),
            Duration::from_secs(20),
        )?;
        let mut decode = args(&["-v", "error", "-nostdin", "-xerror", "-i"]);
        decode.push(output.join(name).into());
        decode.extend(args(&["-f", "null", "-"]));
        Owned::start(tools.ffmpeg, &decode)?.wait(Duration::from_secs(30))?;
    }

    Ok(())
}

pub(super) fn run(values: &[OsString]) -> Result<(), Error> {
    run_with(values, std::env::current_exe())
}

fn run_with(values: &[OsString], helper: std::io::Result<std::path::PathBuf>) -> Result<(), Error> {
    let [mode, bin, cfg, output, ffmpeg, ffprobe, curl, simulation] = values else {
        return Err(Error::Usage);
    };
    let helper = helper?;
    verify(
        mode,
        &Tools {
            helper: &helper,
            server: timed::TimedServer::start,
            bin: Path::new(bin),
            ffmpeg: Path::new(ffmpeg),
            ffprobe: Path::new(ffprobe),
            curl: Path::new(curl),
        },
        Path::new(cfg),
        Path::new(output),
        if simulation == "-" {
            None
        } else {
            Some(Path::new(simulation))
        },
    )
}

#[cfg(test)]
mod tests;
