//! Bounded USB/IP wire codec and synthetic USB descriptors.
//!
//! References:
//! - <https://docs.kernel.org/usb/usbip_protocol.html>
//! - <https://www.usb.org/document-library/usb-20-specification>

use std::io;

pub(crate) const BUS_ID: &str = "1-1";
pub(crate) const MAX_TRANSFER: usize = 1024 * 1024;
pub(crate) const DEVICE: [u8; 18] = [
    18, 1, 0, 2, 0, 0, 0, 64, 0x4c, 5, 0x34, 0x0c, 0, 1, 1, 2, 3, 1,
];
// Two equivalent alternate settings exercise SET_INTERFACE through the real backend.
pub(crate) const CONFIG: [u8; 69] = [
    9, 2, 69, 0, 1, 1, 0, 0x80, 50, 9, 4, 0, 0, 3, 6, 1, 1, 0, 7, 5, 0x81, 2, 0, 2, 0, 7, 5, 0x02,
    2, 0, 2, 0, 7, 5, 0x83, 3, 64, 0, 8, 9, 4, 0, 1, 3, 6, 1, 1, 0, 7, 5, 0x81, 2, 0, 2, 0, 7, 5,
    0x02, 2, 0, 2, 0, 7, 5, 0x83, 3, 64, 0, 8,
];

pub(crate) fn invalid() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "invalid or unsupported USB/IP request",
    )
}

pub(crate) fn word(bytes: &[u8], offset: usize) -> io::Result<u32> {
    let end = offset.checked_add(4).ok_or_else(invalid)?;
    let part: [u8; 4] = bytes
        .get(offset..end)
        .ok_or_else(invalid)?
        .try_into()
        .map_err(|_| invalid())?;
    Ok(u32::from_be_bytes(part))
}

/// A non-isochronous URB. All lengths are validated before allocating payload memory.
#[derive(Debug, Clone)]
pub(crate) struct Urb {
    pub command: u32,
    pub seq: u32,
    pub direction: u32,
    pub endpoint: u32,
    pub length: usize,
    pub unlink: u32,
    pub setup: [u8; 8],
}

impl Urb {
    pub(crate) fn parse(bytes: &[u8]) -> io::Result<Self> {
        if bytes.len() != 48 {
            return Err(invalid());
        }
        let command = word(bytes, 0)?;
        let direction = word(bytes, 12)?;
        let endpoint = word(bytes, 16)?;
        let length = usize::try_from(word(bytes, 24)?).map_err(|_| invalid())?;
        let iso = word(bytes, 32)?;
        if word(bytes, 8)? != 0x0001_0001
            || !matches!(command, 1 | 2)
            || direction > 1
            || endpoint > 15
            || (command == 1 && (length > MAX_TRANSFER || !matches!(iso, 0 | u32::MAX)))
        {
            return Err(invalid());
        }
        Ok(Self {
            command,
            seq: word(bytes, 4)?,
            direction,
            endpoint,
            length: if command == 1 { length } else { 0 },
            unlink: word(bytes, 20)?,
            setup: bytes
                .get(40..48)
                .ok_or_else(invalid)?
                .try_into()
                .map_err(|_| invalid())?,
        })
    }

    pub(crate) fn reply(&self, status: i32, data: &[u8], actual: usize) -> io::Result<Vec<u8>> {
        if actual > self.length
            || (self.command == 1 && self.direction == 1 && actual != data.len())
        {
            return Err(invalid());
        }
        let length = u32::try_from(actual).map_err(|_| invalid())?;
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
        Ok(bytes)
    }
}

pub(crate) fn operation(code: u16, status: u32) -> Vec<u8> {
    [
        0x0111_u16.to_be_bytes().as_slice(),
        code.to_be_bytes().as_slice(),
        status.to_be_bytes().as_slice(),
    ]
    .concat()
}

pub(crate) fn exported(product: u16) -> Vec<u8> {
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
    bytes.extend_from_slice(&0x054c_u16.to_be_bytes());
    bytes.extend_from_slice(&product.to_be_bytes());
    bytes.extend_from_slice(&[1, 0, 0, 0, 0, 1, 1, 1]);
    bytes
}

pub(crate) fn descriptor(kind: u8, index: u8, product: u16) -> Option<Vec<u8>> {
    match (kind, index) {
        (1, 0) => {
            let mut bytes = DEVICE;
            for (slot, byte) in bytes.iter_mut().skip(10).zip(product.to_le_bytes()) {
                *slot = byte;
            }
            Some(bytes.to_vec())
        }
        (2, 0) => Some(CONFIG.to_vec()),
        (3, 0) => Some(vec![4, 3, 9, 4]),
        (3, 1..=3) => {
            let text = match index {
                1 => "Synthetic",
                2 => "USBIP simulated Sony",
                _ => "TETHERCAM-USBIP-0001",
            };
            let mut bytes = vec![0, 3];
            for c in text.encode_utf16() {
                bytes.extend_from_slice(&c.to_le_bytes());
            }
            let length = u8::try_from(bytes.len()).ok()?;
            *bytes.first_mut()? = length;
            Some(bytes)
        }
        _ => None,
    }
}
