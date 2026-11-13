//! `tethercam` turns a USB-tethered Sony Alpha camera into a network camera.
//!
//! The camera is driven over a single USB-C cable (power, data and control) using PTP with
//! Sony's vendor extension. Its live view is re-served as authenticated RTSP behind an ONVIF
//! endpoint, so NVRs such as UniFi Protect and hubs such as Home Assistant can adopt it like any
//! other IP camera.
//!
//! The crate is one library plus the default application binary. Off-by-default `test-support`
//! exposes reusable verification infrastructure; `repo-tools` adds a developer utility without
//! changing normal startup or `cargo run` selection. Runtime layers are modules, bottom to top:
//!
//! - [`ptp`]: transport-independent PTP wire format
//! - [`transport`]: USB and simulated byte transports
//! - [`sony`]: Sony SDIO vendor extension
//! - [`media`]: frame pacing, JPEG decode, H.264 encode
//! - [`rtp`] and [`rtsp`]: packetizers and the RTSP server
//! - [`onvif`]: discovery, Device and Media services
//! - [`config`], [`supervisor`], [`service`]: configuration, camera lifecycle, installation
//! - [`cli`]: the binary's entry point logic
//!
//! See `docs/architecture.md` in the repository for the full layer plan.
//!
//! References:
//! - [PTP, ISO 15740](https://www.iso.org/standard/63602.html)
//! - [USB Still Image Capture Device class](https://www.usb.org/document-library/still-image-capture-device-definition-10-and-errata-16-mar-2007)

pub mod cli;
pub mod config;
pub mod media;
pub mod onvif;
pub mod ptp;
pub mod rtp;
pub mod rtsp;
pub mod service;
pub mod sony;
pub mod supervisor;
#[cfg(feature = "test-support")]
pub mod test_support;
pub mod transport;
