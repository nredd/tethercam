//! Golden request / response tests for the ONVIF Device and Media services, plus the
//! authentication matrix. Requests in `tests/fixtures/onvif/*.request.xml` are shaped like
//! `python-onvif-zeep` (the Home Assistant client) sends them. Set `TETHERCAM_UPDATE_GOLDEN=1`
//! to rewrite the `*.response.xml` files after an intentional change, then review the diff.

#[cfg(test)]
mod tests {

    use std::fs;
    use std::net::SocketAddr;
    use std::path::PathBuf;
    use std::sync::Arc;

    use tethercam::onvif::auth::Credentials;
    use tethercam::onvif::auth::digest::DigestAuth;
    use tethercam::onvif::auth::wsse::UsernameToken;
    use tethercam::onvif::media::SNAPSHOT_PREFIX;
    use tethercam::onvif::model::example_model;
    use tethercam::onvif::service::{
        OnvifService, ServiceRequest, ServiceResponse, SnapshotError, SnapshotSource,
    };
    use tethercam::onvif::soap::{Envelope, SoapLimits};
    use tethercam::onvif::time::{ManualClock, format_datetime};

    const NOW: i64 = 1_750_000_000;
    const USER: &str = "operator";
    const PASS: &str = "synthetic-pass";
    const LOCAL: &str = "192.0.2.10:8080";

    #[derive(Debug)]
    struct FakeSnapshots;

    impl SnapshotSource for FakeSnapshots {
        fn snapshot(&self, token: &str) -> Result<Vec<u8>, SnapshotError> {
            if token == "mjpeg" {
                Ok(vec![0xFF, 0xD8, 0xFF, 0xD9])
            } else {
                Err(SnapshotError)
            }
        }
    }

    struct Rig {
        service: OnvifService,
        clock: Arc<ManualClock>,
    }

    fn rig() -> Rig {
        rig_with_secret([9; 32])
    }

    fn rig_with_secret(secret: [u8; 32]) -> Rig {
        let clock = Arc::new(ManualClock::new(NOW));
        let service = OnvifService::with_digest_secret(
            example_model(),
            Credentials::new(USER, PASS).unwrap(),
            clock.clone(),
            secret,
        )
        .with_snapshot_source(Arc::new(FakeSnapshots));
        Rig { service, clock }
    }

    fn fixture_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/onvif")
    }

    fn security(password: &str, nonce: &[u8], created_at: i64) -> String {
        UsernameToken::digest_for(USER, password, nonce, &format_datetime(created_at))
            .to_header_xml()
    }

    fn post(rig: &Rig, path: &str, body: &str) -> ServiceResponse {
        post_with(rig, path, body, None)
    }

    fn post_with(
        rig: &Rig, path: &str, body: &str, authorization: Option<&str>,
    ) -> ServiceResponse {
        rig.service.handle(&ServiceRequest {
            method: "POST",
            target: path,
            authorization,
            body: body.as_bytes(),
            local: LOCAL.parse::<SocketAddr>().unwrap(),
        })
    }

    fn text(resp: &ServiceResponse) -> String {
        String::from_utf8(resp.body.clone()).unwrap()
    }

    fn endpoint_for(name: &str) -> &'static str {
        if name.starts_with("media_") {
            "/onvif/media_service"
        } else {
            "/onvif/device_service"
        }
    }

    #[test]
    fn golden_responses() {
        let update = std::env::var_os("TETHERCAM_UPDATE_GOLDEN").is_some();
        let mut names: Vec<String> = fs::read_dir(fixture_dir())
            .unwrap()
            .filter_map(|e| {
                let name = e.unwrap().file_name().into_string().unwrap();
                name.strip_suffix(".request.xml").map(str::to_owned)
            })
            .collect();
        names.sort();
        assert!(names.len() >= 17, "fixtures went missing: {names:?}");

        for (i, name) in names.iter().enumerate() {
            let rig = rig();
            let template =
                fs::read_to_string(fixture_dir().join(format!("{name}.request.xml"))).unwrap();
            let nonce = format!("nonce-{i:04}-abcdef");
            let request = template.replace("{{SECURITY}}", &security(PASS, nonce.as_bytes(), NOW));
            let resp = post(&rig, endpoint_for(name), &request);
            assert_eq!(resp.status, 200, "{name}: {}", text(&resp));
            assert_eq!(resp.content_type, "application/soap+xml; charset=utf-8");
            let body = text(&resp);
            // Every response must itself be a well-formed SOAP 1.2 envelope.
            Envelope::parse(body.as_bytes(), &SoapLimits::default())
                .unwrap_or_else(|e| panic!("{name}: {e}"));
            assert!(!body.contains(PASS));

            let golden = fixture_dir().join(format!("{name}.response.xml"));
            if update {
                fs::write(&golden, format!("{body}\n")).unwrap();
            } else {
                let want =
                    fs::read_to_string(&golden).unwrap_or_else(|_| panic!("missing {golden:?}"));
                assert_eq!(body, want.trim_end(), "{name}");
            }
        }
    }

    #[test]
    fn preauth_calls_need_no_credentials() {
        let rig = rig();
        for name in [
            "device_get_system_date_and_time",
            "device_get_capabilities",
            "device_get_services",
        ] {
            let req =
                fs::read_to_string(fixture_dir().join(format!("{name}.request.xml"))).unwrap();
            assert!(!req.contains("{{SECURITY}}"));
            assert_eq!(
                post(&rig, "/onvif/device_service", &req).status,
                200,
                "{name}"
            );
        }
    }

    fn authed_request(name: &str, security: &str) -> String {
        fs::read_to_string(fixture_dir().join(format!("{name}.request.xml")))
            .unwrap()
            .replace("{{SECURITY}}", security)
    }

    #[test]
    fn unauthenticated_get_services_omits_capability_details() {
        let rig = rig();
        let req = authed_request("device_get_services_with_capabilities", "");
        let resp = post(&rig, "/onvif/device_service", &req);
        assert_eq!(resp.status, 200);
        let body = text(&resp);
        assert!(body.contains("device_service") && body.contains("media_service"));
        assert!(!body.contains("MaximumNumberOfProfiles") && !body.contains("SnapshotUri"));
    }

    #[test]
    fn missing_security_header_gets_challenge() {
        let rig = rig();
        let req = authed_request("device_get_device_information", "");
        let resp = post(&rig, "/onvif/device_service", &req);
        assert_eq!(resp.status, 401);
        assert!(
            resp.www_authenticate
                .as_deref()
                .unwrap()
                .starts_with("Digest realm=\"tethercam\"")
        );
        assert!(text(&resp).contains("ter:NotAuthorized"));
        assert!(!text(&resp).contains("Manufacturer"));
    }

    #[test]
    fn wrong_password_is_not_authorized() {
        let rig = rig();
        let req = authed_request(
            "device_get_device_information",
            &security("wrong", b"nonce-wrong-0001", NOW),
        );
        let resp = post(&rig, "/onvif/device_service", &req);
        assert_eq!(resp.status, 400);
        assert!(text(&resp).contains("ter:NotAuthorized"));
        assert!(resp.www_authenticate.is_none());
    }

    #[test]
    fn wrong_user_and_wrong_password_look_identical() {
        let rig = rig();
        let a = authed_request(
            "device_get_device_information",
            &security("wrong", b"nonce-aaaa-0001", NOW),
        );
        let other =
            UsernameToken::digest_for("intruder", PASS, b"nonce-bbbb-0001", &format_datetime(NOW))
                .to_header_xml();
        let b = authed_request("device_get_device_information", &other);
        assert_eq!(
            post(&rig, "/onvif/device_service", &a),
            post(&rig, "/onvif/device_service", &b)
        );
    }

    #[test]
    fn replayed_nonce_is_rejected() {
        let rig = rig();
        let req = authed_request(
            "device_get_device_information",
            &security(PASS, b"nonce-replay-01", NOW),
        );
        assert_eq!(post(&rig, "/onvif/device_service", &req).status, 200);
        let again = post(&rig, "/onvif/device_service", &req);
        assert_eq!(again.status, 400);
        assert!(text(&again).contains("ter:NotAuthorized"));
    }

    #[test]
    fn stale_and_future_created_are_rejected() {
        let rig = rig();
        for (i, at) in [NOW - 301, NOW + 301, NOW - 86_400].into_iter().enumerate() {
            let sec = security(PASS, format!("nonce-stale-{i:03}").as_bytes(), at);
            let resp = post(
                &rig,
                "/onvif/device_service",
                &authed_request("device_get_hostname", &sec),
            );
            assert_eq!(resp.status, 400, "created at {at}");
        }
        // Moving the clock forward makes an otherwise valid token stale.
        let sec = security(PASS, b"nonce-clock-0001", NOW);
        rig.clock.advance(1000);
        assert_eq!(
            post(
                &rig,
                "/onvif/device_service",
                &authed_request("device_get_hostname", &sec)
            )
            .status,
            400
        );
    }

    #[test]
    fn plaintext_password_is_refused() {
        let rig = rig();
        let sec = format!(
            r#"<wsse:Security xmlns:wsse="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-secext-1.0.xsd"><wsse:UsernameToken><wsse:Username>{USER}</wsse:Username><wsse:Password Type="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-username-token-profile-1.0#PasswordText">{PASS}</wsse:Password></wsse:UsernameToken></wsse:Security>"#
        );
        let resp = post(
            &rig,
            "/onvif/device_service",
            &authed_request("device_get_hostname", &sec),
        );
        assert_eq!(resp.status, 400);
    }

    #[test]
    fn http_digest_is_an_alternative_on_soap_endpoints() {
        let rig = rig();
        let req = authed_request("device_get_hostname", "");
        let challenge = post(&rig, "/onvif/device_service", &req)
            .www_authenticate
            .unwrap();
        let auth = DigestAuth::client_response(
            USER,
            PASS,
            "POST",
            "/onvif/device_service",
            &challenge,
            1,
            "c1",
        )
        .unwrap();
        assert_eq!(
            post_with(&rig, "/onvif/device_service", &req, Some(&auth)).status,
            200
        );
    }

    #[test]
    fn wrong_digest_password_gets_401_without_stale() {
        let rig = rig();
        let req = authed_request("device_get_hostname", "");
        let challenge = post(&rig, "/onvif/device_service", &req)
            .www_authenticate
            .unwrap();
        let bad = DigestAuth::client_response(
            USER,
            "wrong",
            "POST",
            "/onvif/device_service",
            &challenge,
            1,
            "c1",
        )
        .unwrap();
        let resp = post_with(&rig, "/onvif/device_service", &req, Some(&bad));
        assert_eq!(resp.status, 401);
        let fresh = resp.www_authenticate.unwrap();
        assert!(fresh.starts_with("Digest realm=\"tethercam\"") && !fresh.contains("stale"));
        // The fresh challenge is usable with the right password.
        let good = DigestAuth::client_response(
            USER,
            PASS,
            "POST",
            "/onvif/device_service",
            &fresh,
            1,
            "c1",
        )
        .unwrap();
        assert_eq!(
            post_with(&rig, "/onvif/device_service", &req, Some(&good)).status,
            200
        );
    }

    #[test]
    fn stale_digest_nonce_gets_stale_true_and_retry_succeeds() {
        let rig = rig();
        let req = authed_request("device_get_hostname", "");
        let challenge = post(&rig, "/onvif/device_service", &req)
            .www_authenticate
            .unwrap();
        rig.clock.advance(1000);
        let late = DigestAuth::client_response(
            USER,
            PASS,
            "POST",
            "/onvif/device_service",
            &challenge,
            1,
            "c1",
        )
        .unwrap();
        let resp = post_with(&rig, "/onvif/device_service", &req, Some(&late));
        assert_eq!(resp.status, 401);
        let fresh = resp.www_authenticate.unwrap();
        assert!(fresh.contains("stale=true"));
        let retry = DigestAuth::client_response(
            USER,
            PASS,
            "POST",
            "/onvif/device_service",
            &fresh,
            1,
            "c1",
        )
        .unwrap();
        assert_eq!(
            post_with(&rig, "/onvif/device_service", &req, Some(&retry)).status,
            200
        );
    }

    #[test]
    fn nonce_from_a_previous_server_run_is_stale_not_denied() {
        let old = rig_with_secret([1; 32]);
        let req = authed_request("device_get_hostname", "");
        let challenge = post(&old, "/onvif/device_service", &req)
            .www_authenticate
            .unwrap();
        let restarted = rig_with_secret([2; 32]);
        let auth = DigestAuth::client_response(
            USER,
            PASS,
            "POST",
            "/onvif/device_service",
            &challenge,
            1,
            "c1",
        )
        .unwrap();
        let resp = post_with(&restarted, "/onvif/device_service", &req, Some(&auth));
        assert_eq!(resp.status, 401);
        assert!(resp.www_authenticate.unwrap().contains("stale=true"));
    }

    #[test]
    fn ws_security_takes_precedence_over_http_digest() {
        let rig = rig();
        let challenge = post(
            &rig,
            "/onvif/device_service",
            &authed_request("device_get_hostname", ""),
        )
        .www_authenticate
        .unwrap();
        let good_digest = DigestAuth::client_response(
            USER,
            PASS,
            "POST",
            "/onvif/device_service",
            &challenge,
            1,
            "c1",
        )
        .unwrap();
        // Bad token plus a good Authorization header: the token decides, SOAP fault, no challenge.
        let bad_token = authed_request(
            "device_get_hostname",
            &security("wrong", b"nonce-both-0001", NOW),
        );
        let resp = post_with(
            &rig,
            "/onvif/device_service",
            &bad_token,
            Some(&good_digest),
        );
        assert_eq!(resp.status, 400);
        assert!(resp.www_authenticate.is_none() && text(&resp).contains("ter:NotAuthorized"));
        // Good token plus a garbage Authorization header: the token decides, success.
        let good_token = authed_request(
            "device_get_hostname",
            &security(PASS, b"nonce-both-0002", NOW),
        );
        let resp = post_with(
            &rig,
            "/onvif/device_service",
            &good_token,
            Some("Digest garbage"),
        );
        assert_eq!(resp.status, 200);
    }

    #[test]
    fn unknown_action_is_a_fault() {
        let rig = rig();
        let body = r#"<?xml version="1.0"?><e:Envelope xmlns:e="http://www.w3.org/2003/05/soap-envelope"><e:Header>{{SECURITY}}</e:Header><e:Body><x:Reboot xmlns:x="http://www.onvif.org/ver10/device/wsdl"/></e:Body></e:Envelope>"#
        .replace("{{SECURITY}}", &security(PASS, b"nonce-unknown-01", NOW));
        let resp = post(&rig, "/onvif/device_service", &body);
        assert_eq!(resp.status, 500);
        assert!(text(&resp).contains("ter:ActionNotSupported"));
    }

    #[test]
    fn media_call_on_device_endpoint_is_unsupported() {
        let rig = rig();
        let req = authed_request(
            "media_get_profiles",
            &security(PASS, b"nonce-cross-0001", NOW),
        );
        let resp = post(&rig, "/onvif/device_service", &req);
        assert!(text(&resp).contains("ter:ActionNotSupported"));
    }

    #[test]
    fn unknown_profile_token_is_no_profile() {
        let rig = rig();
        let req = authed_request(
            "media_get_profile",
            &security(PASS, b"nonce-token-0001", NOW),
        )
        .replace(">main<", ">nope<");
        let resp = post(&rig, "/onvif/media_service", &req);
        assert_eq!(resp.status, 400);
        assert!(text(&resp).contains("ter:NoProfile"));
    }

    #[test]
    fn multicast_stream_setup_is_refused() {
        let rig = rig();
        let req = authed_request(
            "media_get_stream_uri",
            &security(PASS, b"nonce-mcast-0001", NOW),
        )
        .replace("RTP-Unicast", "RTP-Multicast");
        assert!(text(&post(&rig, "/onvif/media_service", &req)).contains("ter:InvalidStreamSetup"));
    }

    #[test]
    fn stream_uri_has_no_credentials_and_uses_the_local_address() {
        let rig = rig();
        let req = authed_request(
            "media_get_stream_uri",
            &security(PASS, b"nonce-uri-000001", NOW),
        );
        let body = text(&post(&rig, "/onvif/media_service", &req));
        assert!(
            body.contains("<tt:Uri>rtsp://192.0.2.10:8554/live/main</tt:Uri>"),
            "{body}"
        );
        assert!(!body.contains('@'));
    }

    #[test]
    fn ipv6_local_address_is_bracketed() {
        let rig = rig();
        let req = authed_request(
            "media_get_stream_uri",
            &security(PASS, b"nonce-v6-0000001", NOW),
        );
        let resp = rig.service.handle(&ServiceRequest {
            method: "POST",
            target: "/onvif/media_service",
            authorization: None,
            body: req.as_bytes(),
            local: "[2001:db8::1]:8080".parse().unwrap(),
        });
        assert!(text(&resp).contains("rtsp://[2001:db8::1]:8554/live/main"));
    }

    #[test]
    fn malformed_and_hostile_bodies_are_rejected() {
        let rig = rig();
        let laughs = r#"<?xml version="1.0"?><!DOCTYPE l [<!ENTITY a "aaaaaaaaaa"><!ENTITY b "&a;&a;&a;&a;&a;&a;">]><e:Envelope xmlns:e="http://www.w3.org/2003/05/soap-envelope"><e:Body><x:GetHostname xmlns:x="http://www.onvif.org/ver10/device/wsdl">&b;</x:GetHostname></e:Body></e:Envelope>"#;
        assert_eq!(post(&rig, "/onvif/device_service", laughs).status, 400);
        assert_eq!(post(&rig, "/onvif/device_service", "not xml").status, 400);
        assert_eq!(post(&rig, "/onvif/device_service", "").status, 400);
        let deep = format!(
            r#"<e:Envelope xmlns:e="http://www.w3.org/2003/05/soap-envelope"><e:Body>{}{}</e:Body></e:Envelope>"#,
            "<a>".repeat(500),
            "</a>".repeat(500)
        );
        assert_eq!(post(&rig, "/onvif/device_service", &deep).status, 400);
        assert_eq!(
            post(&rig, "/onvif/device_service", &" ".repeat(70_000)).status,
            413
        );
        let v11 = r#"<e:Envelope xmlns:e="http://schemas.xmlsoap.org/soap/envelope/"><e:Body><a/></e:Body></e:Envelope>"#;
        let resp = post(&rig, "/onvif/device_service", v11);
        assert!(text(&resp).contains("s:VersionMismatch"));
    }

    #[test]
    fn routing_and_methods() {
        let rig = rig();
        assert_eq!(post(&rig, "/nope", "").status, 404);
        let get = rig.service.handle(&ServiceRequest {
            method: "GET",
            target: "/onvif/device_service",
            authorization: None,
            body: b"",
            local: LOCAL.parse().unwrap(),
        });
        assert_eq!(get.status, 405);
    }

    fn snapshot_get(rig: &Rig, token: &str, authorization: Option<&str>) -> ServiceResponse {
        rig.service.handle(&ServiceRequest {
            method: "GET",
            target: &format!("{SNAPSHOT_PREFIX}{token}"),
            authorization,
            body: b"",
            local: LOCAL.parse().unwrap(),
        })
    }

    #[test]
    fn snapshot_endpoint_requires_digest() {
        let rig = rig();
        let unauth = snapshot_get(&rig, "mjpeg", None);
        assert_eq!(unauth.status, 401);
        let challenge = unauth.www_authenticate.unwrap();
        let target = format!("{SNAPSHOT_PREFIX}mjpeg");
        let auth =
            DigestAuth::client_response(USER, PASS, "GET", &target, &challenge, 1, "c").unwrap();
        let ok = snapshot_get(&rig, "mjpeg", Some(&auth));
        assert_eq!((ok.status, ok.content_type), (200, "image/jpeg"));
        assert_eq!(ok.body, [0xFF, 0xD8, 0xFF, 0xD9]);
        // Replaying the same Authorization (same nc) fails.
        assert_eq!(snapshot_get(&rig, "mjpeg", Some(&auth)).status, 401);
        // Unknown token and a source with no frame.
        let t2 = format!("{SNAPSHOT_PREFIX}main");
        let a2 = DigestAuth::client_response(USER, PASS, "GET", &t2, &challenge, 2, "c").unwrap();
        assert_eq!(snapshot_get(&rig, "main", Some(&a2)).status, 503);
        let t3 = format!("{SNAPSHOT_PREFIX}ghost");
        let a3 = DigestAuth::client_response(USER, PASS, "GET", &t3, &challenge, 3, "c").unwrap();
        assert_eq!(snapshot_get(&rig, "ghost", Some(&a3)).status, 404);
    }
}
