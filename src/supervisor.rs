//! Single-owner camera actor and bounded, on-demand live-view pipeline.
//!
//! Cancellation is a request, never cancellation of a camera future. The actor observes it
//! only between completed Sony operations and always awaits `SonyCamera::run` through close.
//!
//! References:
//! - `docs/architecture.md`, Sony driver and streams contracts
//! - [RTP, RFC 3550](https://www.rfc-editor.org/rfc/rfc3550)

mod actor;
mod pipeline;
mod runtime;
#[cfg(test)]
pub(crate) mod testing;

pub use actor::{CameraFactory, CameraStatus, Control, OpenError, Supervisor, SupervisorError};
pub use pipeline::{PipelineError, RuntimeSource};
pub use runtime::{RunError, serve};
