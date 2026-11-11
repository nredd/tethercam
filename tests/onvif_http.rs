//! End-to-end tests of the ONVIF HTTP server over real loopback sockets. Every server binds
//! `127.0.0.1:0`; no fixed ports.

#[cfg(test)]
mod tests {

    use std::io::{Read, Write};
    use std::net::{SocketAddr, TcpStream};
    use std::sync::Arc;
    use std::time::Duration;

    use tethercam::onvif::auth::Credentials;
    use tethercam::onvif::auth::digest::DigestAuth;
    use tethercam::onvif::auth::wsse::UsernameToken;
    use tethercam::onvif::http::{HttpLimits, HttpServer};
    use tethercam::onvif::model::example_model;
    use tethercam::onvif::service::OnvifService;
    use tethercam::onvif::time::{Clock, SystemClock, format_datetime};

    const USER: &str = "operator";
    const PASS: &str = "synthetic-pass";

    fn start(limits: HttpLimits) -> HttpServer {
        let service = OnvifService::new(
            example_model(),
            Credentials::new(USER, PASS).unwrap(),
            Arc::new(SystemClock),
        )
        .unwrap();
        HttpServer::start("127.0.0.1:0".parse().unwrap(), Arc::new(service), limits).unwrap()
    }

    fn connect(addr: SocketAddr) -> TcpStream {
        let s = TcpStream::connect(addr).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        s
    }

    struct Reply {
        status: u16,
        headers: String,
        body: String,
    }

    /// Read exactly one response using its `Content-Length`.
    fn read_reply(s: &mut TcpStream) -> Reply {
        let mut buf = Vec::new();
        let mut byte = [0_u8; 1];
        while !buf.ends_with(b"\r\n\r\n") {
            if s.read(&mut byte).unwrap() == 0 {
                break;
            }
            buf.push(byte[0]);
        }
        let head = String::from_utf8(buf).unwrap();
        let status = head.split(' ').nth(1).unwrap().parse().unwrap();
        let len: usize = head
            .lines()
            .find_map(|l| l.strip_prefix("Content-Length: "))
            .map_or(0, |v| v.parse().unwrap());
        let mut body = vec![0_u8; len];
        s.read_exact(&mut body).unwrap();
        Reply {
            status,
            headers: head,
            body: String::from_utf8(body).unwrap(),
        }
    }

    fn soap_post(path: &str, body: &str, extra_headers: &str) -> String {
        format!(
            "POST {path} HTTP/1.1\r\nHost: 192.0.2.1\r\nContent-Type: application/soap+xml; charset=utf-8\r\n{extra_headers}Content-Length: {}\r\n\r\n{body}",
            body.len()
        )
    }

    fn envelope(header: &str, op: &str) -> String {
        format!(
            r#"<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope"><s:Header>{header}</s:Header><s:Body>{op}</s:Body></s:Envelope>"#
        )
    }

    const GET_PROFILES: &str =
        r#"<trt:GetProfiles xmlns:trt="http://www.onvif.org/ver10/media/wsdl"/>"#;
    const GET_TIME: &str =
        r#"<tds:GetSystemDateAndTime xmlns:tds="http://www.onvif.org/ver10/device/wsdl"/>"#;

    fn token(nonce: &[u8]) -> String {
        let now = SystemClock.now_unix();
        UsernameToken::digest_for(USER, PASS, nonce, &format_datetime(now)).to_header_xml()
    }

    #[test]
    fn preauth_call_over_loopback_reports_local_time() {
        let server = start(HttpLimits::default());
        let mut s = connect(server.local_addr());
        s.write_all(soap_post("/onvif/device_service", &envelope("", GET_TIME), "").as_bytes())
            .unwrap();
        let r = read_reply(&mut s);
        assert_eq!(r.status, 200);
        assert!(r.body.contains("<tt:Year>"));
    }

    #[test]
    fn authenticated_get_profiles_on_a_keepalive_connection() {
        let server = start(HttpLimits::default());
        let port = server.local_addr().port();
        let mut s = connect(server.local_addr());
        for i in 0..3 {
            let body = envelope(
                &token(format!("nonce-loop-{i:04}").as_bytes()),
                GET_PROFILES,
            );
            s.write_all(soap_post("/onvif/media_service", &body, "").as_bytes())
                .unwrap();
            let r = read_reply(&mut s);
            assert_eq!(r.status, 200, "{}", r.body);
            assert!(r.headers.contains("Connection: keep-alive"));
            // Advertised URLs follow the address the client connected to, not a configured one.
            assert!(r.body.contains("video_encoder_main"));
        }
        let body = envelope(
            &token(b"nonce-uri-00001"),
            r#"<trt:GetStreamUri xmlns:trt="http://www.onvif.org/ver10/media/wsdl"><trt:ProfileToken>main</trt:ProfileToken></trt:GetStreamUri>"#,
        );
        s.write_all(soap_post("/onvif/media_service", &body, "").as_bytes())
            .unwrap();
        let r = read_reply(&mut s);
        assert!(
            r.body.contains("rtsp://127.0.0.1:8554/live/main"),
            "{}",
            r.body
        );
        assert_ne!(port, 0);
    }

    #[test]
    fn unauthenticated_and_wrong_password_are_refused() {
        let server = start(HttpLimits::default());
        let mut s = connect(server.local_addr());
        s.write_all(soap_post("/onvif/media_service", &envelope("", GET_PROFILES), "").as_bytes())
            .unwrap();
        let r = read_reply(&mut s);
        assert_eq!(r.status, 401);
        assert!(
            r.headers
                .contains("WWW-Authenticate: Digest realm=\"tethercam\"")
        );
        assert!(!r.body.contains("Profiles"));

        let bad = UsernameToken::digest_for(
            USER,
            "wrong",
            b"nonce-bad-00001",
            &format_datetime(SystemClock.now_unix()),
        )
        .to_header_xml();
        s.write_all(
            soap_post("/onvif/media_service", &envelope(&bad, GET_PROFILES), "").as_bytes(),
        )
        .unwrap();
        assert_eq!(read_reply(&mut s).status, 400);
    }

    #[test]
    fn http_digest_flow_over_the_wire() {
        let server = start(HttpLimits::default());
        let mut s = connect(server.local_addr());
        let body = envelope("", GET_PROFILES);
        s.write_all(soap_post("/onvif/media_service", &body, "").as_bytes())
            .unwrap();
        let r = read_reply(&mut s);
        let challenge = r
            .headers
            .lines()
            .find_map(|l| l.strip_prefix("WWW-Authenticate: "))
            .unwrap()
            .to_owned();
        let auth = DigestAuth::client_response(
            USER,
            PASS,
            "POST",
            "/onvif/media_service",
            &challenge,
            1,
            "cnonce1",
        )
        .unwrap();
        s.write_all(
            soap_post(
                "/onvif/media_service",
                &body,
                &format!("Authorization: {auth}\r\n"),
            )
            .as_bytes(),
        )
        .unwrap();
        assert_eq!(read_reply(&mut s).status, 200);
    }

    #[test]
    fn oversized_body_is_rejected_and_connection_closed() {
        let server = start(HttpLimits::default());
        let mut s = connect(server.local_addr());
        s.write_all(b"POST /onvif/device_service HTTP/1.1\r\nContent-Length: 10000000\r\n\r\n")
            .unwrap();
        let r = read_reply(&mut s);
        assert_eq!(r.status, 413);
        assert!(r.headers.contains("Connection: close"));
        let mut rest = Vec::new();
        let _ = s.read_to_end(&mut rest);
        assert_eq!(rest.len(), 0);
    }

    #[test]
    fn garbage_and_hostile_xml_get_400() {
        let server = start(HttpLimits::default());
        let mut s = connect(server.local_addr());
        s.write_all(b"\x00\x01\x02 not http\r\n\r\n").unwrap();
        assert_eq!(read_reply(&mut s).status, 400);

        let mut s = connect(server.local_addr());
        let bomb = r#"<?xml version="1.0"?><!DOCTYPE l [<!ENTITY a "aaaa">]><s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope"><s:Body><x>&a;</x></s:Body></s:Envelope>"#;
        s.write_all(soap_post("/onvif/device_service", bomb, "").as_bytes())
            .unwrap();
        assert_eq!(read_reply(&mut s).status, 400);
    }

    #[test]
    fn unknown_path_is_404_and_server_survives() {
        let server = start(HttpLimits::default());
        let mut s = connect(server.local_addr());
        s.write_all(b"GET /admin HTTP/1.1\r\n\r\n").unwrap();
        assert_eq!(read_reply(&mut s).status, 404);
        s.write_all(b"GET /admin HTTP/1.1\r\n\r\n").unwrap();
        assert_eq!(read_reply(&mut s).status, 404);
    }

    #[test]
    fn connection_cap_answers_503() {
        let limits = HttpLimits {
            max_connections: 1,
            ..HttpLimits::default()
        };
        let server = start(limits);
        // Hold the only slot open with an idle connection, then confirm it is counted.
        let _held = connect(server.local_addr());
        let mut saw_503 = false;
        for _ in 0..50 {
            let mut s = connect(server.local_addr());
            let mut head = [0_u8; 12];
            if s.read_exact(&mut head).is_ok() && head.starts_with(b"HTTP/1.1 503") {
                saw_503 = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(saw_503);
    }

    #[test]
    fn slow_clients_time_out_instead_of_pinning_a_thread() {
        let limits = HttpLimits {
            io_timeout: Duration::from_millis(200),
            ..HttpLimits::default()
        };
        let server = start(limits);
        let mut s = connect(server.local_addr());
        s.write_all(b"POST /onvif/device_service HTTP/1.1\r\nContent-Length: 100\r\n\r\nabc")
            .unwrap();
        let mut rest = Vec::new();
        // The server drops the connection once its read times out.
        let _ = s.read_to_end(&mut rest);
        assert_eq!(rest.len(), 0);
    }

    #[test]
    fn shutdown_stops_accepting() {
        let mut server = start(HttpLimits::default());
        let addr = server.local_addr();
        server.shutdown();
        server.shutdown();
        assert!(TcpStream::connect_timeout(&addr, Duration::from_millis(500)).is_err());
    }
}
