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
//! native cleanup settles. A *quarantine* (camera removed, failed close, owner gone) is terminal
//! for the process: the native run loop can then never return, so after the application has shut
//! down in order the process exits with [`QUARANTINE_EXIT`] and the service manager restarts it.
//! That is the one deliberate departure from joined shutdown; it is taken only when the native
//! owner has declared its state unresolvable.
//!
//! References:
//! - `docs/macos-camera.md`, Ownership and deadlines
//! - `docs/service.md`, Shutdown and validation

use std::future::Future;
use std::io::{self, Write};
use std::pin::Pin;
use std::time::Instant;

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
            },
            quarantined,
        )
    }

    fn quarantine(&self) {
        tracing::error!(event = "icc_runtime_quarantined");
        self.fatal.send_replace(true);
    }

    /// Put the handle back and translate a failed discovery.
    fn rejected(
        &mut self, handle: IccHandle, error: IccError,
    ) -> Result<Option<IccCamera>, OpenError> {
        self.handle = Some(handle);
        if error == IccError::NotFound {
            return Ok(None);
        }
        if error.effect() == Effect::Quarantine {
            self.quarantine();
        }
        tracing::warn!(event = "icc_connect_failed", category = %error);
        Err(open_error(error))
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
            return Err(OpenError::Busy);
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
        if camera.quarantined {
            self.quarantine();
        }
        self.handle = Some(camera.handle);
    }

    fn pause(&self, delay: std::time::Duration) -> impl Future<Output = ()> + Send {
        tokio::time::sleep(delay)
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

/// Build the worker entry, run it through `run` (the native main-thread runtime) and report.
/// After a quarantine it writes the error through `out` and calls `terminate`, because the
/// native run loop cannot return.
fn launch_with<W: Write + Send + 'static>(
    options: &Options, cfg: &Config, dispatch: &Dispatch, mut out: W,
    run: fn(Entry) -> Result<Result<(), Error>, IccError>, terminate: fn(u8),
) -> Result<(), Error> {
    let plan = Plan {
        options: options.clone(),
        cfg: cfg.clone(),
        dispatch: dispatch.clone(),
    };
    let entry: Entry = Box::new(move |handle| {
        let result = worker(handle, &plan, &mut out);
        if let Err(error @ Error::Quarantined) = &result {
            // Best effort: the process is about to end and there is nowhere left to report to.
            let _ = writeln!(out, "Error: {error}");
            let _ = out.flush();
            terminate(QUARANTINE_EXIT);
        }
        result
    });
    run(entry).map_err(|_| Error::IccUnavailable)?
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
        terminate,
    )
}
