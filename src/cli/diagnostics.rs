//! Bounded host diagnostics, sanitized probe reports and foreground shutdown signals.
//!
//! References:
//! - `docs/architecture.md`, session hygiene and security model

use std::io::Write;
use std::net::{SocketAddr, TcpListener};

use super::Error;
use super::first_frame::FirstFrameCause;
use crate::config::{Config, Streams};
use crate::onvif::discovery::{self, EndpointReference, Interface, ServiceAddress};
use crate::ptp::session::SessionConfig;
use crate::sony::{Battery, Capabilities, Protocol, SonyCamera, SonyConfig, SonyError};
use crate::supervisor::{CameraFactory, check_first_frame};

pub(super) fn level(level: &str) -> tracing::Level {
    match level {
        "error" => tracing::Level::ERROR,
        "warn" => tracing::Level::WARN,
        "debug" => tracing::Level::DEBUG,
        "trace" => tracing::Level::TRACE,
        _ => tracing::Level::INFO,
    }
}

pub(super) fn check_host(cfg: &Config, now: i64) -> Result<(), Error> {
    // Reject a reset clock before starting listeners or touching the camera. WSSE relies on
    // meaningful UTC. This is plausibility, not a claim that an NTP server was contacted.
    if !(1_577_836_800..4_102_444_800).contains(&now) {
        return Err(Error::Clock);
    }
    let _rtsp = TcpListener::bind(SocketAddr::new(cfg.rtsp_address, cfg.rtsp_port))
        .map_err(|_| Error::Ports)?;
    let _onvif = TcpListener::bind(SocketAddr::new(cfg.onvif_address, cfg.onvif_port))
        .map_err(|_| Error::Ports)?;
    Ok(())
}

pub(super) async fn probe<F: CameraFactory>(factory: F, out: &mut dyn Write) -> Result<(), Error> {
    let (protocol, capabilities, battery, _) = inspect(factory, SonyConfig::default(), false)
        .await?
        .map_err(|failure| Error::Probe(FirstFrameCause::of(&failure.error, false)))?;
    report(out, protocol, &capabilities, battery)
}

/// `probe` plus the step `run` waits on: fetch the first live-view frame and apply `run`'s own
/// check of it under `streams`. A failure names its likely cause ([`FirstFrameCause`]). The
/// driver's deadlines are injected so a missing frame is testable fast.
pub(super) async fn doctor_camera<F: CameraFactory>(
    factory: F, config: SonyConfig, streams: &Streams, out: &mut dyn Write,
) -> Result<(), Error> {
    let (protocol, capabilities, battery, frame) = inspect(factory, config, true)
        .await?
        .map_err(|failure| Error::FirstFrame(failure.cause(streams)))?;
    // The very check `run` makes of its first frame, so ready here means `run` accepts it.
    check_first_frame(&frame, streams)
        .map_err(|error| Error::FirstFrame(FirstFrameCause::of_pipeline(&error)))?;
    report(out, protocol, &capabilities, battery)
}

/// Produce the allowlisted camera portion of a public support report.
pub(super) async fn doctor_report<F: CameraFactory>(
    factory: F, streams: &Streams, out: &mut dyn Write,
) -> Result<(), Error> {
    write_camera_report(
        inspect(factory, SonyConfig::default(), true).await?,
        streams,
        out,
    )
}

fn write_camera_report(
    inspected: Result<Inspected, Failure>, streams: &Streams, out: &mut dyn Write,
) -> Result<(), Error> {
    match inspected {
        Ok((protocol, capabilities, battery, frame)) => {
            report_observations(out, protocol, &capabilities, battery, &frame)?;
            check_first_frame(&frame, streams)
                .map_err(|error| Error::FirstFrame(FirstFrameCause::of_pipeline(&error)))
        }
        Err(failure) => {
            if let Some((protocol, capabilities, battery)) = &failure.observed {
                return report_observations(
                    out,
                    *protocol,
                    capabilities,
                    *battery,
                    failure.frame.as_deref().unwrap_or_default(),
                )
                .and(Err(Error::FirstFrame(failure.cause(streams))));
            }
            Err(Error::FirstFrame(failure.cause(streams)))
        }
    }
}

/// Print the observations available even if the subsequent check or session close failed.
fn report_observations(
    out: &mut dyn Write, protocol: Protocol, capabilities: &Capabilities, battery: Option<Battery>,
    frame: &[u8],
) -> Result<(), Error> {
    use crate::sony::Capability;
    let dimensions = crate::media::JpegDecoder::dimensions(frame);
    writeln!(out, "protocol: {protocol:?}")?;
    for (name, capability) in [
        ("live_view", Capability::LiveView),
        ("still_capture", Capability::StillCapture),
        ("set_property", Capability::SetProperty),
        ("battery", Capability::Battery),
        ("priority_mode", Capability::PriorityMode),
    ] {
        writeln!(
            out,
            "capability_{name}: {}",
            capabilities.supports(capability)
        )?;
    }
    if let Ok((width, height)) = dimensions {
        writeln!(out, "frame: {width} x {height}")?;
    }
    report_battery(out, battery)
}

pub(super) fn report_battery(out: &mut dyn Write, battery: Option<Battery>) -> Result<(), Error> {
    let percent = battery.and_then(|reading| reading.percent);
    let power = report_power(battery);
    if let Some(percent) = percent {
        writeln!(out, "battery_percent: {percent}")?;
    } else {
        writeln!(out, "battery_percent: unknown")?;
    }
    writeln!(out, "power: {power}")?;
    Ok(())
}

pub(super) fn report_power(battery: Option<Battery>) -> &'static str {
    battery.map_or("unknown", |reading| match reading.source {
        crate::sony::PowerSource::Battery => "battery",
        crate::sony::PowerSource::Usb => "usb",
        crate::sony::PowerSource::NoBattery => "no_battery",
        crate::sony::PowerSource::Unknown => "unknown",
    })
}

/// A failed walk of the session: the driver error, whether it came while waiting for the first
/// frame (as opposed to for live view to become ready), and the frame if one was fetched. The
/// driver's error wins over a close error, so an error with a frame in hand is the close.
struct Failure {
    error: SonyError,
    waiting_for_frame: bool,
    frame: Option<Vec<u8>>,
    observed: Option<(Protocol, Capabilities, Option<Battery>)>,
}

impl Failure {
    fn cause(&self, streams: &Streams) -> FirstFrameCause {
        match &self.frame {
            None => FirstFrameCause::of(&self.error, self.waiting_for_frame),
            // A frame run would reject is the more actionable finding. Otherwise only the
            // close failed: never ready, and never blamed on the missing frame.
            Some(frame) => check_first_frame(frame, streams).map_or_else(
                |error| FirstFrameCause::of_pipeline(&error),
                |()| FirstFrameCause::CloseFailed,
            ),
        }
    }
}

type Inspected = (Protocol, Capabilities, Option<Battery>, Vec<u8>);

/// Open the camera and walk the session as far as `check_frame` asks. The outer error is the
/// factory's; the inner one is the [`Failure`].
async fn inspect<F: CameraFactory>(
    mut factory: F, config: SonyConfig, check_frame: bool,
) -> Result<Result<Inspected, Failure>, Error> {
    let transport = factory.connect().await?.ok_or(Error::NoCamera)?;
    let mut session = factory.session(transport, SessionConfig::default(), std::time::Instant::now);
    let mut waiting_for_frame = false;
    let mut frame = None;
    let mut observed = None;
    let report = SonyCamera::run(&mut session, config, async |camera| {
        observed = Some((camera.protocol(), camera.capabilities(), None));
        camera.wait_live_view_ready().await?;
        observed = Some((camera.protocol(), camera.capabilities(), camera.battery()));
        if check_frame {
            waiting_for_frame = true;
            frame = Some(camera.next_live_view_frame().await?);
        }
        Ok((camera.protocol(), camera.capabilities(), camera.battery()))
    })
    .await;
    let transport = factory.recover(session);
    factory.release(transport);
    Ok(match report {
        Ok((protocol, capabilities, battery)) => {
            Ok((protocol, capabilities, battery, frame.unwrap_or_default()))
        }
        Err(error) => Err(Failure {
            error,
            waiting_for_frame,
            frame,
            observed,
        }),
    })
}

fn report(
    out: &mut dyn Write, protocol: Protocol, capabilities: &Capabilities, battery: Option<Battery>,
) -> Result<(), Error> {
    writeln!(
        out,
        "Camera: PC Remote; protocol {protocol:?}\nCapabilities: {capabilities:?}\nBattery/power: {battery:?}"
    )?;
    Ok(())
}

pub(super) fn local_discovery(
    cfg: &Config, interfaces: std::io::Result<Vec<if_addrs::Interface>>,
) -> Result<discovery::Config, Error> {
    discovery(cfg, interfaces?)
}

/// Startup discovery that keeps following the host's interfaces while running.
pub(super) fn watched_discovery(
    cfg: &Config, list: fn() -> std::io::Result<Vec<if_addrs::Interface>>,
) -> Result<discovery::Config, Error> {
    local_discovery(cfg, list()).map(|d| d.with_watch(interface_watch(cfg, list)))
}

/// Which host interfaces discovery serves, decided from the configuration alone so the startup
/// snapshot and every later refresh agree.
#[derive(Clone, Debug)]
struct Selector {
    interface: Option<String>,
    address: std::net::IpAddr,
}

impl Selector {
    fn new(cfg: &Config) -> Self {
        Self {
            interface: cfg.interface.clone(),
            address: cfg.onvif_address,
        }
    }

    /// Loopback, IPv4 link-local and point-to-point (VPN tunnel) addresses never receive probes
    /// from a LAN client, and a tunnel must not learn the camera exists. IPv6 link-local entries
    /// are kept: `discovery::Config` retains them only for subnet matching next to a routable
    /// address on the same interface.
    fn unusable_by_default(i: &if_addrs::Interface) -> bool {
        i.is_p2p()
            || i.ip().is_loopback()
            || matches!(i.ip(), std::net::IpAddr::V4(a) if a.is_link_local())
    }

    fn select(&self, interfaces: Vec<if_addrs::Interface>) -> Vec<Interface> {
        // Naming an interface or a specific address is an explicit choice and is honored as given.
        let skip_unusable = self.interface.is_none() && self.address.is_unspecified();
        interfaces
            .into_iter()
            .filter(|i| {
                i.is_oper_up()
                    && !(skip_unusable && Self::unusable_by_default(i))
                    && self.interface.as_ref().is_none_or(|name| name == &i.name)
                    && (self.address.is_unspecified() || self.address == i.ip())
                    && self.address.is_ipv4() == i.ip().is_ipv4()
            })
            .map(|i| {
                let prefix_len = match &i.addr {
                    if_addrs::IfAddr::V4(a) => a.prefixlen,
                    if_addrs::IfAddr::V6(a) => a.prefixlen,
                };
                Interface {
                    addr: i.ip(),
                    prefix_len,
                    index: i.index.unwrap_or(0),
                }
            })
            .collect()
    }
}

/// Follow the host's interfaces while running: re-enumerate through `list` every
/// [`discovery::WATCH_INTERVAL`] and apply the same selection as startup. A failed enumeration
/// keeps the current set.
pub(super) fn interface_watch(
    cfg: &Config, list: fn() -> std::io::Result<Vec<if_addrs::Interface>>,
) -> discovery::Watch {
    interface_watch_every(cfg, discovery::WATCH_INTERVAL, list)
}

fn interface_watch_every(
    cfg: &Config, every: std::time::Duration,
    list: fn() -> std::io::Result<Vec<if_addrs::Interface>>,
) -> discovery::Watch {
    let selector = Selector::new(cfg);
    discovery::Watch::new(every, move || list().ok().map(|l| selector.select(l)))
}

/// Startup discovery configuration. With no usable interface yet (a service starting before the
/// network is up) it serves nothing until the [`interface_watch`] finds one, rather than failing.
pub(super) fn discovery(
    cfg: &Config, interfaces: Vec<if_addrs::Interface>,
) -> Result<discovery::Config, Error> {
    let interfaces = Selector::new(cfg).select(interfaces);
    discovery::Config::idle(
        EndpointReference::from_uuid(cfg.endpoint_uuid),
        crate::supervisor::device_description(cfg).scopes(),
        ServiceAddress {
            port: cfg.onvif_port,
            path: "/onvif/device_service".into(),
        },
    )
    .and_then(|c| c.with_interfaces_capped(interfaces, &[]))
    .map_err(|_| Error::Interface)
}

/// OS signal registrations are established before the runtime starts. A failure is diagnostic,
/// never a silent fallback that leaves SIGTERM unable to close a camera session.
#[cfg(unix)]
pub(super) struct Signals {
    interrupt: tokio::signal::unix::Signal,
    terminate: tokio::signal::unix::Signal,
}

#[cfg(unix)]
impl Signals {
    pub(super) fn install() -> std::io::Result<Self> {
        use tokio::signal::unix::{SignalKind, signal};
        Self::from_signals(
            signal(SignalKind::interrupt()),
            signal(SignalKind::terminate()),
        )
    }

    pub(super) fn from_signals(
        interrupt: std::io::Result<tokio::signal::unix::Signal>,
        terminate: std::io::Result<tokio::signal::unix::Signal>,
    ) -> std::io::Result<Self> {
        Ok(Self {
            interrupt: interrupt?,
            terminate: terminate?,
        })
    }

    pub(super) async fn wait(mut self) {
        tokio::select! { _ = self.interrupt.recv() => {}, _ = self.terminate.recv() => {} }
    }
}

#[cfg(not(unix))]
pub(super) struct Signals;
#[cfg(not(unix))]
impl Signals {
    pub(super) fn install() -> std::io::Result<Self> {
        Ok(Self)
    }
    pub(super) async fn wait(self) {
        let _ = tokio::signal::ctrl_c().await;
    }
}
