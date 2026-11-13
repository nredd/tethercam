//! Exercise Sony lifecycle through an independently implemented session boundary.
//!
//! References:
//! - `docs/architecture.md`, Sony driver contract
use std::time::{Duration, Instant};

use tethercam::ptp::session::{Event, Outcome, PtpSession, Session, SessionConfig, SessionError};
use tethercam::sony::{SonyCamera, SonyConfig, SonyError};
use tethercam::transport::TransportError;
use tethercam::transport::sim::SimCamera;
use tethercam::transport::sim::sony::{SonySimConfig, sony_camera};

struct ObservedSession {
    raw: Session<SimCamera>,
    operations: Vec<u16>,
    closed: usize,
    fail_close: bool,
}

impl PtpSession for ObservedSession {
    fn now(&self) -> Instant {
        self.raw.now()
    }
    fn is_open(&self) -> bool {
        self.raw.is_open()
    }
    async fn open(&mut self) -> Result<(), SessionError> {
        self.raw.open().await
    }
    async fn close(&mut self) -> Result<(), SessionError> {
        self.closed += 1;
        self.raw.close().await?;
        if self.fail_close {
            Err(TransportError::Disconnected.into())
        } else {
            Ok(())
        }
    }
    async fn transaction(
        &mut self, operation: u16, params: &[u32], data: Option<&[u8]>,
    ) -> Result<Outcome, SessionError> {
        self.operations.push(operation);
        self.raw.transaction(operation, params, data).await
    }
    async fn transaction_within(
        &mut self, limit: Duration, operation: u16, params: &[u32], data: Option<&[u8]>,
    ) -> Result<Outcome, SessionError> {
        self.operations.push(operation);
        self.raw
            .transaction_within(limit, operation, params, data)
            .await
    }
    async fn get_device_info(&mut self) -> Result<Vec<u8>, SessionError> {
        self.raw.get_device_info().await
    }
    async fn poll_event(&mut self) -> Result<Option<Event>, SessionError> {
        self.raw.poll_event().await
    }
    async fn poll_event_within(
        &mut self, timeout: Duration,
    ) -> Result<Option<Event>, SessionError> {
        self.raw.poll_event_within(timeout).await
    }
}

fn observed() -> ObservedSession {
    ObservedSession {
        raw: Session::new(
            sony_camera(SonySimConfig::default()),
            SessionConfig::default(),
        ),
        operations: Vec::new(),
        closed: 0,
        fail_close: true,
    }
}

#[tokio::test]
async fn sony_uses_session_boundary_and_preserves_close_error_precedence() {
    let mut session = observed();
    let result = SonyCamera::run(&mut session, SonyConfig::default(), async |camera| {
        assert_ne!(camera.device_info().model, "");
        camera.refresh_properties().await?;
        Err::<(), _>(SonyError::LiveViewNotReady)
    })
    .await;
    assert!(matches!(result, Err(SonyError::LiveViewNotReady)));
    assert_ne!(session.operations, [0_u16; 0]);
    assert_eq!(session.closed, 1);
    assert!(!session.raw.is_open());

    let mut session = observed();
    let result = SonyCamera::run(&mut session, SonyConfig::default(), async |_| Ok(())).await;
    assert!(matches!(
        result,
        Err(SonyError::Session(SessionError::Transport(
            TransportError::Disconnected
        )))
    ));
    assert_eq!(session.closed, 1);
    assert!(!session.raw.is_open());
}
