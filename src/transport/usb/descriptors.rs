//! Selection of PTP interfaces from USB configuration descriptor bytes.
//!
//! References:
//! - USB 2.0 specification, section 9.6 (standard descriptors)
//! - USB Still Image Capture Device Definition 1.0, section 5.1 (endpoints)

use thiserror::Error;

/// Endpoint addresses and packet size for one still-image interface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Endpoints {
    /// Interface number.
    pub interface: u8,
    /// Alternate setting.
    pub alternate: u8,
    /// Bulk input address.
    pub bulk_in: u8,
    /// Bulk output address.
    pub bulk_out: u8,
    /// Interrupt input address.
    pub interrupt_in: u8,
    /// Bulk input maximum packet size.
    pub max_packet_size: u16,
}

/// A configuration descriptor is malformed or has no usable PTP interface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum DescriptorError {
    /// A descriptor has an invalid length or is truncated.
    #[error("invalid USB descriptor length")]
    Malformed,
    /// No still-image interface has all required endpoints.
    #[error("no usable still-image interface")]
    NoInterface,
}

const INTERFACE: u8 = 4;
const ENDPOINT: u8 = 5;

#[derive(Default)]
struct Candidate {
    interface: u8,
    alternate: u8,
    bulk_in: Option<(u8, u16)>,
    bulk_out: Option<u8>,
    interrupt_in: Option<u8>,
}

impl Candidate {
    fn finish(&self) -> Option<Endpoints> {
        let (bulk_in, max_packet_size) = self.bulk_in?;
        Some(Endpoints {
            interface: self.interface,
            alternate: self.alternate,
            bulk_in,
            bulk_out: self.bulk_out?,
            interrupt_in: self.interrupt_in?,
            max_packet_size,
        })
    }
}

/// Select the first complete still-image interface in a raw configuration descriptor.
///
/// Walks the descriptors of one configuration: an interface descriptor with class, subclass
/// and protocol `6/1/1` starts a candidate, and the bulk-in, bulk-out and interrupt-in
/// endpoint descriptors that follow it complete it. The first complete candidate wins, so the
/// lowest alternate setting that has all three endpoints is used.
///
/// # Errors
///
/// [`DescriptorError::Malformed`] for a descriptor shorter than 2 bytes, longer than what
/// remains, or an interface or endpoint descriptor too short for its fields, and
/// [`DescriptorError::NoInterface`] when no still-image interface has all three endpoints.
/// Total: never panics, whatever the bytes.
pub fn select(bytes: &[u8]) -> Result<Endpoints, DescriptorError> {
    let mut rest = bytes;
    let mut candidate: Option<Candidate> = None;
    while let Some(&length) = rest.first() {
        let length = usize::from(length);
        let (descriptor, remaining) = rest
            .split_at_checked(length)
            .filter(|_| length >= 2)
            .ok_or(DescriptorError::Malformed)?;
        rest = remaining;
        match *descriptor {
            [
                _,
                INTERFACE,
                interface,
                alternate,
                _,
                class,
                subclass,
                protocol,
                _,
                ..,
            ] => {
                if let Some(done) = candidate.as_ref().and_then(Candidate::finish) {
                    return Ok(done);
                }
                candidate = ((class, subclass, protocol) == (6, 1, 1)).then_some(Candidate {
                    interface,
                    alternate,
                    ..Candidate::default()
                });
            }
            [_, ENDPOINT, address, attributes, low, high, _, ..] => {
                if let Some(current) = candidate.as_mut() {
                    let size = u16::from_le_bytes([low, high]) & 0x07ff;
                    match (attributes & 3, address & 0x80 != 0, size) {
                        (2, true, 1..) => current.bulk_in = Some((address, size)),
                        (2, false, 1..) => current.bulk_out = Some(address),
                        (3, true, 1..) => current.interrupt_in = Some(address),
                        _ => {}
                    }
                }
            }
            [_, INTERFACE | ENDPOINT, ..] => return Err(DescriptorError::Malformed),
            _ => {}
        }
    }
    candidate
        .as_ref()
        .and_then(Candidate::finish)
        .ok_or(DescriptorError::NoInterface)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    /// Configuration header (9), then a still-image interface 0 with bulk-in `0x81`,
    /// bulk-out `0x02` (both 512 bytes) and interrupt-in `0x83`, shaped like a camera in
    /// PC Remote mode.
    const CONFIG: &[u8] = &[
        9, 2, 39, 0, 1, 1, 0, 0xc0, 1, // configuration
        9, 4, 0, 0, 3, 6, 1, 1, 0, // interface
        7, 5, 0x81, 2, 0, 2, 0, // bulk in
        7, 5, 0x02, 2, 0, 2, 0, // bulk out
        7, 5, 0x83, 3, 8, 0, 6, // interrupt in
    ];

    fn interface(number: u8, alternate: u8, class: (u8, u8, u8)) -> Vec<u8> {
        vec![9, 4, number, alternate, 3, class.0, class.1, class.2, 0]
    }

    fn endpoint(address: u8, attributes: u8, size: u16) -> Vec<u8> {
        let [low, high] = size.to_le_bytes();
        vec![7, 5, address, attributes, low, high, 0]
    }

    fn still(number: u8, alternate: u8) -> Vec<u8> {
        interface(number, alternate, (6, 1, 1))
    }

    fn join(parts: &[Vec<u8>]) -> Vec<u8> {
        parts.concat()
    }

    #[test]
    fn selects_endpoints_from_captured_bytes() {
        assert_eq!(
            select(CONFIG),
            Ok(Endpoints {
                interface: 0,
                alternate: 0,
                bulk_in: 0x81,
                bulk_out: 2,
                interrupt_in: 0x83,
                max_packet_size: 512
            })
        );
    }

    #[test]
    fn usb3_packet_size_and_extra_descriptors_are_handled() {
        let bytes = join(&[
            still(1, 0),
            vec![6, 0x24, 1, 2, 3, 4], // class-specific descriptor is skipped
            endpoint(0x82, 2, 1024),
            endpoint(0x01, 2, 1024),
            endpoint(0x84, 3, 8),
        ]);
        let endpoints = select(&bytes).expect("select");
        assert_eq!((endpoints.interface, endpoints.max_packet_size), (1, 1024));
        assert_eq!(
            (
                endpoints.bulk_in,
                endpoints.bulk_out,
                endpoints.interrupt_in
            ),
            (0x82, 1, 0x84)
        );
    }

    #[test]
    fn ignores_other_classes_and_picks_the_still_image_interface() {
        let bytes = join(&[
            interface(0, 0, (8, 6, 80)),
            endpoint(0x81, 2, 512),
            endpoint(0x02, 2, 512),
            endpoint(0x83, 3, 8),
            still(3, 0),
            endpoint(0x85, 2, 512),
            endpoint(0x06, 2, 512),
            endpoint(0x87, 3, 8),
        ]);
        assert_eq!(select(&bytes).expect("select").interface, 3);
    }

    #[test]
    fn endpoints_outside_a_still_image_interface_do_not_count() {
        let bytes = join(&[
            endpoint(0x81, 2, 512),
            interface(0, 0, (8, 6, 80)),
            endpoint(0x02, 2, 512),
        ]);
        assert_eq!(select(&bytes), Err(DescriptorError::NoInterface));
    }

    #[test]
    fn first_complete_alternate_wins_over_an_incomplete_one() {
        let bytes = join(&[
            still(0, 0),
            endpoint(0x81, 2, 512),
            still(0, 1),
            endpoint(0x81, 2, 512),
            endpoint(0x02, 2, 512),
            endpoint(0x83, 3, 8),
            still(0, 2),
            endpoint(0x85, 2, 512),
            endpoint(0x06, 2, 512),
            endpoint(0x87, 3, 8),
        ]);
        assert_eq!(select(&bytes).expect("select").alternate, 1);
    }

    #[test]
    fn missing_any_endpoint_is_no_interface() {
        let bulk_in = endpoint(0x81, 2, 512);
        let bulk_out = endpoint(0x02, 2, 512);
        let interrupt = endpoint(0x83, 3, 8);
        for parts in [
            vec![still(0, 0), bulk_out.clone(), interrupt.clone()],
            vec![still(0, 0), bulk_in.clone(), interrupt.clone()],
            vec![still(0, 0), bulk_in.clone(), bulk_out.clone()],
            vec![still(0, 0)],
            vec![],
        ] {
            assert_eq!(select(&join(&parts)), Err(DescriptorError::NoInterface));
        }
    }

    #[test]
    fn unusable_endpoints_are_skipped() {
        let bytes = join(&[
            still(0, 0),
            endpoint(0x81, 2, 0),   // zero-size bulk in
            endpoint(0x01, 1, 512), // isochronous
            endpoint(0x82, 3, 0),   // zero-size interrupt
            endpoint(0x03, 3, 8),   // interrupt OUT
        ]);
        assert_eq!(select(&bytes), Err(DescriptorError::NoInterface));
    }

    #[test]
    fn rejects_malformed_descriptors() {
        assert_eq!(select(&[0]), Err(DescriptorError::Malformed));
        assert_eq!(select(&[1]), Err(DescriptorError::Malformed));
        assert_eq!(select(&[3, 4]), Err(DescriptorError::Malformed));
        assert_eq!(
            select(&CONFIG[..CONFIG.len() - 1]),
            Err(DescriptorError::Malformed)
        );
        assert_eq!(select(&[2, 4]), Err(DescriptorError::Malformed));
        assert_eq!(select(&[2, 5]), Err(DescriptorError::Malformed));
        assert_eq!(
            select(&[8, 4, 0, 0, 0, 6, 1, 1]),
            Err(DescriptorError::Malformed)
        );
        assert_eq!(
            select(&[6, 5, 0x81, 2, 0, 2]),
            Err(DescriptorError::Malformed)
        );
    }

    proptest! {
        #[test]
        fn arbitrary_bytes_never_panic(data in proptest::collection::vec(any::<u8>(), 0..512)) {
            let _ = select(&data);
        }

        #[test]
        fn structured_descriptors_never_panic(
            parts in proptest::collection::vec(
                (2_u8..8, prop_oneof![Just(4_u8), Just(5_u8), any::<u8>()],
                 proptest::collection::vec(any::<u8>(), 0..8)),
                0..16,
            )
        ) {
            let mut bytes = Vec::new();
            for (length, kind, body) in parts {
                bytes.push(length);
                bytes.push(kind);
                bytes.extend(body);
            }
            let _ = select(&bytes);
        }
    }
}
