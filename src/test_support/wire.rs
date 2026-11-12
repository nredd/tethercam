//! Bounded USB/IP wire codec and synthetic USB descriptors.
//!
//! References:
//! - <https://docs.kernel.org/usb/usbip_protocol.html>
//! - <https://www.usb.org/document-library/usb-20-specification>

use std::io;

/// Exported synthetic bus identifier.
pub const BUS_ID: &str = "1-1";
/// Maximum admitted bytes per transfer.
pub const MAX_TRANSFER: usize = 1024 * 1024;
/// Sony fixture device descriptor.
pub const DEVICE: [u8; 18] = [
    18, 1, 0, 2, 0, 0, 0, 64, 0x4c, 5, 0x34, 0x0c, 0, 1, 1, 2, 3, 1,
];
/// Two equivalent alternate settings exercise `SET_INTERFACE` through the real backend.
pub const CONFIG: [u8; 69] = [
    9, 2, 69, 0, 1, 1, 0, 0x80, 50, 9, 4, 0, 0, 3, 6, 1, 1, 0, 7, 5, 0x81, 2, 0, 2, 0, 7, 5, 0x02,
    2, 0, 2, 0, 7, 5, 0x83, 3, 64, 0, 8, 9, 4, 0, 1, 3, 6, 1, 1, 0, 7, 5, 0x81, 2, 0, 2, 0, 7, 5,
    0x02, 2, 0, 2, 0, 7, 5, 0x83, 3, 64, 0, 8,
];

/// Invalid or unsupported protocol input.
#[must_use]
pub fn invalid() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "invalid or unsupported USB/IP request",
    )
}

/// Read one big-endian network word without indexing.
///
/// # Errors
/// Returns an invalid-data error for short input or offset overflow.
pub fn word(bytes: &[u8], offset: usize) -> io::Result<u32> {
    let part = bytes
        .get(offset..)
        .and_then(<[u8]>::first_chunk::<4>)
        .ok_or_else(invalid)?;
    Ok(u32::from_be_bytes(*part))
}

/// A non-isochronous URB. All lengths are validated before allocating payload memory.
#[derive(Debug, Clone)]
pub struct Urb {
    /// USB/IP command field.
    pub command: u32,
    /// USB/IP seq field.
    pub seq: u32,
    /// USB/IP direction field.
    pub direction: u32,
    /// USB/IP endpoint field.
    pub endpoint: u32,
    /// USB/IP length field.
    pub length: usize,
    /// USB/IP unlink field.
    pub unlink: u32,
    /// USB/IP setup field.
    pub setup: [u8; 8],
}

impl Urb {
    /// Parse and bound a non-isochronous submit/unlink header.
    ///
    /// # Errors
    /// Rejects short headers, invalid fields and oversized transfers.
    pub fn parse(bytes: &[u8]) -> io::Result<Self> {
        let header: [u8; 48] = bytes.try_into().map_err(|_| invalid())?;
        let [
            c0,
            c1,
            c2,
            c3,
            s0,
            s1,
            s2,
            s3,
            d0,
            d1,
            d2,
            d3,
            r0,
            r1,
            r2,
            r3,
            e0,
            e1,
            e2,
            e3,
            u0,
            u1,
            u2,
            u3,
            l0,
            l1,
            l2,
            l3,
            _,
            _,
            _,
            _,
            i0,
            i1,
            i2,
            i3,
            _,
            _,
            _,
            _,
            setup0,
            setup1,
            setup2,
            setup3,
            setup4,
            setup5,
            setup6,
            setup7,
        ] = header;
        let command = u32::from_be_bytes([c0, c1, c2, c3]);
        let direction = u32::from_be_bytes([r0, r1, r2, r3]);
        let endpoint = u32::from_be_bytes([e0, e1, e2, e3]);
        let length = u32::from_be_bytes([l0, l1, l2, l3]) as usize;
        let iso = u32::from_be_bytes([i0, i1, i2, i3]);
        if u32::from_be_bytes([d0, d1, d2, d3]) != 0x0001_0001
            || !matches!(command, 1 | 2)
            || direction > 1
            || endpoint > 15
            || (command == 1 && (length > MAX_TRANSFER || !matches!(iso, 0 | u32::MAX)))
        {
            return Err(invalid());
        }
        Ok(Self {
            command,
            seq: u32::from_be_bytes([s0, s1, s2, s3]),
            direction,
            endpoint,
            length: if command == 1 { length } else { 0 },
            unlink: u32::from_be_bytes([u0, u1, u2, u3]),
            setup: [
                setup0, setup1, setup2, setup3, setup4, setup5, setup6, setup7,
            ],
        })
    }

    /// Encode a response with consistent lengths.
    ///
    /// # Errors
    /// Rejects payload lengths inconsistent with the request.
    pub fn reply(&self, status: i32, data: &[u8], actual: usize) -> io::Result<Vec<u8>> {
        if actual > self.length
            || (self.command == 1 && self.direction == 1 && actual != data.len())
        {
            return Err(invalid());
        }
        let length = u32::try_from(actual).map_err(|_| invalid())?;
        Ok(self.encoded(status, data, length))
    }

    /// Encode a status-only response with no payload or fallible length conversion.
    #[must_use]
    pub fn status(&self, status: i32) -> Vec<u8> {
        self.encoded(status, &[], 0)
    }

    fn encoded(&self, status: i32, data: &[u8], length: u32) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(48 + data.len());
        for value in [if self.command == 1 { 3 } else { 4 }, self.seq, 0, 0, 0] {
            bytes.extend_from_slice(&value.to_be_bytes());
        }
        bytes.extend_from_slice(&status.to_be_bytes());
        bytes.extend_from_slice(&length.to_be_bytes());
        bytes.resize(48, 0);
        if self.direction == 1 && self.command == 1 {
            bytes.extend_from_slice(data);
        }
        bytes
    }
}

/// Encode a USB/IP management operation header.
#[must_use]
pub fn operation(code: u16, status: u32) -> Vec<u8> {
    [
        0x0111_u16.to_be_bytes().as_slice(),
        code.to_be_bytes().as_slice(),
        status.to_be_bytes().as_slice(),
    ]
    .concat()
}

/// Encode a device export record.
#[must_use]
pub fn exported(identity: &super::Identity) -> Vec<u8> {
    let mut bytes = vec![0; 256];
    let path = b"/synthetic/usb/1-1";
    for (slot, byte) in bytes.iter_mut().zip(path) {
        *slot = *byte;
    }
    let mut bus = [0; 32];
    for (slot, byte) in bus.iter_mut().zip(BUS_ID.bytes()) {
        *slot = byte;
    }
    bytes.extend_from_slice(&bus);
    for value in [1_u32, 1, 3] {
        bytes.extend_from_slice(&value.to_be_bytes());
    }
    bytes.extend_from_slice(&identity.vendor.to_be_bytes());
    bytes.extend_from_slice(&identity.product.to_be_bytes());
    bytes.extend_from_slice(&[1, 0, 0, 0, 0, 1, 1, 1]);
    bytes
}
