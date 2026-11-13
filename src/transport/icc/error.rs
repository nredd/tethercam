//! Sanitized `ImageCaptureCore` failure categories.
//!
//! Every variant is a static category. Framework error text, device identifiers and payload
//! bytes never cross this boundary, so each is safe to log. Camera refusal responses are not
//! errors here: they stay protocol results ([`SessionError::Response`](crate::ptp::session::SessionError)).
//!
//! References:
//! - `docs/macos-camera.md`, Ownership and deadlines

use thiserror::Error;

/// What a failure means for the session that observed it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    /// Nothing was submitted or changed; the session is as it was.
    Keep,
    /// The session cannot carry further transactions but still owns a lease that must be
    /// closed with confirmation.
    Fault,
    /// The native lease was retired. Native cleanup continues on its own; a replacement
    /// session is refused as busy until it settles.
    Retire,
    /// Ownership cannot be resolved. No new work and no replacement session, ever.
    Quarantine,
}

/// Why an `ImageCaptureCore` operation failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum IccError {
    /// Camera access is restricted for this process.
    #[error("camera access restricted")]
    Permission,
    /// A native operation or cleanup is outstanding.
    #[error("camera busy with an outstanding native operation")]
    Busy,
    /// The selected camera was removed.
    #[error("camera removed")]
    Removed,
    /// Admission expired before native submission; nothing was submitted.
    #[error("admission expired before submission")]
    AdmissionExpired,
    /// The caller's deadline passed after submission. Native work is NOT cancelled.
    #[error("deadline expired, native work retained")]
    DeadlineExceeded,
    /// A native completion could not be framed or violated a bound.
    #[error("malformed native completion")]
    MalformedCompletion,
    /// The bounded advisory event queue overflowed, so signals were lost.
    #[error("event queue overflowed")]
    EventOverflow,
    /// An event could not be framed.
    #[error("malformed event")]
    MalformedEvent,
    /// Closing the session failed.
    #[error("native cleanup failed")]
    CleanupFailure,
    /// The native session graph is quarantined.
    #[error("native session quarantined")]
    Quarantined,
    /// Exact selection found no camera.
    #[error("no matching camera")]
    NotFound,
    /// Exact selection found several cameras.
    #[error("more than one matching camera")]
    Ambiguous,
    /// A deadline, identifier, operation or payload was invalid.
    #[error("invalid input")]
    InvalidInput,
    /// A native bound was exceeded.
    #[error("native bound exceeded")]
    LimitExceeded,
    /// The framework reported a failure.
    #[error("native failure")]
    NativeFailure,
    /// A native callback adapter failed internally.
    #[error("native callback failure")]
    CallbackFailure,
    /// `ImageCaptureCore` owns session negotiation; raw `OpenSession` and `CloseSession` are
    /// never forwarded.
    #[error("raw session operations are not permitted")]
    RawSessionOperation,
    /// There is no open native session.
    #[error("no open native session")]
    NotOpen,
    /// The bridge or native owner ended.
    #[error("native owner ended")]
    Disconnected,
    /// The native runtime could not be created or its worker failed.
    #[error("native runtime unavailable")]
    Runtime,
}

impl IccError {
    /// The consequence of this failure for the session that saw it.
    #[must_use]
    pub const fn effect(self) -> Effect {
        match self {
            Self::AdmissionExpired
            | Self::InvalidInput
            | Self::Busy
            | Self::RawSessionOperation => Effect::Keep,
            Self::DeadlineExceeded | Self::NotOpen => Effect::Retire,
            Self::Quarantined
            | Self::Disconnected
            | Self::CallbackFailure
            | Self::Removed
            | Self::CleanupFailure
            | Self::Runtime => Effect::Quarantine,
            Self::NativeFailure
            | Self::MalformedCompletion
            | Self::LimitExceeded
            | Self::EventOverflow
            | Self::MalformedEvent
            | Self::Permission
            | Self::NotFound
            | Self::Ambiguous => Effect::Fault,
        }
    }

    /// Whether the bridge must give up its native lease after this failure: the lease is
    /// retired or unresolved, and only the native owner can settle it.
    #[must_use]
    pub const fn retires_lease(self) -> bool {
        matches!(self.effect(), Effect::Retire | Effect::Quarantine) || matches!(self, Self::Busy)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [IccError; 20] = [
        IccError::Permission,
        IccError::Busy,
        IccError::Removed,
        IccError::AdmissionExpired,
        IccError::DeadlineExceeded,
        IccError::MalformedCompletion,
        IccError::EventOverflow,
        IccError::MalformedEvent,
        IccError::CleanupFailure,
        IccError::Quarantined,
        IccError::NotFound,
        IccError::Ambiguous,
        IccError::InvalidInput,
        IccError::LimitExceeded,
        IccError::NativeFailure,
        IccError::CallbackFailure,
        IccError::RawSessionOperation,
        IccError::NotOpen,
        IccError::Disconnected,
        IccError::Runtime,
    ];

    #[test]
    fn every_category_has_a_distinct_sanitized_message() {
        let mut seen = std::collections::BTreeSet::new();
        for error in ALL {
            let text = error.to_string();
            assert!(!text.is_empty() && !text.contains('/'));
            assert!(seen.insert(text));
        }
    }

    #[test]
    fn effects_partition_the_categories() {
        let effect = |errors: &[IccError]| errors.iter().map(|e| e.effect()).collect::<Vec<_>>();
        assert_eq!(effect(&[IccError::AdmissionExpired]), [Effect::Keep]);
        assert_eq!(effect(&[IccError::DeadlineExceeded]), [Effect::Retire]);
        assert_eq!(effect(&[IccError::Removed]), [Effect::Quarantine]);
        assert_eq!(effect(&[IccError::EventOverflow]), [Effect::Fault]);
        let retiring: Vec<_> = ALL.into_iter().filter(|e| e.retires_lease()).collect();
        assert_eq!(
            retiring,
            [
                IccError::Busy,
                IccError::Removed,
                IccError::DeadlineExceeded,
                IccError::CleanupFailure,
                IccError::Quarantined,
                IccError::CallbackFailure,
                IccError::NotOpen,
                IccError::Disconnected,
                IccError::Runtime,
            ]
        );
    }
}
