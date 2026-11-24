//! Compile-time checks for the intentional default-feature library entry points.
//!
//! References:
//! - `docs/architecture.md`, 1.0 compatibility boundary

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
}
