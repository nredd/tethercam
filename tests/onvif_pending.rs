//! ONVIF service before its model exists (no camera yet), then after publication.
//!
//! The Device and Media services answer `503` until the camera defines the profiles; the
//! authenticated status endpoint answers throughout.

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::sync::Arc;

    use tethercam::onvif::auth::Credentials;
    use tethercam::onvif::auth::digest::DigestAuth;
    use tethercam::onvif::media::{SNAPSHOT_PREFIX, STATUS_PATH};
    use tethercam::onvif::model::example_model;
    use tethercam::onvif::service::{
        ModelPublisher, OnvifService, ServiceRequest, ServiceResponse, SnapshotError,
        SnapshotSource, StatusSource,
    };
    use tethercam::onvif::time::ManualClock;

    const USER: &str = "operator";
    const PASS: &str = "synthetic-pass";
    const LOCAL: &str = "192.0.2.10:8080";

    #[derive(Debug)]
    struct Snapshots;

    impl SnapshotSource for Snapshots {
        fn snapshot(&self, _token: &str) -> Result<Vec<u8>, SnapshotError> {
            Ok(vec![0xFF, 0xD8, 0xFF, 0xD9])
        }
    }

    #[derive(Debug)]
    struct Status;

    impl StatusSource for Status {
        fn status_json(&self) -> String {
            r#"{"camera":"waiting"}"#.into()
        }
    }

    fn pending() -> (OnvifService, ModelPublisher) {
        let (service, publisher) = OnvifService::pending(
            Credentials::new(USER, PASS).unwrap(),
            Arc::new(ManualClock::new(1_750_000_000)),
        )
        .unwrap();
        (
            service
                .with_snapshot_source(Arc::new(Snapshots))
                .with_status_source(Arc::new(Status)),
            publisher,
        )
    }

    fn request<'a>(method: &'a str, target: &'a str, auth: Option<&'a str>) -> ServiceRequest<'a> {
        ServiceRequest {
            method,
            target,
            authorization: auth,
            body: b"",
            local: LOCAL.parse().unwrap(),
        }
    }

    /// An authenticated `GET`, following the Digest challenge like a client.
    fn get(service: &OnvifService, target: &str, nc: u32) -> ServiceResponse {
        let challenge = service
            .handle(&request("GET", target, None))
            .www_authenticate
            .unwrap();
        let auth =
            DigestAuth::client_response(USER, PASS, "GET", target, &challenge, nc, "c").unwrap();
        service.handle(&request("GET", target, Some(&auth)))
    }

    fn soap(service: &OnvifService, path: &str, operation: &str) -> ServiceResponse {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/onvif");
        let template = fs::read_to_string(dir.join(format!("{operation}.request.xml"))).unwrap();
        let body = template.replace("{{SECURITY}}", "");
        service.handle(&ServiceRequest {
            body: body.as_bytes(),
            ..request("POST", path, None)
        })
    }

    #[test]
    fn soap_services_answer_503_before_authentication_until_published() {
        let (service, publisher) = pending();
        for (path, operation) in [
            ("/onvif/device_service", "device_get_device_information"),
            ("/onvif/media_service", "media_get_profiles"),
        ] {
            let response = soap(&service, path, operation);
            assert_eq!(response.status, 503, "{path}");
            assert_eq!(response.body, b"camera not ready");
            assert!(response.www_authenticate.is_none());
        }
        publisher.publish(example_model());
        // Published: the same service now routes, which for a request without credentials means
        // a Digest challenge rather than a 503.
        let response = soap(&service, "/onvif/media_service", "media_get_profiles");
        assert_eq!(response.status, 401);
    }

    #[test]
    fn snapshot_authenticates_then_reports_not_ready_until_published() {
        let (service, publisher) = pending();
        let target = format!("{SNAPSHOT_PREFIX}mjpeg");
        assert_eq!(service.handle(&request("GET", &target, None)).status, 401);
        let not_ready = get(&service, &target, 1);
        assert_eq!(not_ready.status, 503);
        assert_eq!(not_ready.body, b"camera not ready");
        publisher.publish(example_model());
        assert_eq!(get(&service, &target, 2).status, 200);
    }

    #[test]
    fn status_endpoint_is_authenticated_get_only_and_optional() {
        let (service, _publisher) = pending();
        assert_eq!(
            service.handle(&request("GET", STATUS_PATH, None)).status,
            401
        );
        let ok = get(&service, STATUS_PATH, 1);
        assert_eq!((ok.status, ok.content_type), (200, "application/json"));
        assert_eq!(ok.body, br#"{"camera":"waiting"}"#);
        assert_eq!(
            service.handle(&request("POST", STATUS_PATH, None)).status,
            405
        );
        let (bare, _publisher) = OnvifService::pending(
            Credentials::new(USER, PASS).unwrap(),
            Arc::new(ManualClock::new(1_750_000_000)),
        )
        .unwrap();
        assert_eq!(bare.handle(&request("GET", STATUS_PATH, None)).status, 404);
    }
}
