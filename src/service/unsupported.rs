//! Portable fail-closed fallback outside Unix.
//!
//! References: docs/service.md, supported platform scope.

use super::{Action, Error, Status};

/// Unsupported native manager; construction never mutates host state.
pub struct Manager;

impl Manager {
    /// Reject an unsupported operating system.
    ///
    /// # Errors
    /// Always returns `Error::Unsupported`.
    pub fn new(_user: bool) -> Result<Self, Error> {
        Err(Error::Unsupported)
    }

    /// Reject a native lifecycle operation.
    ///
    /// # Errors
    /// Always returns `Error::Unsupported`.
    pub fn manage(&mut self, _action: Action) -> Result<Status, Error> {
        Err(Error::Unsupported)
    }
}
