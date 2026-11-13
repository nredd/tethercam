//! Device personalities are independent of USB/IP framing and of one another.
//!
//! References:
//! - <https://www.usb.org/document-library/still-image-capture-device-definition-10-and-errata-16-mar-2007>
use super::*;

#[tokio::test(start_paused = true)]
async fn public_errors_preserve_domain_variants_sources_and_payload_free_categories() {
    use std::error::Error as _;
    use std::io;
    let deadline = tokio::time::timeout(Duration::ZERO, std::future::pending::<()>())
        .await
        .expect_err("deadline");
    let task = tokio::spawn(std::future::pending::<()>());
    task.abort();
    let cancelled = task.await.expect_err("cancelled task");
    for (error, kind, source) in [
        (Error::Protocol, io::ErrorKind::InvalidData, false),
        (Error::Identity, io::ErrorKind::InvalidInput, false),
        (
            io::Error::from(io::ErrorKind::BrokenPipe).into(),
            io::ErrorKind::BrokenPipe,
            true,
        ),
        (
            TransportError::Disconnected.into(),
            io::ErrorKind::Other,
            true,
        ),
        (deadline.into(), io::ErrorKind::TimedOut, true),
        (cancelled.into(), io::ErrorKind::Other, true),
    ] {
        assert_eq!(error.kind(), kind);
        assert_eq!(error.source().is_some(), source);
        assert_ne!(error.to_string(), "");
        assert_ne!(format!("{error:?}"), "");
    }
    assert_eq!(
        wire::Urb::parse(&[]).expect_err("protocol").kind(),
        io::ErrorKind::InvalidData
    );
    let mut invalid = Synthetic::default().identity();
    invalid.device.clear();
    assert_eq!(
        invalid.validate().expect_err("identity").kind(),
        io::ErrorKind::InvalidInput
    );
}

#[tokio::test]
async fn synthetic_device_uses_non_sony_endpoints_and_stalls_unknown_requests() {
    let mut device = Synthetic::default();
    assert_eq!(device.identity().vendor, 0xffff);
    let mut bytes = [0; 8];
    assert_eq!(
        device.transfer(5, 1, &[], &mut bytes).await.err(),
        Some(TransportError::Timeout)
    );
    assert_eq!(
        device
            .transfer(4, 0, &[1, 2, 3], &mut bytes)
            .await
            .expect("out"),
        3
    );
    assert_eq!(
        device
            .transfer(5, 1, &[], &mut bytes[..2])
            .await
            .expect("in"),
        2
    );
    assert_eq!(&bytes[..2], &[1, 2]);
    assert_eq!(
        device
            .transfer(5, 1, &[], &mut bytes)
            .await
            .expect("remainder"),
        1
    );
    assert_eq!(
        device.transfer(1, 1, &[], &mut bytes).await.err(),
        Some(TransportError::Stall)
    );
    assert!(
        device
            .control([0; 8], &[])
            .await
            .expect("control")
            .data
            .is_none()
    );
}

#[tokio::test]
async fn sony_standard_and_class_controls_include_cancellation_status_and_faults() {
    let mut device = Sony::default();
    let identity = device.identity();
    assert_eq!(identity.vendor, 0x054c);
    let mut bytes = [0; 16];
    assert_eq!(
        device.transfer(7, 0, &[], &mut bytes).await.err(),
        Some(TransportError::Stall)
    );
    device
        .transfer(2, 0, &[12], &mut bytes)
        .await
        .expect("partial command");
    let cancelled = device
        .control([0x21, 0x64, 0, 0, 0, 0, 6, 0], &[1, 0x40, 1, 0, 0, 0])
        .await
        .expect("cancel");
    assert!(cancelled.flush_bulk);
    assert_eq!(
        device
            .control([0xa1, 0x67, 0, 0, 0, 0, 4, 0], &[])
            .await
            .expect("status")
            .data,
        Some(vec![4, 0, 0x19, 0x20])
    );
    assert_eq!(
        device
            .control([0xa1, 0x67, 0, 0, 0, 0, 4, 0], &[])
            .await
            .expect("status")
            .data,
        Some(vec![4, 0, 1, 0x20])
    );
    assert!(
        !device
            .control([0x21, 0x64, 0, 0, 0, 0, 6, 0], &[1, 0x40, 0, 0, 0, 0])
            .await
            .expect("idle cancel")
            .flush_bulk
    );
    for payload in [&[][..], &[0, 0, 0, 0, 0, 0][..]] {
        assert!(
            device
                .control([0x21, 0x64, 0, 0, 0, 0, 6, 0], payload)
                .await
                .expect("invalid cancellation")
                .data
                .is_none()
        );
    }
    for endpoint in [0x81, 2, 0x83, 9] {
        assert_eq!(
            device
                .control([2, 1, 0, 0, endpoint, 0, 0, 0], &[])
                .await
                .expect("clear halt")
                .data
                .is_some(),
            endpoint != 9
        );
    }
    assert!(
        device
            .control([0x21, 0x66, 0, 0, 0, 0, 0, 0], &[])
            .await
            .expect("reset")
            .flush_bulk
    );
    device.class_stall = true;
    assert!(
        device
            .control([0x21, 0x66, 0, 0, 0, 0, 0, 0], &[])
            .await
            .expect("stall")
            .data
            .is_none()
    );
}

#[tokio::test]
async fn malformed_class_controls_stall_and_native_endpoints_are_distinct() {
    let mut device = Sony::default();
    let mut bytes = [0; 16];
    for setup in [
        [0x21, 0x64, 1, 0, 0, 0, 6, 0],
        [0x21, 0x64, 0, 1, 0, 0, 6, 0],
        [0x21, 0x64, 0, 0, 1, 0, 6, 0],
        [2, 1, 1, 0, 0x81, 0, 0, 0],
        [2, 1, 0, 1, 0x81, 0, 0, 0],
        [2, 1, 0, 0, 0x81, 1, 0, 0],
        [2, 1, 0, 0, 0x81, 0, 1, 0],
        [0x21, 0x66, 0, 0, 1, 0, 0, 0],
        [0x21, 0x66, 0, 0, 0, 0, 1, 0],
        [0xa1, 0x67, 0, 0, 1, 0, 4, 0],
    ] {
        assert!(
            device
                .control(setup, &[])
                .await
                .expect("invalid")
                .data
                .is_none()
        );
    }
    device.camera.push_raw_bulk_in(vec![1]);
    assert_eq!(
        device.transfer(1, 1, &[], &mut bytes).await.expect("bulk"),
        1
    );
    device.camera.push_raw_interrupt(vec![2]);
    assert_eq!(
        device
            .transfer(3, 1, &[], &mut bytes)
            .await
            .expect("interrupt"),
        1
    );
}

#[test]
fn descriptor_validation_bounds_identity_and_string_encoding() {
    let good = Synthetic::default().identity();
    good.validate().expect("valid");
    for mutation in 0..15 {
        let mut value = good.clone();
        match mutation {
            0 => value.device.clear(),
            1 => value.device[0] = 0,
            2 => value.vendor = 1,
            3 => value.product = 2,
            4 => value.configuration.resize(4097, 0),
            5 => value.configuration[0] = 0,
            6 => value.configuration[2] = 0,
            7 => value.strings[0].clear(),
            8 => value.strings[1] = "a".repeat(127),
            9 => value.strings[2] = "bad\n".into(),
            10 => value.bulk_endpoints.clear(),
            11 => value.bulk_endpoints = vec![0],
            12 => value.bulk_endpoints = vec![16],
            13 => value.alternates.clear(),
            _ => value.alternates = vec![1],
        }
        assert!(value.validate().is_err(), "mutation {mutation}");
    }
    assert!(good.descriptor(1, 0).is_some());
    assert!(good.descriptor(2, 0).is_some());
    assert_eq!(good.descriptor(3, 0), Some(vec![4, 3, 9, 4]));
    for index in 1..=3 {
        assert!(good.descriptor(3, index).is_some());
    }
    assert!(good.descriptor(99, 0).is_none());
    let mut invalid = good;
    invalid.strings[0] = "a".repeat(127);
    assert!(invalid.descriptor(3, 1).is_none());
}

proptest::proptest! {
    #[test]
    fn descriptors_and_class_control_parsers_are_total(kind: u8, index: u8, text in ".*", setup: [u8; 8], payload in proptest::collection::vec(proptest::num::u8::ANY, 0..64)) {
        let mut identity = Synthetic::default().identity();
        identity.strings[0] = text;
        let _ = identity.descriptor(kind, index);
        let _ = identity.validate();
        let runtime = tokio::runtime::Builder::new_current_thread().build().expect("runtime");
        let _ = runtime.block_on(Sony::default().control(setup, &payload));
    }
}

#[tokio::test]
async fn disconnected_sony_class_recovery_returns_typed_io_errors() {
    let mut device = Sony::default();
    device.camera.inject(
        Endpoint::BulkOut,
        0,
        crate::transport::sim::Fault::Disconnect,
    );
    assert!(device.transfer(2, 0, &[], &mut []).await.is_err());
    assert!(
        device
            .control([2, 1, 0, 0, 0x81, 0, 0, 0], &[])
            .await
            .is_err()
    );
    assert!(
        device
            .control([0x21, 0x66, 0, 0, 0, 0, 0, 0], &[])
            .await
            .is_err()
    );
}

#[test]
#[cfg(target_pointer_width = "64")]
fn reply_rejects_unrepresentable_public_lengths_without_allocating_payload() {
    let request = wire::Urb {
        command: 2,
        seq: 1,
        direction: 0,
        endpoint: 0,
        length: usize::MAX,
        unlink: 0,
        setup: [0; 8],
    };
    assert!(request.reply(0, &[], usize::MAX).is_err());
}

#[test]
fn codec_boundary_cases_are_deterministic_in_the_source_suite() {
    assert!(wire::word(&[0; 3], 0).is_err());
    assert!(wire::word(&[], usize::MAX).is_err());
    for (command, direction, iso) in [(1_u32, 1, u32::MAX), (1, 0, 0), (2, 1, 0)] {
        let mut header = Vec::new();
        for value in [
            command,
            1,
            0x0001_0001,
            direction,
            1,
            0,
            16,
            0,
            iso,
            0,
            0,
            0,
        ] {
            header.extend_from_slice(&value.to_be_bytes());
        }
        let request = wire::Urb::parse(&header).expect("header");
        request.reply(0, &[], 0).expect("empty");
        if command == 1 && direction == 1 {
            request.reply(0, &[9], 1).expect("payload");
            assert!(request.reply(0, &[9], 0).is_err());
        }
        assert!(request.reply(0, &[], 17).is_err());
    }
    assert!(wire::Urb::parse(&[]).is_err());
}
