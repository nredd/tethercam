//! Transport-independent PTP (Picture Transfer Protocol) wire format.
//!
//! Nothing in this module performs I/O. It only encodes and decodes bytes, so it is fully
//! testable without a camera and usable on any transport (USB bulk pipes today).

mod container;

pub use container::{
    Container, ContainerError, ContainerHeader, ContainerKind, HEADER_LEN, MAX_PARAMS,
};
