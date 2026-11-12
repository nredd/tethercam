//! ONVIF endpoint: Device and Media services, WS-Security and a small HTTP server.
//!
//! WS-Discovery lives next to this module and is out of scope here.
//!
//! The layers, bottom to top:
//!
//! - [`time`]: injectable [`time::Clock`], `xs:dateTime` parsing and formatting
//! - [`soap`]: bounded, namespace-aware SOAP 1.2 envelope parser and response builders. DTDs
//!   (and therefore entity declarations) are rejected outright
//! - [`auth`]: WS-Security `UsernameToken` digest with a nonce replay cache and a bounded
//!   clock-skew window, HTTP Digest as the alternative, constant-time comparison
//! - [`model`]: the device and stream description the caller passes in. Nothing
//!   camera-specific is hardcoded in this module tree
//! - [`device`] and [`media`]: the service handlers
//! - [`service`]: [`service::OnvifService`], the transport-independent request router
//! - [`http`]: bounded HTTP/1.1 request parser and a threaded loopback-testable server
//!
//! # Calls implemented
//!
//! Device service (`/onvif/device_service`):
//! - `GetSystemDateAndTime` (pre-auth)
//! - `GetCapabilities` (pre-auth), `GetServices` (pre-auth)
//! - `GetServiceCapabilities`
//! - `GetDeviceInformation`, `GetScopes`, `GetNetworkInterfaces`, `GetHostname`
//!
//! Media service (`/onvif/media_service`):
//! - `GetServiceCapabilities`
//! - `GetProfiles`, `GetProfile`
//! - `GetVideoSources`
//! - `GetVideoSourceConfigurations`, `GetVideoSourceConfiguration`
//! - `GetVideoEncoderConfigurations`, `GetVideoEncoderConfiguration`
//! - `GetStreamUri`, `GetSnapshotUri`
//! - `GetAudioSources`, `GetAudioEncoderConfigurations`, `GetMetadataConfigurations`: always
//!   empty, the device has no audio or metadata
//!
//! Snapshot endpoint: `GET /onvif/snapshot/{profile token}`, HTTP Digest only, served from a
//! caller-supplied [`service::SnapshotSource`].
//!
//! # Where the call list comes from
//!
//! The Home Assistant ONVIF integration (`homeassistant/components/onvif/device.py`, built on
//! `python-onvif-zeep`) makes these calls during setup: `GetServices` with a `GetCapabilities`
//! fallback, `GetSystemDateAndTime`, `GetDeviceInformation`, `GetNetworkInterfaces` (the
//! hardware address becomes the unique id), the Media `GetServiceCapabilities`, `GetProfiles`
//! and `GetStreamUri`. UniFi Protect is closed source. Its documented behavior (the
//! [help article](https://help.ui.com/hc/en-us/articles/26301104828439-Third-Party-Cameras-in-UniFi-Protect))
//! is: authentication `Digest & ws-username token`, a correct device clock (so
//! `GetSystemDateAndTime` is pre-auth and accurate), discovery of the highest and lowest quality
//! streams (so `GetProfiles` / `GetVideoEncoderConfigurations` must carry resolution and bitrate
//! per profile; supply two profiles of different quality) and optional PTZ and audio (not
//! advertised). Its call list is not published, so beyond that it is taken from the ONVIF
//! Profile S / T mandatory media calls plus the common adoption probes (`GetScopes`,
//! `GetHostname`, `GetVideoSources`, `GetVideoSourceConfigurations`,
//! `GetVideoEncoderConfigurations`, `GetSnapshotUri`). Events, PTZ and Imaging services are not
//! advertised, so well-behaved clients do not call them.
//!
//! # Security
//!
//! Everything except the three pre-auth calls requires WS-Security or HTTP Digest. Plaintext
//! `PasswordText` is refused. Authentication failures are indistinguishable on the wire (the
//! variant is only visible to in-process callers and tests). Credentials never appear in
//! `Debug` output, responses or stream URIs.
//!
//! References:
//! - [ONVIF Core Specification](https://www.onvif.org/specs/core/ONVIF-Core-Specification.pdf)
//! - [ONVIF Media Service Specification](https://www.onvif.org/specs/srv/media/ONVIF-Media-Service-Spec.pdf)
//! - [WS-Security UsernameToken Profile 1.1](https://docs.oasis-open.org/wss/v1.1/wss-v1.1-spec-os-UsernameTokenProfile.pdf)
//! - [SOAP 1.2 Part 1](https://www.w3.org/TR/soap12-part1/)
//! - [HTTP Digest, RFC 7616 / RFC 2617](https://www.rfc-editor.org/rfc/rfc7616)

pub mod auth;
pub mod device;
pub mod http;
pub mod media;
pub mod model;
pub mod service;
pub mod soap;
pub mod time;
