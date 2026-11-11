//! Property tests for the PTP container codec through the public API.

use proptest::prelude::*;
use tethercam::ptp::{Container, ContainerHeader, ContainerKind, HEADER_LEN};

fn kind() -> impl Strategy<Value = ContainerKind> {
    prop_oneof![
        Just(ContainerKind::Command),
        Just(ContainerKind::Data),
        Just(ContainerKind::Response),
        Just(ContainerKind::Event),
    ]
}

proptest! {
    #[test]
    fn encode_then_decode_round_trips(
        kind in kind(),
        code: u16,
        transaction_id: u32,
        payload in proptest::collection::vec(any::<u8>(), 0..2048),
        trailer in proptest::collection::vec(any::<u8>(), 0..32),
    ) {
        let original = Container { kind, code, transaction_id, payload: &payload };
        let mut wire = Vec::new();
        original.encode_into(&mut wire).unwrap();
        prop_assert_eq!(wire.len(), HEADER_LEN + payload.len());
        wire.extend_from_slice(&trailer);

        let (decoded, rest) = Container::decode(&wire).unwrap();
        prop_assert_eq!(decoded, original);
        prop_assert_eq!(rest, &trailer[..]);
    }

    #[test]
    fn decode_never_panics_on_arbitrary_bytes(bytes in proptest::collection::vec(any::<u8>(), 0..256)) {
        let _ = Container::decode(&bytes);
        let _ = ContainerHeader::decode(&bytes);
    }

    #[test]
    fn command_params_round_trip(
        code: u16,
        transaction_id: u32,
        params in proptest::collection::vec(any::<u32>(), 0..=5),
    ) {
        let wire = Container::command(code, transaction_id, &params).unwrap();
        let (decoded, rest) = Container::decode(&wire).unwrap();
        prop_assert!(rest.is_empty());
        prop_assert_eq!(decoded.kind, ContainerKind::Command);
        prop_assert_eq!(decoded.params().collect::<Vec<_>>(), params);
    }
}
