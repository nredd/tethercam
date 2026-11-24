//! Compile-time checks for the intentional default-feature library entry points.
//!
//! References:
//! - `docs/architecture.md`, 1.0 compatibility boundary

use std::{io::Write, path::Path, process::ExitCode};

use tethercam::{cli, config, media, onvif, ptp, rtp, rtsp, service, sony, supervisor, transport};

#[test]
fn public_entry_points_remain_reachable() {
    fn accepts_type<T: ?Sized>() {}

    accepts_type::<cli::Options>();
    accepts_type::<config::Config>();
    accepts_type::<media::JpegFrame>();
    accepts_type::<onvif::model::DeviceModel>();
    accepts_type::<ptp::DeviceInfo>();
    accepts_type::<rtp::PacketPool>();
    accepts_type::<rtsp::RtspServer<rtsp::source::ScriptedSource>>();
    accepts_type::<service::Manager>();
    accepts_type::<sony::SonyConfig>();
    accepts_type::<supervisor::CameraStatus>();
    accepts_type::<transport::usb::UsbError>();

    let _: fn(&Path) -> Result<config::Config, config::Error> = config::Config::load;
    let _: fn(&Path) -> Result<config::Config, config::Error> = config::Config::load_or_create;
    let _: fn(usize, usize) -> rtp::PacketPool = rtp::PacketPool::with_capacity;
    let _: fn(&mut dyn Write) -> ExitCode = cli::run;
    let _: fn(bool) -> Result<service::Manager, service::Error> = service::Manager::new;
    let _: fn(&mut service::Manager, service::Action) -> Result<service::Status, service::Error> =
        service::Manager::manage;
    let _: fn(
        &transport::usb::NusbBus,
    ) -> Result<Vec<transport::usb::CameraInfo>, transport::usb::UsbError> =
        transport::usb::list_cameras::<transport::usb::NusbBus>;
    let _ = transport::usb::open_first::<transport::usb::NusbBus>;
    let _ = rtsp::RtspServer::<rtsp::source::ScriptedSource>::bind_mounts;
    let _ = sony_signature;
}

async fn sony_signature(
    session: &mut ptp::session::Session<transport::sim::SimCamera>,
) -> Result<(), sony::SonyError> {
    sony::SonyCamera::run(session, sony::SonyConfig::default(), async |_| Ok(())).await
}
