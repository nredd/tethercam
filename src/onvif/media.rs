//! ONVIF Media service handlers (Media 1, `ver10/media/wsdl`).
//!
//! Profiles come from the [`DeviceModel`](super::model::DeviceModel) the caller supplied. The
//! device exposes one video source; each profile gets its own encoder configuration and all
//! profiles share one source configuration. Stream URIs carry no credentials.
//!
//! References:
//! - [ONVIF Media Service Specification](https://www.onvif.org/specs/srv/media/ONVIF-Media-Service-Spec.pdf)
//! - [ONVIF Media WSDL](https://www.onvif.org/ver10/media/wsdl/media.wsdl)

use super::model::{StreamProfile, VideoCodec};
use super::service::Context;
use super::soap::{Element, Fault, NS_MEDIA, escape};

/// Path of the Media service.
pub const MEDIA_PATH: &str = "/onvif/media_service";
/// Path prefix of the snapshot endpoint; the profile token follows.
pub const SNAPSHOT_PREFIX: &str = "/onvif/snapshot/";

/// Path of the authenticated camera status endpoint (`GET`, JSON).
pub const STATUS_PATH: &str = "/tethercam/status";

const SOURCE_TOKEN: &str = "video_source_0";
const SOURCE_CONFIG_TOKEN: &str = "video_source_config_0";

fn encoder_token(p: &StreamProfile) -> String {
    format!("video_encoder_{}", p.token)
}

/// Answer a Media service operation with the inner XML of the response body.
pub(crate) fn handle(op: &Element, ctx: &Context<'_>) -> Result<String, Fault> {
    if op.namespace.as_deref() != Some(NS_MEDIA) {
        return Err(Fault::action_not_supported());
    }
    match op.name.as_str() {
        "GetServiceCapabilities" => Ok(format!(
            "<trt:GetServiceCapabilitiesResponse>{}</trt:GetServiceCapabilitiesResponse>",
            ctx.media_capabilities_xml()
        )),
        "GetProfiles" => Ok(format!(
            "<trt:GetProfilesResponse>{}</trt:GetProfilesResponse>",
            ctx.model
                .profiles()
                .iter()
                .map(|p| profile_xml("trt:Profiles", p, ctx))
                .collect::<String>()
        )),
        "GetProfile" => {
            let p = profile_arg(op, ctx)?;
            Ok(format!(
                "<trt:GetProfileResponse>{}</trt:GetProfileResponse>",
                profile_xml("trt:Profile", p, ctx)
            ))
        }
        "GetVideoSources" => Ok(format!(
            "<trt:GetVideoSourcesResponse><trt:VideoSources token=\"{SOURCE_TOKEN}\">{}\
             </trt:VideoSources></trt:GetVideoSourcesResponse>",
            source_body(ctx)
        )),
        "GetVideoSourceConfigurations" => Ok(format!(
            "<trt:GetVideoSourceConfigurationsResponse>{}\
             </trt:GetVideoSourceConfigurationsResponse>",
            source_config_xml("trt:Configurations", ctx)
        )),
        "GetVideoSourceConfiguration" => match op.child_text("ConfigurationToken") {
            Some(SOURCE_CONFIG_TOKEN) => Ok(format!(
                "<trt:GetVideoSourceConfigurationResponse>{}\
                     </trt:GetVideoSourceConfigurationResponse>",
                source_config_xml("trt:Configuration", ctx)
            )),
            _ => Err(Fault::invalid_arg(
                "ter:NoConfig",
                "Configuration does not exist",
            )),
        },
        "GetVideoEncoderConfigurations" => Ok(format!(
            "<trt:GetVideoEncoderConfigurationsResponse>{}\
             </trt:GetVideoEncoderConfigurationsResponse>",
            ctx.model
                .profiles()
                .iter()
                .map(|p| encoder_config_xml("trt:Configurations", p))
                .collect::<String>()
        )),
        "GetVideoEncoderConfiguration" => {
            let token = op.child_text("ConfigurationToken").unwrap_or_default();
            let p = ctx
                .model
                .profiles()
                .iter()
                .find(|p| encoder_token(p) == token)
                .ok_or_else(|| {
                    Fault::invalid_arg("ter:NoConfig", "Configuration does not exist")
                })?;
            Ok(format!(
                "<trt:GetVideoEncoderConfigurationResponse>{}\
                 </trt:GetVideoEncoderConfigurationResponse>",
                encoder_config_xml("trt:Configuration", p)
            ))
        }
        "GetStreamUri" => stream_uri(op, ctx),
        "GetSnapshotUri" => snapshot_uri(op, ctx),
        "GetAudioSources" => Ok("<trt:GetAudioSourcesResponse/>".to_owned()),
        "GetAudioEncoderConfigurations" => {
            Ok("<trt:GetAudioEncoderConfigurationsResponse/>".to_owned())
        }
        "GetMetadataConfigurations" => Ok("<trt:GetMetadataConfigurationsResponse/>".to_owned()),
        _ => Err(Fault::action_not_supported()),
    }
}

fn profile_arg<'a>(op: &Element, ctx: &Context<'a>) -> Result<&'a StreamProfile, Fault> {
    let token = op.child_text("ProfileToken").unwrap_or_default();
    ctx.model.profile(token).ok_or_else(|| {
        Fault::invalid_arg(
            "ter:NoProfile",
            "The requested profile token does not exist",
        )
    })
}

fn source_body(ctx: &Context<'_>) -> String {
    let (w, h, f) = ctx.model.source_format();
    format!(
        "<tt:Framerate>{f}</tt:Framerate><tt:Resolution><tt:Width>{w}</tt:Width>\
         <tt:Height>{h}</tt:Height></tt:Resolution>"
    )
}

fn source_config_xml(tag: &str, ctx: &Context<'_>) -> String {
    let (w, h, _) = ctx.model.source_format();
    format!(
        "<{tag} token=\"{SOURCE_CONFIG_TOKEN}\"><tt:Name>video_source_config</tt:Name>\
         <tt:UseCount>{}</tt:UseCount><tt:SourceToken>{SOURCE_TOKEN}</tt:SourceToken>\
         <tt:Bounds x=\"0\" y=\"0\" width=\"{w}\" height=\"{h}\"/></{tag}>",
        ctx.model.profiles().len()
    )
}

fn encoder_config_xml(tag: &str, p: &StreamProfile) -> String {
    let (encoding, codec_block) = match p.codec {
        VideoCodec::H264(profile) => (
            "H264",
            format!(
                "<tt:H264><tt:GovLength>{}</tt:GovLength><tt:H264Profile>{}</tt:H264Profile></tt:H264>",
                p.frame_rate,
                profile.as_str()
            ),
        ),
        VideoCodec::Jpeg => ("JPEG", String::new()),
    };
    format!(
        "<{tag} token=\"{token}\"><tt:Name>{name}</tt:Name><tt:UseCount>1</tt:UseCount>\
         <tt:Encoding>{encoding}</tt:Encoding><tt:Resolution><tt:Width>{w}</tt:Width>\
         <tt:Height>{h}</tt:Height></tt:Resolution><tt:Quality>5</tt:Quality>\
         <tt:RateControl><tt:FrameRateLimit>{fps}</tt:FrameRateLimit>\
         <tt:EncodingInterval>1</tt:EncodingInterval><tt:BitrateLimit>{kbps}</tt:BitrateLimit>\
         </tt:RateControl>{codec_block}\
         <tt:Multicast><tt:Address><tt:Type>IPv4</tt:Type><tt:IPv4Address>0.0.0.0</tt:IPv4Address>\
         </tt:Address><tt:Port>0</tt:Port><tt:TTL>0</tt:TTL><tt:AutoStart>false</tt:AutoStart>\
         </tt:Multicast><tt:SessionTimeout>PT60S</tt:SessionTimeout></{tag}>",
        token = escape(&encoder_token(p)),
        name = escape(&p.name),
        w = p.width,
        h = p.height,
        fps = p.frame_rate,
        kbps = p.bitrate_kbps,
    )
}

fn profile_xml(tag: &str, p: &StreamProfile, ctx: &Context<'_>) -> String {
    format!(
        "<{tag} token=\"{token}\" fixed=\"true\"><tt:Name>{name}</tt:Name>{vsc}{vec}</{tag}>",
        token = escape(&p.token),
        name = escape(&p.name),
        vsc = source_config_xml("tt:VideoSourceConfiguration", ctx),
        vec = encoder_config_xml("tt:VideoEncoderConfiguration", p),
    )
}

fn media_uri(uri: &str) -> String {
    format!(
        "<trt:MediaUri><tt:Uri>{}</tt:Uri><tt:InvalidAfterConnect>false</tt:InvalidAfterConnect>\
         <tt:InvalidAfterReboot>false</tt:InvalidAfterReboot><tt:Timeout>PT0S</tt:Timeout>\
         </trt:MediaUri>",
        escape(uri)
    )
}

fn stream_uri(op: &Element, ctx: &Context<'_>) -> Result<String, Fault> {
    let p = profile_arg(op, ctx)?;
    if let Some(setup) = op.child("StreamSetup") {
        if setup.child_text("Stream") == Some("RTP-Multicast") {
            return Err(Fault::invalid_arg(
                "ter:InvalidStreamSetup",
                "Multicast streaming is not supported",
            ));
        }
        let proto = setup
            .child("Transport")
            .and_then(|t| t.child_text("Protocol"));
        if !matches!(proto, None | Some("RTSP" | "TCP" | "UDP")) {
            return Err(Fault::invalid_arg(
                "ter:InvalidStreamSetup",
                "Unsupported transport protocol",
            ));
        }
    }
    Ok(format!(
        "<trt:GetStreamUriResponse>{}</trt:GetStreamUriResponse>",
        media_uri(&ctx.rtsp_url(p))
    ))
}

fn snapshot_uri(op: &Element, ctx: &Context<'_>) -> Result<String, Fault> {
    let p = profile_arg(op, ctx)?;
    if !ctx.snapshot {
        return Err(Fault::action_not_supported());
    }
    Ok(format!(
        "<trt:GetSnapshotUriResponse>{}</trt:GetSnapshotUriResponse>",
        media_uri(&ctx.snapshot_url(p))
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::onvif::model::example_model;

    fn op(name: &str, args: &[(&str, &str)]) -> Element {
        Element {
            namespace: Some(NS_MEDIA.into()),
            name: name.into(),
            text: String::new(),
            attributes: vec![],
            children: args
                .iter()
                .map(|(name, text)| Element {
                    namespace: None,
                    name: (*name).into(),
                    text: (*text).into(),
                    attributes: vec![],
                    children: vec![],
                })
                .collect(),
        }
    }

    #[test]
    fn configuration_and_empty_collection_operations() {
        let model = example_model();
        let ctx = Context {
            model: &model,
            local: "192.0.2.10:8080".parse().unwrap(),
            now: 0,
            snapshot: true,
            authenticated: true,
        };
        for (name, args, expected) in [
            ("GetProfiles", vec![], "<trt:GetProfilesResponse>"),
            (
                "GetProfile",
                vec![("ProfileToken", "main")],
                "<trt:GetProfileResponse>",
            ),
            ("GetVideoSources", vec![], "<trt:GetVideoSourcesResponse>"),
            (
                "GetStreamUri",
                vec![("ProfileToken", "main")],
                "<trt:GetStreamUriResponse>",
            ),
            (
                "GetSnapshotUri",
                vec![("ProfileToken", "main")],
                "<trt:GetSnapshotUriResponse>",
            ),
            (
                "GetServiceCapabilities",
                vec![],
                "MaximumNumberOfProfiles=\"2\"",
            ),
            (
                "GetVideoSourceConfigurations",
                vec![],
                "video_source_config_0",
            ),
            (
                "GetVideoSourceConfiguration",
                vec![("ConfigurationToken", "video_source_config_0")],
                "video_source_config_0",
            ),
            (
                "GetVideoEncoderConfigurations",
                vec![],
                "video_encoder_main",
            ),
            (
                "GetVideoEncoderConfiguration",
                vec![("ConfigurationToken", "video_encoder_main")],
                "video_encoder_main",
            ),
            ("GetAudioSources", vec![], "<trt:GetAudioSourcesResponse/>"),
            (
                "GetAudioEncoderConfigurations",
                vec![],
                "<trt:GetAudioEncoderConfigurationsResponse/>",
            ),
            (
                "GetMetadataConfigurations",
                vec![],
                "<trt:GetMetadataConfigurationsResponse/>",
            ),
        ] {
            let result = handle(&op(name, &args), &ctx).unwrap();
            assert!(result.contains(expected), "{name}: {result}");
        }
        error_operations(&ctx);
    }

    fn error_operations(ctx: &Context<'_>) {
        for name in [
            "GetVideoSourceConfiguration",
            "GetVideoEncoderConfiguration",
        ] {
            assert!(handle(&op(name, &[("ConfigurationToken", "bad")]), ctx).is_err());
            assert!(handle(&op(name, &[]), ctx).is_err());
        }
        for name in ["GetStreamUri", "GetSnapshotUri"] {
            assert!(handle(&op(name, &[("ProfileToken", "bad")]), ctx).is_err());
        }
        assert!(handle(&op("unknown", &[]), ctx).is_err());
        assert!(handle(&op("GetProfile", &[("ProfileToken", "bad")]), ctx).is_err());
        let mut wrong_namespace = op("GetProfiles", &[]);
        wrong_namespace.namespace = None;
        assert!(handle(&wrong_namespace, ctx).is_err());
        let mut stream = op("GetStreamUri", &[("ProfileToken", "main")]);
        stream.children.push(Element {
            namespace: None,
            name: "StreamSetup".into(),
            text: String::new(),
            attributes: vec![],
            children: vec![Element {
                namespace: None,
                name: "Transport".into(),
                text: String::new(),
                attributes: vec![],
                children: vec![Element {
                    namespace: None,
                    name: "Protocol".into(),
                    text: "bad".into(),
                    attributes: vec![],
                    children: vec![],
                }],
            }],
        });
        assert!(handle(&stream, ctx).is_err());
        for protocol in ["RTSP", "TCP", "UDP"] {
            stream.children[1].children[0].children[0].text = protocol.into();
            assert!(handle(&stream, ctx).is_ok());
        }
        stream.children[1].children.push(Element {
            namespace: None,
            name: "Stream".into(),
            text: "RTP-Multicast".into(),
            attributes: vec![],
            children: vec![],
        });
        assert!(handle(&stream, ctx).is_err());
        let without_snapshot = Context {
            snapshot: false,
            ..*ctx
        };
        assert!(
            handle(
                &op("GetSnapshotUri", &[("ProfileToken", "main")]),
                &without_snapshot
            )
            .is_err()
        );
    }
}
