//! Benchmark: authenticate a WS-Security request and answer `GetProfiles`.
//!
//! Includes building the client token (SHA-1 plus base64) so the number is the full
//! request cost seen by the service, and a fresh nonce per iteration so the replay cache
//! does real work. The clock advances one second per iteration, which keeps the cache
//! bounded by the skew window.

use std::hint::black_box;
use std::net::SocketAddr;
use std::sync::Arc;

use criterion::{Criterion, criterion_group, criterion_main};
use tethercam::onvif::auth::Credentials;
use tethercam::onvif::auth::wsse::UsernameToken;
use tethercam::onvif::model::example_model;
use tethercam::onvif::service::{OnvifService, ServiceRequest};
use tethercam::onvif::time::{ManualClock, format_datetime};

const START: i64 = 1_750_000_000;

fn authenticate_and_get_profiles(c: &mut Criterion) {
    let clock = Arc::new(ManualClock::new(START));
    let Ok(credentials) = Credentials::new("operator", "synthetic-pass") else {
        return;
    };
    let service =
        OnvifService::with_digest_secret(example_model(), credentials, clock.clone(), [1; 32]);
    let local: SocketAddr = SocketAddr::from(([192, 0, 2, 10], 8080));
    let mut counter: u64 = 0;

    c.bench_function("onvif_auth_get_profiles", |b| {
        b.iter(|| {
            counter += 1;
            clock.advance(1);
            let now = START + i64::try_from(counter).unwrap_or(0);
            let nonce = format!("nonce-{counter:016}");
            let header =
                UsernameToken::digest_for("operator", "synthetic-pass", nonce.as_bytes(), &format_datetime(now))
                    .to_header_xml();
            let body = format!(
                r#"<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope"><s:Header>{header}</s:Header><s:Body><trt:GetProfiles xmlns:trt="http://www.onvif.org/ver10/media/wsdl"/></s:Body></s:Envelope>"#
            );
            let resp = service.handle(&ServiceRequest {
                method: "POST",
                target: "/onvif/media_service",
                authorization: None,
                body: body.as_bytes(),
                local,
            });
            assert_eq!(resp.status, 200);
            black_box(resp)
        });
    });
}

criterion_group!(benches, authenticate_and_get_profiles);
criterion_main!(benches);
