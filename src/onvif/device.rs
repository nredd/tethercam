//! ONVIF Device service handlers.
//!
//! References:
//! - [ONVIF Core Specification, Device management](https://www.onvif.org/specs/core/ONVIF-Core-Specification.pdf)
//! - [ONVIF Device WSDL](https://www.onvif.org/ver10/device/wsdl/devicemgmt.wsdl)

use super::service::Context;
use super::soap::{Element, Fault, NS_DEVICE, NS_MEDIA, escape, text_element};
use super::time::civil_from_unix;

/// Path of the Device service.
pub const DEVICE_PATH: &str = "/onvif/device_service";

/// Answer a Device service operation with the inner XML of the response body.
pub(crate) fn handle(op: &Element, ctx: &Context<'_>) -> Result<String, Fault> {
    if op.namespace.as_deref() != Some(NS_DEVICE) {
        return Err(Fault::action_not_supported());
    }
    match op.name.as_str() {
        "GetSystemDateAndTime" => Ok(system_date_and_time(ctx.now)),
        "GetDeviceInformation" => Ok(device_information(ctx)),
        "GetCapabilities" => capabilities(op, ctx),
        "GetServices" => Ok(services(op, ctx)),
        "GetServiceCapabilities" => Ok(service_capabilities(ctx)),
        "GetScopes" => Ok(scopes(ctx)),
        "GetNetworkInterfaces" => Ok(network_interfaces(ctx)),
        "GetHostname" => Ok(hostname(ctx)),
        _ => Err(Fault::action_not_supported()),
    }
}

fn system_date_and_time(now: i64) -> String {
    let c = civil_from_unix(now);
    let stamp = format!(
        "<tt:Time><tt:Hour>{}</tt:Hour><tt:Minute>{}</tt:Minute><tt:Second>{}</tt:Second></tt:Time>\
         <tt:Date><tt:Year>{}</tt:Year><tt:Month>{}</tt:Month><tt:Day>{}</tt:Day></tt:Date>",
        c.hour, c.minute, c.second, c.year, c.month, c.day
    );
    format!(
        "<tds:GetSystemDateAndTimeResponse><tds:SystemDateAndTime>\
         <tt:DateTimeType>Manual</tt:DateTimeType><tt:DaylightSavings>false</tt:DaylightSavings>\
         <tt:TimeZone><tt:TZ>UTC0</tt:TZ></tt:TimeZone>\
         <tt:UTCDateTime>{stamp}</tt:UTCDateTime><tt:LocalDateTime>{stamp}</tt:LocalDateTime>\
         </tds:SystemDateAndTime></tds:GetSystemDateAndTimeResponse>"
    )
}

fn device_information(ctx: &Context<'_>) -> String {
    let d = ctx.model.device();
    format!(
        "<tds:GetDeviceInformationResponse>{}{}{}{}{}</tds:GetDeviceInformationResponse>",
        text_element("tds:Manufacturer", &d.manufacturer),
        text_element("tds:Model", &d.model),
        text_element("tds:FirmwareVersion", &d.firmware_version),
        text_element("tds:SerialNumber", &d.serial_number),
        text_element("tds:HardwareId", &d.hardware_id),
    )
}

fn streaming_capabilities_element() -> &'static str {
    "<tt:StreamingCapabilities><tt:RTPMulticast>false</tt:RTPMulticast>\
     <tt:RTP_TCP>true</tt:RTP_TCP><tt:RTP_RTSP_TCP>true</tt:RTP_RTSP_TCP></tt:StreamingCapabilities>"
}

fn capabilities(op: &Element, ctx: &Context<'_>) -> Result<String, Fault> {
    let want = op.child_text("Category");
    let (device, media) = match want {
        None | Some("" | "All") => (true, true),
        Some("Device") => (true, false),
        Some("Media") => (false, true),
        Some("Analytics" | "Events" | "Imaging" | "PTZ") => {
            return Err(Fault::invalid_arg(
                "ter:NoSuchService",
                "Requested capability category is not supported",
            ));
        }
        Some(_) => {
            return Err(Fault::invalid_arg(
                "ter:InvalidCapabilityCategory",
                "Unknown capability category",
            ));
        }
    };
    let device_xml = device.then(|| {
        format!(
            "<tt:Device><tt:XAddr>{}</tt:XAddr></tt:Device>",
            escape(&ctx.device_url())
        )
    });
    let media_xml = media.then(|| {
        format!(
            "<tt:Media><tt:XAddr>{}</tt:XAddr>{}</tt:Media>",
            escape(&ctx.media_url()),
            streaming_capabilities_element()
        )
    });
    let inner = [device_xml, media_xml]
        .into_iter()
        .flatten()
        .collect::<String>();
    Ok(format!(
        "<tds:GetCapabilitiesResponse><tds:Capabilities>{inner}</tds:Capabilities></tds:GetCapabilitiesResponse>"
    ))
}

fn device_capabilities_xml() -> &'static str {
    r#"<tds:Capabilities><tds:Network IPFilter="false" ZeroConfiguration="false" IPVersion6="false" DynDNS="false"/><tds:Security UsernameToken="true" HttpDigest="true" RELToken="false" TLS1.2="false"/><tds:System DiscoveryResolve="false" DiscoveryBye="false" RemoteDiscovery="false" SystemBackup="false" SystemLogging="false" FirmwareUpgrade="false"/></tds:Capabilities>"#
}

fn services(op: &Element, ctx: &Context<'_>) -> String {
    // Capability details beyond the URLs are only for authenticated callers.
    let include =
        ctx.authenticated && matches!(op.child_text("IncludeCapability"), Some("true" | "1"));
    let entry = |ns: &str, url: &str, caps: &str| {
        format!(
            "<tds:Service><tds:Namespace>{ns}</tds:Namespace><tds:XAddr>{}</tds:XAddr>{}\
             <tds:Version><tt:Major>2</tt:Major><tt:Minor>0</tt:Minor></tds:Version></tds:Service>",
            escape(url),
            if include { caps } else { "" }
        )
    };
    format!(
        "<tds:GetServicesResponse>{}{}</tds:GetServicesResponse>",
        entry(
            NS_DEVICE,
            &ctx.device_url(),
            &format!(
                "<tds:Capabilities>{}</tds:Capabilities>",
                device_capabilities_xml()
            )
        ),
        entry(
            NS_MEDIA,
            &ctx.media_url(),
            &format!(
                "<tds:Capabilities>{}</tds:Capabilities>",
                ctx.media_capabilities_xml()
            )
        ),
    )
}

fn service_capabilities(_ctx: &Context<'_>) -> String {
    format!(
        "<tds:GetServiceCapabilitiesResponse>{}</tds:GetServiceCapabilitiesResponse>",
        device_capabilities_xml()
    )
}

fn scopes(ctx: &Context<'_>) -> String {
    let d = ctx.model.device();
    let item = |s: &str| {
        format!(
            "<tds:Scopes><tt:ScopeDef>Fixed</tt:ScopeDef><tt:ScopeItem>{}</tt:ScopeItem></tds:Scopes>",
            escape(s)
        )
    };
    let enc = |s: &str| {
        s.bytes()
            .map(|b| {
                if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_') {
                    char::from(b).to_string()
                } else {
                    format!("%{b:02X}")
                }
            })
            .collect::<String>()
    };
    let list = [
        "onvif://www.onvif.org/Profile/Streaming".to_owned(),
        "onvif://www.onvif.org/type/video_encoder".to_owned(),
        format!("onvif://www.onvif.org/name/{}", enc(&d.model)),
        format!("onvif://www.onvif.org/hardware/{}", enc(&d.hardware_id)),
    ];
    format!(
        "<tds:GetScopesResponse>{}</tds:GetScopesResponse>",
        list.iter().map(|s| item(s)).collect::<String>()
    )
}

fn network_interfaces(ctx: &Context<'_>) -> String {
    let i = &ctx.model.device().interface;
    format!(
        "<tds:GetNetworkInterfacesResponse><tds:NetworkInterfaces token=\"{name}\">\
         <tt:Enabled>true</tt:Enabled><tt:Info><tt:Name>{name}</tt:Name>\
         <tt:HwAddress>{mac}</tt:HwAddress><tt:MTU>1500</tt:MTU></tt:Info>\
         </tds:NetworkInterfaces></tds:GetNetworkInterfacesResponse>",
        name = escape(&i.name),
        mac = escape(&i.hw_address),
    )
}

fn hostname(ctx: &Context<'_>) -> String {
    format!(
        "<tds:GetHostnameResponse><tds:HostnameInformation><tt:FromDHCP>false</tt:FromDHCP>{}\
         </tds:HostnameInformation></tds:GetHostnameResponse>",
        text_element("tt:Name", &ctx.model.device().hostname)
    )
}
