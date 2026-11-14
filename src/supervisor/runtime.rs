//! Full network runtime over an injected camera factory.
//!
//! References:
//! - `docs/architecture.md`, streams and security model

use std::future::Future;
use std::io::{self, Write};
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use thiserror::Error;

use super::pipeline::Pipeline;
use super::{CameraFactory, Control, PipelineError, Supervisor, SupervisorError};

#[cfg(test)]
#[path = "runtime_tests.rs"]
mod tests;
use crate::config::Config;
use crate::onvif::discovery::{self, Responder};
use crate::onvif::http::{HttpLimits, HttpServer};
use crate::onvif::model::{
    DeviceDescription, DeviceModel, H264Profile, NetworkInterface, StreamProfile, VideoCodec,
};
use crate::onvif::service::{ModelPublisher, OnvifService};
use crate::ptp::session::SessionConfig;
use crate::rtsp::{RtspServer, ServerConfig, StreamPublisher};

/// Startup, serving and cleanup errors. Log only a static event for these: nested transport
/// errors can contain untrusted device text. CLI diagnostics use their category, not Debug.
#[derive(Debug, Error)]
pub enum RunError {
    /// Invalid persisted configuration.
    #[error("invalid configuration")]
    Config(#[from] crate::config::Error),
    /// Invalid actor settings.
    #[error(transparent)]
    Supervisor(#[from] SupervisorError),
    /// Invalid media or encoder settings.
    #[error(transparent)]
    Pipeline(#[from] PipelineError),
    /// RTSP startup failed.
    #[error("RTSP listener unavailable")]
    Rtsp(#[from] crate::rtsp::ServerError),
    /// ONVIF startup failed.
    #[error("ONVIF listener unavailable")]
    Http(#[from] crate::onvif::http::ServerError),
    /// Invalid advertised model.
    #[error("invalid network camera description")]
    Model(#[from] crate::onvif::model::ModelError),
    /// Authentication initialization failed.
    #[error("RTSP authentication initialization failed")]
    Auth(#[from] crate::rtsp::digest::DigestError),
    /// Invalid ONVIF credentials.
    #[error("ONVIF credentials rejected")]
    OnvifAuth(#[from] crate::onvif::auth::AuthError),
    /// ONVIF authentication entropy failed.
    #[error("ONVIF authentication initialization failed")]
    OnvifSetup(#[from] crate::onvif::service::SetupError),
    /// The camera actor ended before providing any metadata.
    #[error("camera actor stopped before startup")]
    CameraStopped,
    /// A joined CPU or cleanup worker failed.
    #[error("runtime worker failed")]
    Worker,
    /// Discovery socket or worker failed.
    #[error("discovery unavailable")]
    Discovery,
    /// Operator output could not be written.
    #[error("cannot write operator output")]
    Output(#[from] io::Error),
}

fn model(cfg: &Config, width: u32, height: u32) -> Result<DeviceModel, RunError> {
    let [first, rest @ ..] = *cfg.endpoint_uuid.as_bytes();
    let hw_address = std::iter::once((first | 0x02) & 0xfe)
        .chain(rest)
        .take(6)
        .map(|byte| format!("{byte:02x}"))
        .collect::<Vec<_>>()
        .join(":");
    let profile = |token: &str, codec| StreamProfile {
        name: token.into(),
        token: token.into(),
        codec,
        width,
        height,
        frame_rate: cfg.streams.frame_rate,
        bitrate_kbps: cfg.streams.bitrate.div_ceil(1000),
        rtsp_path: format!("/live/{token}"),
    };
    let mut profiles = Vec::new();
    if cfg.streams.h264 {
        profiles.push(profile("main", VideoCodec::H264(H264Profile::Baseline)));
    }
    if cfg.streams.mjpeg {
        profiles.push(profile("mjpeg", VideoCodec::Jpeg));
    }
    Ok(DeviceModel::new(
        DeviceDescription {
            manufacturer: "tethercam".into(),
            model: "USB camera bridge".into(),
            firmware_version: env!("CARGO_PKG_VERSION").into(),
            serial_number: cfg.endpoint_uuid.to_string(),
            hardware_id: "tethercam".into(),
            hostname: cfg.device_name.clone(),
            interface: NetworkInterface {
                name: cfg.interface.clone().unwrap_or_else(|| "default".into()),
                hw_address,
            },
        },
        profiles,
        cfg.rtsp_port,
    )?)
}

/// Serve the actual media/network pipeline over a camera factory. `shutdown` is a request:
/// after it resolves, this future waits for all camera operations, `CloseSession`, RTSP
/// connections and the CPU/discovery workers. Never abort/drop this future to stop it.
/// `discovery: None` disables only multicast announcements, useful for isolated simulator runs.
///
/// # Errors
/// Returns a typed startup/media failure after the actor's cleanup has completed.
pub async fn serve<F: CameraFactory>(
    factory: F, cfg: &Config, discovery: Option<discovery::Config>,
    shutdown: impl Future<Output = ()>, out: &mut dyn Write,
) -> Result<F, RunError> {
    serve_until(factory, cfg, discovery, Box::pin(shutdown), out).await
}

async fn serve_until<F: CameraFactory>(
    factory: F, cfg: &Config, discovery: Option<discovery::Config>, mut shutdown: Shutdown<'_>,
    out: &mut dyn Write,
) -> Result<F, RunError> {
    let interval = Duration::from_secs(1)
        .checked_div(cfg.streams.frame_rate)
        .unwrap_or(Duration::ZERO);
    // Construct only bounded channels before validation; no factory call or camera I/O occurs.
    let (actor, control) = Supervisor::new(
        factory,
        crate::sony::SonyConfig::default(),
        SessionConfig::default(),
        interval,
    )?;
    let actor = actor.with_still_snapshots(cfg.streams.full_resolution_snapshot);
    cfg.validate()?;
    tracing::info!(event = "waiting_camera");
    let network = async {
        let result = serve_network(cfg, &control, discovery, &mut shutdown, out).await;
        control.shutdown();
        result
    };
    // Neither sibling is abandoned when the other finishes. Only the actor owns PTP.
    let (factory, result) = tokio::join!(actor.run(), network);
    result
        .inspect(|()| tracing::info!(event = "runtime_stopped"))
        .inspect_err(|_| tracing::error!(event = "runtime_failed"))
        .map(|()| factory)
}

fn worker_result<T>(result: Result<T, tokio::task::JoinError>) -> Result<T, RunError> {
    result.map_err(|_| RunError::Worker)
}

async fn run_discovery(
    control: &Control,
    responder: Option<
        Responder<discovery::UdpTransport, discovery::SystemEntropy, discovery::SystemClock>,
    >,
) -> Result<(), RunError> {
    let Some(mut responder) = responder else {
        return Ok(());
    };
    let flag = Arc::new(AtomicBool::new(control.stopping()));
    let worker_flag = Arc::clone(&flag);
    let handle = tokio::task::spawn_blocking(move || responder.run(&worker_flag));
    finish_discovery(control, handle, flag).await
}

async fn finish_discovery(
    control: &Control, mut handle: tokio::task::JoinHandle<Result<(), discovery::Error>>,
    flag: Arc<AtomicBool>,
) -> Result<(), RunError> {
    let mut stop = control.stop.subscribe();
    let result = if control.stopping() {
        handle.await
    } else {
        tokio::select! {
            result = &mut handle => { control.shutdown(); result },
            _ = stop.changed() => {
                flag.store(true, Ordering::SeqCst);
                handle.await
            },
        }
    };
    worker_result(result)?.map_err(|_| RunError::Discovery)
}

/// Wait for the first live-view frame, which defines the stream format. `None` means a stop was
/// requested first.
async fn first_frame(control: &Control) -> Result<Option<Arc<[u8]>>, RunError> {
    let mut frames = control.frames.clone();
    let mut stop = control.stop.subscribe();
    loop {
        if control.stopping() {
            return Ok(None);
        }
        if let Some(frame) = frames.borrow_and_update().clone() {
            return Ok(Some(frame));
        }
        // Stop first: the actor drops the frame slot on its way out of a stop request, and a
        // closed slot during shutdown is not an error.
        tokio::select! {
            biased;
            _ = stop.changed() => {},
            changed = frames.changed() => changed.map_err(|_| RunError::CameraStopped)?,
        }
    }
}

type Shutdown<'a> = Pin<Box<dyn Future<Output = ()> + 'a>>;

/// Listeners bound before any camera exists, and the one-shot publishers that give them their
/// streams and model once the first frame defines the format.
struct Listeners {
    rtsp: RtspServer<super::RuntimeSource>,
    streams: StreamPublisher<super::RuntimeSource>,
    model: ModelPublisher,
}

async fn serve_network(
    cfg: &Config, control: &Control, discovery_cfg: Option<discovery::Config>,
    shutdown: &mut Shutdown<'_>, out: &mut dyn Write,
) -> Result<(), RunError> {
    let (listeners, http) = start_listeners(cfg, control).await?;
    let result = serve_started(cfg, control, discovery_cfg, shutdown, out, listeners).await;
    control.shutdown();
    // HTTP connections may be waiting for an actor snapshot. Join them off the executor so
    // the actor remains runnable through shutter release and CloseSession, even on an error.
    worker_result(tokio::task::spawn_blocking(move || drop(http)).await).and(result)
}

/// The path of the primary RTSP mount.
fn primary_path(cfg: &Config) -> &'static str {
    if cfg.streams.h264 {
        "/live/main"
    } else {
        "/live/mjpeg"
    }
}

/// Bind RTSP and ONVIF HTTP now, with no stream or model. Their format comes from the first
/// frame, never a guess, so until then RTSP answers `503` to `DESCRIBE`/`SETUP`, the ONVIF
/// Device and Media services answer `503`, and only the status endpoint reports state. Binding
/// early also fails a busy port or bad address at startup instead of whenever a camera appears.
async fn start_listeners(
    cfg: &Config, control: &Control,
) -> Result<(Listeners, HttpServer), RunError> {
    let onvif_credentials = crate::onvif::auth::Credentials::new(
        cfg.credentials.username.expose(),
        cfg.credentials.password.expose(),
    )?;
    let credentials = crate::rtsp::digest::Credentials::new(
        cfg.credentials.username.expose(),
        cfg.credentials.password.expose(),
        "tethercam",
    )?;
    let (rtsp, streams) = RtspServer::bind_pending(
        SocketAddr::new(cfg.rtsp_address, cfg.rtsp_port),
        credentials,
        ServerConfig {
            mount_path: primary_path(cfg).into(),
            ..ServerConfig::default()
        },
    )
    .await?;
    let pending =
        OnvifService::pending(onvif_credentials, Arc::new(crate::onvif::time::SystemClock));
    let (http, model) = start_http(cfg, control, pending)?;
    Ok((
        Listeners {
            rtsp,
            streams,
            model,
        },
        http,
    ))
}

fn start_http(
    cfg: &Config, control: &Control,
    pending: Result<(OnvifService, ModelPublisher), crate::onvif::service::SetupError>,
) -> Result<(HttpServer, ModelPublisher), RunError> {
    let (service, model) = pending?;
    let service = service
        .with_snapshot_source(Arc::new(control.clone()))
        .with_status_source(Arc::new(control.clone()));
    let http = HttpServer::start(
        SocketAddr::new(cfg.onvif_address, cfg.onvif_port),
        Arc::new(service),
        HttpLimits::default(),
    )?;
    Ok((http, model))
}

type DiscoveryResponder =
    Responder<discovery::UdpTransport, discovery::SystemEntropy, discovery::SystemClock>;

fn bind_discovery(
    discovery_cfg: Option<discovery::Config>,
) -> Result<Option<DiscoveryResponder>, RunError> {
    discovery_cfg
        .map(|cfg| {
            discovery::UdpTransport::bind(cfg.interfaces())
                .map(|transport| {
                    // Interfaces that failed to bind must not claim a subnet in the responder.
                    let cfg = cfg.with_usable_interfaces(transport.usable());
                    Responder::new(
                        cfg,
                        discovery::SystemEntropy::default(),
                        transport,
                        discovery::SystemClock,
                    )
                })
                .map_err(|_| RunError::Discovery)
        })
        .transpose()
}

async fn serve_started(
    cfg: &Config, control: &Control, discovery_cfg: Option<discovery::Config>,
    shutdown: &mut Shutdown<'_>, out: &mut dyn Write, listeners: Listeners,
) -> Result<(), RunError> {
    let Listeners {
        rtsp,
        streams,
        model,
    } = listeners;
    // Bound now so a bad interface fails at startup, but announced only once there is a camera
    // to adopt: a device Protect lists before it can serve video would fail adoption.
    let discovery = bind_discovery(discovery_cfg)?;
    writeln!(
        out,
        "Listening for RTSP and ONVIF; waiting for a camera. Status: GET {}",
        crate::onvif::media::STATUS_PATH
    )?;
    out.flush()?;
    tracing::info!(event = "runtime_listening");
    let mut stop = control.stop.subscribe();
    let stop_signal = async {
        tokio::select! {
            () = &mut *shutdown => control.shutdown(),
            _ = stop.changed() => {},
        }
    };
    let mut rtsp_stop = control.stop.subscribe();
    let rtsp = rtsp.serve(async {
        if !*rtsp_stop.borrow() {
            let _ = rtsp_stop.changed().await;
        }
    });
    let live = async {
        let result = serve_live(cfg, control, out, (streams, model), discovery).await;
        control.shutdown();
        result
    };
    let ((), (), live) = tokio::join!(stop_signal, rtsp, live);
    live
}

/// Wait for the camera, publish the streams and model its first frame defines, start
/// discovery, and run the media pipeline until stopped.
async fn serve_live(
    cfg: &Config, control: &Control, out: &mut dyn Write,
    publishers: (StreamPublisher<super::RuntimeSource>, ModelPublisher),
    discovery: Option<DiscoveryResponder>,
) -> Result<(), RunError> {
    let (streams, model_slot) = publishers;
    let Some(first) = first_frame(control).await? else {
        return Ok(());
    };
    let (pipeline, sources) = Pipeline::prepare(&first, &cfg.streams, control.clone())?;
    let description = model(cfg, pipeline.width, pipeline.height)?;
    let [main, jpeg] = sources;
    let (primary, additional) = if cfg.streams.h264 {
        let additional = if cfg.streams.mjpeg {
            vec![("/live/mjpeg".into(), jpeg)]
        } else {
            Vec::new()
        };
        (main, additional)
    } else {
        (jpeg, Vec::new())
    };
    streams.publish(primary, additional)?;
    model_slot.publish(description);
    writeln!(
        out,
        "Serving authenticated RTSP and ONVIF; interrupt to stop."
    )?;
    out.flush()?;
    tracing::info!(event = "runtime_ready");
    let media = async {
        let result = pipeline.run().await;
        control.shutdown();
        result
    };
    let (media, discovery) = tokio::join!(media, run_discovery(control, discovery));
    media.map_err(RunError::from).and(discovery)
}
