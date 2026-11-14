//! Real two-stream runtime throughput with persistent authenticated RTSP consumers.
//!
//! Startup and subscription changes are outside the measured loop. The measurement includes
//! simulated PTP fetch, pacing, JPEG decode, pure-Rust H.264 encode, both packetizers, bounded
//! queues and TCP delivery. No physical device or multicast interface is opened.
//!
//! References:
//! - `docs/architecture.md`, CLI and supervisor contract

use std::hint::black_box;
use std::io::{self, Write};
use std::net::SocketAddr;
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use criterion::{Criterion, Throughput};
use jpeg_encoder::{ColorType, Encoder, SamplingFactor};
use tethercam as camera_api;
use tethercam::config::{Config, Secret};
use tethercam::supervisor::{CameraFactory, OpenError};
use tethercam::transport::sim::{
    SimCamera,
    sony::{SonySimConfig, sony_camera},
};
use tokio::sync::oneshot;

#[path = "../tests/support/runtime_client.rs"]
mod client;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

fn image() -> Result<Vec<u8>> {
    let mut rgb = Vec::with_capacity(1024 * 680 * 3);
    for y in 0..680_u16 {
        for x in 0..1024_u16 {
            rgb.extend_from_slice(&[
                u8::try_from(x % 255)?,
                u8::try_from(y % 255)?,
                u8::try_from((x + y) % 255)?,
            ]);
        }
    }
    let mut bytes = Vec::new();
    let mut encoder = Encoder::new(&mut bytes, 85);
    encoder.set_sampling_factor(SamplingFactor::F_2_1);
    encoder.encode(&rgb, 1024, 680, ColorType::Rgb)?;
    Ok(bytes)
}

struct Factory(SonySimConfig);
impl CameraFactory for Factory {
    type Camera = SimCamera;
    type Session = tethercam::ptp::session::Session<Self::Camera>;

    fn session(
        &self, camera: Self::Camera, mut cfg: tethercam::ptp::session::SessionConfig,
        clock: fn() -> std::time::Instant,
    ) -> Self::Session {
        let _ = &mut cfg;
        Self::Session::new(camera, cfg).with_clock(clock)
    }
    fn recover(&self, session: Self::Session) -> Self::Camera {
        session.into_transport()
    }

    fn connect(
        &mut self,
    ) -> impl Future<Output = std::result::Result<Option<SimCamera>, OpenError>> + Send {
        std::future::ready(Ok(Some(sony_camera(self.0.clone()))))
    }
    fn release(&mut self, camera: SimCamera) {
        drop(camera);
    }
    fn pause(&self, delay: Duration) -> impl Future<Output = ()> + Send {
        tokio::time::sleep(delay)
    }
}

struct Ready(mpsc::SyncSender<()>);
impl Write for Ready {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.starts_with(b"Serving") {
            let _ = self.0.try_send(());
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct Running {
    runtime: tokio::runtime::Runtime,
    stop: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<io::Result<()>>>,
    clients: Option<(client::Client, client::Client)>,
}

impl Running {
    fn start() -> Result<Self> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let mut cfg = Config::generate()?;
        cfg.rtsp_address = "127.0.0.1".parse()?;
        cfg.onvif_address = cfg.rtsp_address;
        let rtsp = std::net::TcpListener::bind("127.0.0.1:0")?;
        let http = std::net::TcpListener::bind("127.0.0.1:0")?;
        cfg.rtsp_port = rtsp.local_addr()?.port();
        cfg.onvif_port = http.local_addr()?.port();
        cfg.streams.frame_rate = 120;
        cfg.streams.bitrate = 3_000_000;
        cfg.credentials.username = Secret::new(client::USER.into());
        cfg.credentials.password = Secret::new(client::PASSWORD.into());
        let address = SocketAddr::new(cfg.rtsp_address, cfg.rtsp_port);
        let jpeg = image()?;
        let factory = Factory(SonySimConfig {
            warmup_reads: 0,
            live_view_jpeg: jpeg.clone(),
            still: jpeg,
            ..SonySimConfig::default()
        });
        let (ready, ready_rx) = mpsc::sync_channel(1);
        let (stop, stopped) = oneshot::channel();
        drop((rtsp, http));
        let task = thread::Builder::new()
            .name("pipeline-bench".into())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()?;
                runtime
                    .block_on(tethercam::supervisor::serve(
                        factory,
                        &cfg,
                        None,
                        async {
                            let _ = stopped.await;
                        },
                        &mut Ready(ready),
                    ))
                    .map(|_| ())
                    .map_err(|_| io::Error::other("simulated runtime failed"))
            })?;
        // Own cleanup before waiting for readiness or performing any fallible client setup.
        let mut running = Self {
            runtime,
            stop: Some(stop),
            task: Some(task),
            clients: None,
        };
        ready_rx.recv_timeout(Duration::from_secs(15))?;
        running.clients = Some(running.runtime.block_on(async {
            let mut main = client::Client::open(address).await?;
            let mut jpeg = client::Client::open(address).await?;
            let main_uri = format!("rtsp://{address}/live/main");
            let jpeg_uri = format!("rtsp://{address}/live/mjpeg");
            main.authenticate(&main_uri).await?;
            jpeg.authenticate(&jpeg_uri).await?;
            let description = main.request("DESCRIBE", &main_uri, "").await?;
            if !String::from_utf8_lossy(&description.body).contains("H264/90000") {
                return Err(io::Error::other("invalid runtime SDP"));
            }
            main.play(&main_uri).await?;
            jpeg.play(&jpeg_uri).await?;
            let first = main.frame(96).await?;
            if !tethercam::media::annexb::contains_idr(&client::annex_b(&first)?) {
                return Err(io::Error::other("runtime emitted no IDR"));
            }
            Ok::<_, io::Error>((main, jpeg))
        })?);
        Ok(running)
    }

    fn frame_pair(&mut self) -> io::Result<usize> {
        let (main, jpeg) = self
            .clients
            .as_mut()
            .ok_or_else(|| io::Error::other("missing benchmark consumers"))?;
        self.runtime.block_on(async {
            let (h264, mjpeg) = tokio::try_join!(main.frame(96), jpeg.frame(26))?;
            Ok(h264.iter().chain(mjpeg.iter()).map(Vec::len).sum())
        })
    }

    fn finish(mut self) -> Result<()> {
        self.clients.take();
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(task) = self.task.take() {
            task.join()
                .map_err(|_| io::Error::other("benchmark runtime thread failed"))??;
        }
        Ok(())
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        self.clients.take();
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(task) = self.task.take() {
            let _ = task.join();
        }
    }
}

fn main() -> Result<()> {
    let mut criterion = Criterion::default().configure_from_args();
    let mut running = Running::start()?;
    let mut failure = None;
    let mut group = criterion.benchmark_group("pipeline_steady_state");
    group.throughput(Throughput::Elements(2));
    group.bench_function("1024x680_h264_and_jpeg_tcp", |b| {
        b.iter(|| {
            if failure.is_some() {
                return 0;
            }
            match running.frame_pair() {
                Ok(bytes) => black_box(bytes),
                Err(error) => {
                    failure = Some(error);
                    0
                }
            }
        });
    });
    group.finish();
    running.finish()?;
    if let Some(error) = failure {
        return Err(error.into());
    }
    criterion.final_summary();
    Ok(())
}
