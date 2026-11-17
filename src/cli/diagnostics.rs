//! Bounded host diagnostics, sanitized probe reports and foreground shutdown signals.
//!
//! References:
//! - `docs/architecture.md`, session hygiene and security model

use std::io::Write;
use std::net::{SocketAddr, TcpListener};

use super::Error;
use crate::config::Config;
use crate::onvif::discovery::{self, EndpointReference, Interface, ServiceAddress};
use crate::ptp::session::SessionConfig;
use crate::sony::{SonyCamera, SonyConfig};
use crate::supervisor::CameraFactory;

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

pub(super) async fn probe<F: CameraFactory>(
    mut factory: F, out: &mut dyn Write,
) -> Result<(), Error> {
    let transport = factory.connect().await?.ok_or(Error::NoCamera)?;
    let mut session = factory.session(transport, SessionConfig::default(), std::time::Instant::now);
    let report = SonyCamera::run(&mut session, SonyConfig::default(), async |camera| {
        camera.wait_live_view_ready().await?;
        Ok((camera.protocol(), camera.capabilities(), camera.battery()))
    })
    .await;
    let transport = factory.recover(session);
    factory.release(transport);
    let (protocol, capabilities, battery) = report.map_err(|_| Error::Camera)?;
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
