//! Erase model types only at the verification I/O boundary, not in the public model API.
//!
//! The non-generic protocol engine has one framing/lifecycle implementation for every model.
//! Boxed futures are bounded, off the normal camera runtime path, and avoid duplicating that
//! state machine for every personality and fault fixture.
//!
//! References:
//! - <https://doc.rust-lang.org/reference/types/trait-object.html>

use super::{Control, DeviceModel, Identity};
use crate::transport::TransportError;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::Mutex;

type Call<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

pub(super) trait ModelIo: Send + Sync {
    fn identity(&self) -> Call<'_, Identity>;
    fn transfer<'a>(
        &'a self, endpoint: u32, direction: u32, payload: &'a [u8], bytes: &'a mut [u8],
    ) -> Call<'a, Result<usize, TransportError>>;
    fn control<'a>(&'a self, setup: [u8; 8], payload: &'a [u8]) -> Call<'a, io::Result<Control>>;
}

struct Shared<M>(Arc<Mutex<M>>);
impl<M: DeviceModel> ModelIo for Shared<M> {
    fn identity(&self) -> Call<'_, Identity> {
        Box::pin(async { self.0.lock().await.identity() })
    }
    fn transfer<'a>(
        &'a self, endpoint: u32, direction: u32, payload: &'a [u8], bytes: &'a mut [u8],
    ) -> Call<'a, Result<usize, TransportError>> {
        Box::pin(async move {
            self.0
                .lock()
                .await
                .transfer(endpoint, direction, payload, bytes)
                .await
        })
    }
    fn control<'a>(&'a self, setup: [u8; 8], payload: &'a [u8]) -> Call<'a, io::Result<Control>> {
        Box::pin(async move { self.0.lock().await.control(setup, payload).await })
    }
}

pub(super) fn shared<M: DeviceModel>(model: &Arc<Mutex<M>>) -> Arc<dyn ModelIo> {
    Arc::new(Shared(Arc::clone(model)))
}
