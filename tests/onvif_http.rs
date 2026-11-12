//! End-to-end tests of the ONVIF HTTP server over real loopback sockets. Every server binds
//! `127.0.0.1:0`; no fixed ports.

#[cfg(test)]
mod tests {

    use std::io::{Read, Write};
    use std::net::{SocketAddr, TcpStream};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use tethercam::onvif::auth::Credentials;
    use tethercam::onvif::auth::digest::DigestAuth;
    use tethercam::onvif::auth::wsse::UsernameToken;
    use tethercam::onvif::http::{HttpLimits, HttpServer};
    use tethercam::onvif::model::example_model;
    use tethercam::onvif::service::{OnvifService, SnapshotError, SnapshotSource};
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

    /// Trickle `payload` one byte every `gap` and return how long until the server hung up
    /// (a failed write or EOF on read), giving up after `give_up`.
    fn trickle_until_closed(
        addr: SocketAddr, prefix: &[u8], payload: &[u8], gap: Duration, give_up: Duration,
    ) -> Option<Duration> {
        let mut s = connect(addr);
        s.set_read_timeout(Some(Duration::from_millis(1))).unwrap();
        s.write_all(prefix).unwrap();
        let started = Instant::now();
        for byte in payload.iter().cycle() {
            if started.elapsed() > give_up {
                return None;
            }
            if s.write_all(&[*byte]).is_err() {
                return Some(started.elapsed());
            }
            let mut probe = [0_u8; 64];
            if let Ok(0) = s.read(&mut probe) {
                return Some(started.elapsed());
            }
            std::thread::sleep(gap);
        }
        None
    }

    fn assert_serves_new_client(addr: SocketAddr) {
        // The slot is released asynchronously when the worker thread exits; poll briefly.
        for _ in 0..50 {
            let mut s = connect(addr);
            s.write_all(soap_post("/onvif/device_service", &envelope("", GET_TIME), "").as_bytes())
                .unwrap();
            if read_reply(&mut s).status == 200 {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("server never served a new client");
    }

    fn short_limits() -> HttpLimits {
        HttpLimits {
            max_connections: 1,
            head_timeout: Duration::from_millis(300),
            body_timeout: Duration::from_millis(300),
            idle_timeout: Duration::from_millis(300),
            write_timeout: Duration::from_millis(300),
            ..HttpLimits::default()
        }
    }

    #[test]
    fn trickled_body_is_cut_at_the_absolute_deadline() {
        // One slot, a declared 64 KiB body, one byte every 50 ms: every read is "fast", but
        // the absolute body deadline (300 ms) must still close the connection.
        let server = start(short_limits());
        let prefix = b"POST /onvif/device_service HTTP/1.1\r\nContent-Length: 65536\r\n\r\n";
        let took = trickle_until_closed(
            server.local_addr(),
            prefix,
            b"a",
            Duration::from_millis(50),
            Duration::from_secs(5),
        )
        .expect("connection was never closed");
        assert!(
            took >= Duration::from_millis(250),
            "closed too early: {took:?}"
        );
        assert!(
            took < Duration::from_secs(3),
            "deadline not enforced: {took:?}"
        );
        assert_serves_new_client(server.local_addr());
    }

    #[test]
    fn trickled_head_is_cut_at_the_absolute_deadline() {
        let server = start(short_limits());
        let took = trickle_until_closed(
            server.local_addr(),
            b"POST /onvif/device_service HTTP/1.1\r\nX-Slow: ",
            b"a",
            Duration::from_millis(50),
            Duration::from_secs(5),
        )
        .expect("connection was never closed");
        assert!(
            took < Duration::from_secs(3),
            "deadline not enforced: {took:?}"
        );
        assert_serves_new_client(server.local_addr());
    }

    #[test]
    fn huge_configured_timeouts_do_not_overflow() {
        let limits = HttpLimits {
            idle_timeout: Duration::MAX,
            head_timeout: Duration::MAX,
            body_timeout: Duration::MAX,
            write_timeout: Duration::MAX,
            ..HttpLimits::default()
        };
        let server = start(limits);
        assert_serves_new_client(server.local_addr());
    }

    #[derive(Debug)]
    struct HugeSnapshot;

    impl SnapshotSource for HugeSnapshot {
        fn snapshot(&self, _token: &str) -> Result<Vec<u8>, SnapshotError> {
            Ok(vec![0xAB; 64 * 1024 * 1024])
        }
    }

    #[test]
    fn client_that_never_reads_is_cut_at_the_write_deadline() {
        // The response is far larger than any socket buffer, so the write blocks. The
        // absolute write deadline must free the only slot.
        let service = OnvifService::new(
            example_model(),
            Credentials::new(USER, PASS).unwrap(),
            Arc::new(SystemClock),
        )
        .unwrap()
        .with_snapshot_source(Arc::new(HugeSnapshot));
        let server = HttpServer::start(
            "127.0.0.1:0".parse().unwrap(),
            Arc::new(service),
            short_limits(),
        )
        .unwrap();
        let mut s = connect(server.local_addr());
        let target = "/onvif/snapshot/main";
        s.write_all(format!("GET {target} HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes())
            .unwrap();
        let challenge = read_reply(&mut s)
            .headers
            .lines()
            .find_map(|l| l.strip_prefix("WWW-Authenticate: "))
            .unwrap()
            .to_owned();
        let auth =
            DigestAuth::client_response(USER, PASS, "GET", target, &challenge, 1, "c").unwrap();
        s.write_all(
            format!("GET {target} HTTP/1.1\r\nHost: x\r\nAuthorization: {auth}\r\n\r\n").as_bytes(),
        )
        .unwrap();
        // Never read. The held connection is the only slot.
        assert_serves_new_client(server.local_addr());
        drop(s);
    }

    #[test]
    fn idle_keepalive_connection_is_closed() {
        let server = start(short_limits());
        let mut s = connect(server.local_addr());
        let mut rest = Vec::new();
        let started = Instant::now();
        let _ = s.read_to_end(&mut rest);
        assert_eq!(rest.len(), 0);
        assert!(started.elapsed() < Duration::from_secs(3));
        assert_serves_new_client(server.local_addr());
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
