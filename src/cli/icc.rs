//! The experimental `ImageCaptureCore` camera backend (`camera_backend = "icc"`).
//!
//! Threading: `ImageCaptureCore` requires the process main thread for its run loop, and only one
//! native runtime may ever exist in a process. [`launch`] therefore hands the *whole* async
//! application (supervisor, RTSP, ONVIF, discovery, signal handling, graceful shutdown) to
//! [`transport::icc::run`](icc::run) as one closure. The main thread runs the native run loop and
//! the joined worker side builds its own tokio runtime around [`IccFactory`]; shutdown stays
//! joined (stop, close the session with confirmation, native runtime returns). Output goes to
//! the process stdout and the tracing dispatcher is re-installed on the worker, because
//! the main thread is parked in the run loop and holds neither.
//!
//! Selection: `camera_id` names one device exactly. The others attached are ignored and never
//! opened, and an absent one waits, never falling back. Without it the one camera discovery
//! returns is used: zero waits and several are refused (a count, naming `tethercam cameras`).
//! Never the first of several, and never logged. `camera_serial` is a raw-USB identity and is
//! rejected rather than ignored (`config`); identifiers are printed only by `cameras`.
//!
//! Reconnect: the supervisor's loop reuses the one runtime. After a confirmed close the next
//! session reopens on it. A deadline retires the session and the next attempt reports busy until
//! native cleanup settles; busy for [`BUSY_LIMIT`] without a break means the completion is never
//! coming and counts as a quarantine. A *quarantine* (camera removed, failed close, owner gone) is terminal
//! for the process: the native run loop can then never return, so after the application has shut
//! down in order the process exits with [`QUARANTINE_EXIT`] and the service manager restarts it.
//! That is the one deliberate departure from joined shutdown; it is taken only when the native
//! owner has declared its state unresolvable.
//!
//! One-shot commands (`cameras`, `probe`, `doctor`): `native::run` returns only once native
//! cleanup settles, which after a timed-out step may be never, and the command's result would be
//! printed by a process that then never exits. When the work is done and its result is written
//! and flushed, the process therefore waits [`SETTLE_LIMIT`] for the native side to settle. Past
//! it, a counts-only notice is printed and the process exits through the same `terminate` path
//! with [`UNSETTLED_EXIT`]. A command that settles in time is a joined, clean exit with status 0.
//! The service keeps its own policy (the busy limit) and is not watched this way.
//!
//! References:
//! - `docs/macos-camera.md`, Ownership and deadlines
//! - `docs/service.md`, Shutdown and validation

use std::future::Future;
use std::io::{self, Write};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use tokio::sync::watch;
use tracing::Dispatch;

use super::{Command, Error, Options, cameras, command_until, diagnostics};
use crate::config::{self, Config};
use crate::ptp::session::SessionConfig;
use crate::supervisor::{CameraFactory, OpenError};
use crate::transport::icc::{
    self as icc, DISCOVERY_WINDOW, Effect, IccDeviceId, IccError, IccHandle, IccSession,
    IccTimeout, Selector,
};

#[cfg(test)]
#[path = "selection_tests.rs"]
mod selection_tests;
#[cfg(test)]
#[path = "icc_tests.rs"]
mod tests;

/// Exit status after a quarantine (`EX_TEMPFAIL`): the process cannot recover, a restart can.
pub(super) const QUARANTINE_EXIT: u8 = 75;

/// Exit status of a one-shot command whose own result is complete but whose native cleanup had
/// not settled within [`SETTLE_LIMIT`] (`EX_UNAVAILABLE`): the output can be trusted, the native
/// runtime cannot be reused or joined. Distinct from [`QUARANTINE_EXIT`], which a service
/// manager answers with a restart, and from the generic failure status 1.
pub(super) const UNSETTLED_EXIT: u8 = 69;

/// How long a finished one-shot command waits for native cleanup before it exits with
/// [`UNSETTLED_EXIT`].
///
/// Every step of `cameras` is bounded at 10 s, and `probe` and `doctor` at their 60 s whole
/// transaction deadline. A step that hit its deadline leaves one retired command to complete and
/// then the confirming close to run, each a native step of up to that same bound, so a camera that
/// is merely slow settles within a bound of that size. 60 s is six of the `cameras` steps and the
/// longest `probe` step, yet still a bounded, human-scale wait before a command that has already
/// printed its answer gives up. It is well under the service's [`BUSY_LIMIT`]: nothing here waits
/// on a 300 s download.
pub(super) const SETTLE_LIMIT: Duration = Duration::from_secs(60);

/// The settle bound for `command`: one-shot commands only, never the long-running service.
fn settle_bound(command: Command) -> Option<Duration> {
    (command != Command::Run).then_some(SETTLE_LIMIT)
}

/// Longest a run of consecutive busy answers may last before the native runtime is declared stuck
/// and the process restarts.
///
/// Busy is the honest answer while a retired command or a close still settles natively. The
/// longest wait this process ever asks of the native side is the Sony driver's 300 s still
/// download (`SonyConfig::download_timeout`); the 60 s `transaction_timeout` and the 30 s
/// discovery wait are shorter. A command retired at its deadline can still be completing, so the
/// native side gets as long again as that longest wait to settle: 300 s. A completion that never
/// arrives (a camera hung or unplugged mid-command) then costs five minutes of `waiting` before
/// the exit-75 restart. It is a wall clock, not a retry count, because the supervisor's backoff
/// (100 ms doubling to 5 s) decides how many attempts fit in it.
const BUSY_LIMIT: Duration = Duration::from_secs(300);

/// Pause after the sole candidate refused the Sony handshake, in place of the 5 s backoff cap. A
/// phone or other non-Sony device is a camera to `ImageCaptureCore`, and every reopen of it can
/// raise a trust prompt on that device; a Sony body outside PC Remote mode is retried at the
/// same slower pace, so after switching mode the service picks the camera up within this time.
const REFUSAL_DELAY: Duration = Duration::from_secs(60);

/// What a discovery failure means to the supervisor, as a category safe to log.
pub(super) fn open_error(error: IccError) -> OpenError {
    match error {
        IccError::Permission => OpenError::Permission,
        IccError::Busy | IccError::AdmissionExpired | IccError::DeadlineExceeded => OpenError::Busy,
        IccError::Removed | IccError::Disconnected | IccError::NotFound | IccError::Quarantined => {
            OpenError::Disconnected
        }
        IccError::Ambiguous => OpenError::Ambiguous,
        _ => OpenError::Discovery,
    }
}

/// The selection `cfg` asks for. `Config::validate` already bounded `camera_id`, so a failure
/// here means an unvalidated configuration; it is reported as the same key, never its value.
pub(super) fn selector(cfg: &Config) -> Result<Selector, Error> {
    let Some(id) = &cfg.camera_id else {
        return Ok(Selector::SoleCandidate);
    };
    IccDeviceId::new(id.expose().to_owned())
        .map(Selector::Exact)
        .map_err(|_| {
            Error::Config(config::Error::Validation {
                key: "camera_id",
                value: "[REDACTED]".into(),
            })
        })
}

/// The selected camera and the one bridge handle to it, passed between connect and session.
pub(super) struct IccCamera {
    handle: IccHandle,
    device: IccDeviceId,
    quarantined: bool,
}

/// Camera factory over the one native runtime. It owns the bridge handle while no session does,
/// and reports a quarantine through `fatal`, which the application turns into a shutdown.
pub(super) struct IccFactory {
    handle: Option<IccHandle>,
    selector: Selector,
    fatal: watch::Sender<bool>,
    /// When the current run of consecutive busy answers began, see [`BUSY_LIMIT`].
    busy_since: Option<tokio::time::Instant>,
}

impl IccFactory {
    /// Retain the handle and selection; construction touches nothing native.
    pub(super) fn new(handle: IccHandle, selector: Selector) -> (Self, watch::Receiver<bool>) {
        let (fatal, quarantined) = watch::channel(false);
        (
            Self {
                handle: Some(handle),
                selector,
                fatal,
                busy_since: None,
            },
            quarantined,
        )
    }

    fn quarantine(&self) {
        tracing::error!(event = "icc_runtime_quarantined");
        self.fatal.send_replace(true);
    }

    /// Classify an unsuccessful connect: a run of busy answers is bounded, anything else ends it.
    /// One that outlasts [`BUSY_LIMIT`] means a native completion is never coming. The runtime
    /// is then as unresolvable as after a quarantine and takes the same orderly exit. Nothing is
    /// cancelled.
    fn classify(&mut self, error: OpenError) -> OpenError {
        if error != OpenError::Busy {
            self.busy_since = None;
            return error;
        }
        let since = *self
            .busy_since
            .get_or_insert_with(tokio::time::Instant::now);
        if since.elapsed() >= BUSY_LIMIT {
            tracing::error!(
                event = "icc_busy_limit_exceeded",
                limit_secs = BUSY_LIMIT.as_secs()
            );
            self.quarantine();
        }
        error
    }

    /// Put the handle back and translate a failed discovery.
    fn rejected(
        &mut self, handle: IccHandle, error: IccError,
    ) -> Result<Option<IccCamera>, OpenError> {
        self.handle = Some(handle);
        if error == IccError::NotFound {
            self.busy_since = None;
            return Ok(None);
        }
        if error.effect() == Effect::Quarantine {
            self.quarantine();
        }
        tracing::warn!(event = "icc_connect_failed", category = %error);
        Err(self.classify(open_error(error)))
    }
}

impl CameraFactory for IccFactory {
    type Camera = IccCamera;
    type Session = IccSession;

    async fn connect(&mut self) -> Result<Option<IccCamera>, OpenError> {
        // After a quarantine nothing more may be submitted, not even a discovery.
        if *self.fatal.borrow() {
            return Err(OpenError::Disconnected);
        }
        // Absent only while an earlier camera was never released: native work may be pending.
        let Some(mut handle) = self.handle.take() else {
            return Err(self.classify(OpenError::Busy));
        };
        let found = handle
            .discover(DISCOVERY_WINDOW, IccTimeout::default())
            .await;
        let chosen = found.and_then(|found| {
            self.selector.choose(&found).inspect_err(|error| {
                if *error == IccError::Ambiguous {
                    tracing::warn!(event = "icc_ambiguous_cameras", candidates = found.len());
                }
            })
        });
        match chosen {
            Ok(device) => Ok(Some(IccCamera {
                handle,
                device,
                quarantined: false,
            })),
            Err(error) => self.rejected(handle, error),
        }
    }

    fn session(&self, camera: IccCamera, cfg: SessionConfig, clock: fn() -> Instant) -> IccSession {
        IccSession::new(camera.handle, camera.device, cfg, clock)
    }

    fn recover(&self, session: IccSession) -> IccCamera {
        let quarantined = session.is_quarantined();
        let (handle, device) = session.into_parts();
        IccCamera {
            handle,
            device,
            quarantined,
        }
    }

    fn release(&mut self, camera: IccCamera) {
        // A camera was reached: whatever busy run came before it is over.
        self.busy_since = None;
        if camera.quarantined {
            self.quarantine();
        }
        self.handle = Some(camera.handle);
    }

    fn pause(&self, delay: std::time::Duration) -> impl Future<Output = ()> + Send {
        tokio::time::sleep(delay)
    }

    fn refusal_delay(&self) -> Option<Duration> {
        Some(REFUSAL_DELAY)
    }
}

/// Run one command on the ICC backend until `stop` resolves, a quarantine, or the command ends.
///
/// A quarantine wins over any other result: it decides whether the process can go on.
pub(super) async fn drive_until(
    handle: IccHandle, cfg: &Config, options: &Options, out: &mut dyn Write,
    stop: Pin<Box<dyn Future<Output = ()> + '_>>,
) -> Result<(), Error> {
    if let Command::Cameras { describe } = options.command {
        return cameras::list(handle, cfg, describe, out, stop).await;
    }
    let (factory, mut fatal) = IccFactory::new(handle, selector(cfg)?);
    let quarantined = fatal.clone();
    let shutdown = async move {
        tokio::select! {
            () = stop => {},
            _ = fatal.wait_for(|fatal| *fatal) => {},
        }
    };
    let result = command_until(factory, cfg, options, out, Box::pin(shutdown)).await;
    if *quarantined.borrow() {
        Err(Error::Quarantined)
    } else {
        result
    }
}

/// Everything the worker needs, owned so it can cross to the thread the native runtime joins.
pub(super) struct Plan {
    options: Options,
    cfg: Config,
    dispatch: Dispatch,
}

type StopSignal = io::Result<Pin<Box<dyn Future<Output = ()>>>>;

/// The worker side: scoped logging, a runtime of its own, and OS signals registered in it.
fn worker_with(
    handle: IccHandle, plan: &Plan, out: &mut dyn Write,
    runtime: io::Result<tokio::runtime::Runtime>, stop: fn() -> StopSignal,
) -> Result<(), Error> {
    tracing::dispatcher::with_default(&plan.dispatch, || {
        runtime?
            .block_on(async { drive_until(handle, &plan.cfg, &plan.options, out, stop()?).await })
    })
}

fn worker(handle: IccHandle, plan: &Plan, out: &mut dyn Write) -> Result<(), Error> {
    worker_with(
        handle,
        plan,
        out,
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build(),
        || diagnostics::Signals::install().map(|signals| Box::pin(signals.wait()) as _),
    )
}

type Entry = Box<dyn FnOnce(IccHandle) -> Result<(), Error> + Send>;

/// What the settle watcher is told.
enum Progress {
    /// The application ended. Carries the rendered error when it failed, whose report belongs
    /// before any notice but would otherwise be printed only after `run` returns.
    Finished(Option<String>),
    /// `run` returned: native cleanup settled and the process ends normally.
    Settled,
}

/// Reports [`Progress::Finished`] when the entry ends, by return or by unwinding.
struct Finished {
    progress: mpsc::Sender<Progress>,
    failure: Option<String>,
}

impl Drop for Finished {
    fn drop(&mut self) {
        // A watcher that is gone (no bound for this command) has nothing to be told.
        drop(self.progress.send(Progress::Finished(self.failure.take())));
    }
}

/// The operator's output, shared between the worker and the settle watcher. A holder that
/// panicked mid-write leaves a usable sink.
type Shared<W> = Arc<Mutex<W>>;

fn sink<W>(out: &Shared<W>) -> std::sync::MutexGuard<'_, W> {
    out.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Once the application has ended, give native cleanup `limit` to settle. If it does not, the
/// result is already written: report the error if there was one, add the counts-only notice and
/// exit through `terminate`, because `run` may never return.
fn watch<W: Write>(
    out: &Shared<W>, progress: &mpsc::Receiver<Progress>, limit: Duration, terminate: fn(u8),
) {
    let Ok(Progress::Finished(failure)) = progress.recv() else {
        // `run` ended before the application ever did: nothing is pending on its account.
        return;
    };
    if !matches!(progress.recv_timeout(limit), Err(RecvTimeoutError::Timeout)) {
        return;
    }
    tracing::error!(
        event = "icc_cleanup_unsettled",
        limit_secs = limit.as_secs()
    );
    let mut out = sink(out);
    // Best effort: the process is about to end and there is nowhere left to report to.
    if let Some(failure) = failure {
        let _ = writeln!(out, "Error: {failure}");
    }
    let _ = writeln!(
        out,
        "Native cleanup did not settle within {limit:?}; exiting with status {UNSETTLED_EXIT}. The result above is complete."
    );
    let _ = out.flush();
    drop(out);
    terminate(UNSETTLED_EXIT);
}

/// The native main-thread runtime: runs the entry and returns once native cleanup has settled.
type Run = fn(Entry) -> Result<Result<(), Error>, IccError>;

/// Starts the settle watcher's thread. A seam only so a refusal of the OS is testable.
type Spawn = fn(Box<dyn FnOnce() + Send>) -> io::Result<std::thread::JoinHandle<()>>;

fn spawn_watcher(task: Box<dyn FnOnce() + Send>) -> io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name("icc-settle-watch".into())
        .spawn(task)
}

/// Build the worker entry, run it through `run` (the native main-thread runtime) and report.
/// After a quarantine it writes the error through `out` and calls `terminate`, because the
/// native run loop cannot return. With a `settle` bound, `terminate` is likewise called with
/// [`UNSETTLED_EXIT`] when the application ended but `run` has not returned within it.
fn launch_with<W: Write + Send + 'static>(
    options: &Options, cfg: &Config, dispatch: &Dispatch, out: W, run: Run,
    settle: Option<Duration>, terminate: fn(u8),
) -> Result<(), Error> {
    launch_spawning(
        options,
        cfg,
        dispatch,
        out,
        (run, settle, terminate),
        spawn_watcher,
    )
}

/// [`launch_with`], with the watcher's thread start injectable. A watcher that cannot start fails
/// the command before the native runtime is touched: silently running unbounded would be the hang
/// this exists to prevent.
fn launch_spawning<W: Write + Send + 'static>(
    options: &Options, cfg: &Config, dispatch: &Dispatch, out: W,
    (run, settle, terminate): (Run, Option<Duration>, fn(u8)), spawn: Spawn,
) -> Result<(), Error> {
    let plan = Plan {
        options: options.clone(),
        cfg: cfg.clone(),
        dispatch: dispatch.clone(),
    };
    let out: Shared<W> = Arc::new(Mutex::new(out));
    let (progress, observed) = mpsc::channel();
    let watcher = settle
        .map(|limit| {
            let out = Arc::clone(&out);
            let dispatch = dispatch.clone();
            spawn(Box::new(move || {
                tracing::dispatcher::with_default(&dispatch, || {
                    watch(&out, &observed, limit, terminate);
                });
            }))
        })
        .transpose()?;
    // Set once the application is about to run: a later failure of the native runtime is then not
    // "never started", which would send the operator looking at the platform and the build.
    let started = Arc::new(AtomicBool::new(false));
    let ran = Arc::clone(&started);
    let finished = progress.clone();
    let entry: Entry = Box::new(move |handle| {
        ran.store(true, Ordering::Release);
        let mut report = Finished {
            progress: finished,
            failure: None,
        };
        let mut out = sink(&out);
        let result = worker(handle, &plan, &mut *out);
        if let Err(error @ Error::Quarantined) = &result {
            // Best effort: the process is about to end and there is nowhere left to report to.
            let _ = writeln!(out, "Error: {error}");
            let _ = out.flush();
            terminate(QUARANTINE_EXIT);
        }
        // The result is complete and on its way out before the watcher can run out of patience.
        let _ = out.flush();
        report.failure = result.as_ref().err().map(ToString::to_string);
        result
    });
    let outcome = run(entry);
    drop(progress.send(Progress::Settled));
    if let Some(watcher) = watcher {
        // The watcher only reports; a panic in it has no result to add.
        drop(watcher.join());
    }
    outcome.map_err(|_| {
        if started.load(Ordering::Acquire) {
            Error::IccFailed
        } else {
            Error::IccUnavailable
        }
    })?
}

fn terminate(code: u8) {
    std::process::exit(i32::from(code));
}

/// Run `options.command` on the experimental native backend. The caller's `out` is not used:
/// the main thread is parked in the native run loop, so the worker writes to the process stdout.
pub(super) fn launch(options: &Options, cfg: &Config, dispatch: &Dispatch) -> Result<(), Error> {
    launch_with(
        options,
        cfg,
        dispatch,
        io::stdout(),
        icc::run::<Entry, Result<(), Error>>,
        settle_bound(options.command),
        terminate,
    )
}
