//! Transport-independent PTP (Picture Transfer Protocol) wire format.
//!
//! Nothing in this module performs I/O. It only encodes and decodes bytes, so it is fully
//! testable without a camera and usable on any transport (USB bulk pipes today).
//!
//! - [`Container`]: the generic container framing every phase
//! - [`Reader`] / [`Writer`]: bounds-checked little-endian primitives, strings and arrays
//! - [`DataType`] / [`Value`]: datatype codes and dynamically typed values
//! - [`DeviceInfo`] / [`ObjectInfo`] / [`DevicePropDesc`]: the datasets
//! - [`OperationCode`] / [`ResponseCode`] / [`EventCode`] / [`DevicePropCode`]: code tables

mod codec;
mod codes;
mod container;
mod dataset;
mod datatype;
pub mod session;

pub use codec::{CodecError, MAX_STRING_UNITS, Reader, Writer};
pub use codes::{DevicePropCode, EventCode, OperationCode, ResponseCode};
pub use container::{
    Container, ContainerError, ContainerHeader, ContainerKind, HEADER_LEN, MAX_PARAMS,
};
pub use dataset::{DeviceInfo, DevicePropDesc, GetSet, ObjectInfo, PropertyForm};
pub use datatype::{DataType, Value};
