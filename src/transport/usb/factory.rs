//! Fallible device selection shared by native enumeration and deterministic source tests.
//!
//! Native enumeration failure is not a camera fault. Keep that decision, selection of an
//! absent address, and opening failure separate from the direct OS calls in the adapter.
//!
//! References:
//! - nusb 0.2 enumeration and open: <https://docs.rs/nusb/0.2>

use super::{DeviceAddress, UsbError};

/// Select an exact address, preserving enumeration and open errors.
pub(crate) async fn open<I, D, F: Future<Output = Result<D, UsbError>>>(
    devices: Result<Vec<I>, UsbError>, address: &DeviceAddress,
    address_of: fn(&I) -> DeviceAddress, opener: fn(I) -> F,
) -> Result<D, UsbError> {
    let found = devices?
        .into_iter()
        .find(|info| address_of(info) == *address)
        .ok_or(UsbError::Disconnected)?;
    opener(found).await
}

#[cfg(test)]
mod tests {
    use std::future::{Ready, ready};

    use super::*;

    struct Info {
        address: DeviceAddress,
        result: Result<u8, UsbError>,
    }

    fn info(value: u8) -> Info {
        Info {
            address: DeviceAddress {
                bus: "synthetic".into(),
                address: value,
            },
            result: if value == 2 {
                Err(UsbError::PermissionDenied)
            } else {
                Ok(value)
            },
        }
    }

    fn address_of(value: &Info) -> DeviceAddress {
        value.address.clone()
    }

    fn opener(value: Info) -> Ready<Result<u8, UsbError>> {
        ready(value.result)
    }

    #[tokio::test]
    async fn one_concrete_source_covers_every_factory_outcome() {
        let run = |devices, value| async move {
            open(devices, &address_of(&info(value)), address_of, opener).await
        };
        assert_eq!(run(Err(UsbError::Busy), 1).await, Err(UsbError::Busy));
        assert_eq!(
            run(Ok(vec![info(1), info(2)]), 3).await,
            Err(UsbError::Disconnected)
        );
        assert_eq!(
            run(Ok(vec![info(1), info(2)]), 2).await,
            Err(UsbError::PermissionDenied)
        );
        assert_eq!(run(Ok(vec![info(2), info(1)]), 1).await, Ok(1));
    }
}
