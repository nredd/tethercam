//! A single-flight bridge from async callers to the blocking native owner.
//!
//! The native boundary blocks, and its lease borrows the boundary, so it lives on one owner
//! thread ([`serve`]). Async callers hold an [`IccHandle`] and exchange one request at a time
//! with it:
//! - a shared busy gate admits one request. It is released by the owner when the work has
//!   finished, not when a reply is received, so a cancelled caller future neither backlogs a
//!   second request nor lets one overlap the first. A new request meanwhile is
//!   [`IccError::Busy`], and the queue is bounded to one by construction
//! - dropping a caller future never cancels the native operation. Its result is discarded
//! - a lease is released only through confirmed close or an error that retires it
//!   ([`IccError::retires_lease`]), from a command, close or event poll; the native owner
//!   settles whatever remains
//! - the owner exits when the handle is dropped
//!
//! References:
//! - `docs/macos-camera.md`, Ownership and deadlines

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use tokio::sync::oneshot;

use super::{Boundary, IccDeviceId, IccError, IccReply, IccTimeout, Lease};

type Reply<T> = oneshot::Sender<Result<T, IccError>>;

/// One owned request. No variant borrows from the caller.
enum Request {
    Discover {
        window: Duration,
        timeout: IccTimeout,
        reply: Reply<Vec<IccDeviceId>>,
    },
    Open {
        id: IccDeviceId,
        timeout: IccTimeout,
        reply: Reply<()>,
    },
    Transact {
        operation: u16,
        params: Vec<u32>,
        outgoing: Vec<u8>,
        timeout: IccTimeout,
        reply: Reply<IccReply>,
    },
    Close {
        timeout: IccTimeout,
        reply: Reply<()>,
    },
    Event {
        reply: Reply<Option<Vec<u8>>>,
    },
}

/// The caller side of the bridge. It is not cloneable: one owner, one request in flight.
#[derive(Debug)]
pub struct IccHandle {
    requests: mpsc::Sender<Request>,
    busy: Arc<AtomicBool>,
}

impl IccHandle {
    async fn call<T>(&mut self, build: impl FnOnce(Reply<T>) -> Request) -> Result<T, IccError> {
        if self
            .busy
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(IccError::Busy);
        }
        let (reply, received) = oneshot::channel();
        // A send to an ended owner drops the request, and with it the reply sender, which
        // `received` reports exactly like an owner that died before answering.
        drop(self.requests.send(build(reply)));
        if let Ok(result) = received.await {
            result
        } else {
            self.busy.store(false, Ordering::Release);
            Err(IccError::Disconnected)
        }
    }

    /// Observe a bounded discovery window.
    ///
    /// # Errors
    ///
    /// [`IccError::Busy`] while any request is outstanding, otherwise the boundary's error.
    pub async fn discover(
        &mut self, window: Duration, timeout: IccTimeout,
    ) -> Result<Vec<IccDeviceId>, IccError> {
        self.call(|reply| Request::Discover {
            window,
            timeout,
            reply,
        })
        .await
    }

    /// Acquire the only session lease.
    ///
    /// # Errors
    ///
    /// As [`IccHandle::discover`].
    pub async fn open(&mut self, id: IccDeviceId, timeout: IccTimeout) -> Result<(), IccError> {
        self.call(|reply| Request::Open { id, timeout, reply })
            .await
    }

    /// Run one command on the held lease.
    ///
    /// # Errors
    ///
    /// As [`IccHandle::discover`], or [`IccError::NotOpen`] without a lease.
    pub async fn transact(
        &mut self, operation: u16, params: Vec<u32>, outgoing: Vec<u8>, timeout: IccTimeout,
    ) -> Result<IccReply, IccError> {
        self.call(|reply| Request::Transact {
            operation,
            params,
            outgoing,
            timeout,
            reply,
        })
        .await
    }

    /// Close the held lease with confirmation.
    ///
    /// # Errors
    ///
    /// As [`IccHandle::transact`].
    pub async fn close(&mut self, timeout: IccTimeout) -> Result<(), IccError> {
        self.call(|reply| Request::Close { timeout, reply }).await
    }

    /// Take one queued advisory event, or `None`.
    ///
    /// # Errors
    ///
    /// As [`IccHandle::discover`], plus the boundary's event errors.
    pub async fn poll_event(&mut self) -> Result<Option<Vec<u8>>, IccError> {
        self.call(|reply| Request::Event { reply }).await
    }
}

/// The owner side: receives requests and releases the busy gate.
struct Inbox {
    requests: mpsc::Receiver<Request>,
    busy: Arc<AtomicBool>,
}

impl Drop for Inbox {
    /// An owner that ends, even by unwinding, must not leave the gate held: with it open the
    /// caller's next send fails and reports a disconnect instead of waiting for an answer.
    fn drop(&mut self) {
        self.busy.store(false, Ordering::Release);
    }
}

impl Inbox {
    /// Release the gate first, so a caller that sees the reply may submit immediately.
    fn answer<T>(&self, reply: Reply<T>, result: Result<T, IccError>) {
        self.busy.store(false, Ordering::Release);
        // A dropped receiver is a cancelled caller; the result is deliberately discarded.
        drop(reply.send(result));
    }
}

fn channel() -> (IccHandle, Inbox) {
    let (sender, receiver) = mpsc::channel();
    let busy = Arc::new(AtomicBool::new(false));
    (
        IccHandle {
            requests: sender,
            busy: Arc::clone(&busy),
        },
        Inbox {
            requests: receiver,
            busy,
        },
    )
}

/// A lease that answers a command or close as busy is closing natively and cannot be used
/// again. That is "not open" for the caller, unlike the gate's own busy, which submitted nothing.
fn lease_failure(error: IccError) -> IccError {
    if error == IccError::Busy {
        IccError::NotOpen
    } else {
        error
    }
}

/// Serve requests while a lease is held. Returns whether the owner should keep serving.
fn hold<L: Lease>(mut lease: L, inbox: &Inbox) -> bool {
    loop {
        let Ok(request) = inbox.requests.recv() else {
            return false;
        };
        match request {
            Request::Transact {
                operation,
                params,
                outgoing,
                timeout,
                reply,
            } => {
                let result = lease
                    .transact(operation, &params, outgoing, timeout)
                    .map_err(lease_failure);
                let retire = result.as_ref().is_err_and(|e| e.retires_lease());
                inbox.answer(reply, result);
                if retire {
                    return true;
                }
            }
            Request::Close { timeout, reply } => {
                inbox.answer(reply, lease.close(timeout).map_err(lease_failure));
                return true;
            }
            Request::Event { reply } => {
                let result = lease.poll_event();
                let retire = result.as_ref().is_err_and(|e| e.retires_lease());
                inbox.answer(reply, result);
                if retire {
                    return true;
                }
            }
            Request::Open { reply, .. } => inbox.answer(reply, Err(IccError::Busy)),
            Request::Discover { reply, .. } => inbox.answer(reply, Err(IccError::Busy)),
        }
    }
}

/// Most stale events discarded per open: the native queue holds 64, plus its overflow flag.
const MAX_STALE: usize = 66;

/// Discard what the boundary's one process-wide event queue held before this session opened:
/// stale PTP events, a predecessor's native error or access restriction, and the sticky overflow
/// flag, which a poll clears. Without this the first poll of a new session would fault on, or
/// act on, another session's history. Removal is not lost: the native owner keeps its own sticky
/// removal state that quarantines independently of the queue.
fn discard_stale<L: Lease>(lease: &mut L) {
    for _ in 0..MAX_STALE {
        match lease.poll_event() {
            Ok(None) | Err(IccError::Disconnected) => return,
            Ok(Some(_)) | Err(_) => {}
        }
    }
}

/// Serve requests until the handle is dropped. The inbox is owned so that an owner that
/// unwinds also closes the queue, and callers then see a disconnect instead of waiting forever.
fn serve<B: Boundary>(boundary: &mut B, inbox: Inbox) {
    while let Ok(request) = inbox.requests.recv() {
        match request {
            Request::Discover {
                window,
                timeout,
                reply,
            } => inbox.answer(reply, boundary.discover(window, timeout)),
            Request::Open { id, timeout, reply } => match boundary.open(&id, timeout) {
                Ok(mut lease) => {
                    discard_stale(&mut lease);
                    inbox.answer(reply, Ok(()));
                    if !hold(lease, &inbox) {
                        return;
                    }
                }
                Err(error) => inbox.answer(reply, Err(error)),
            },
            Request::Event { reply } => inbox.answer(reply, boundary.poll_event()),
            Request::Transact { reply, .. } => inbox.answer(reply, Err(IccError::NotOpen)),
            Request::Close { reply, .. } => inbox.answer(reply, Err(IccError::NotOpen)),
        }
    }
    drop(inbox);
}

/// Run `user` with a handle while this thread owns `boundary`, then join `user`.
///
/// This is the portable core of the native entry point: `native::run` supplies the real
/// boundary on the worker thread that `image-capture` owns, and tests supply a deterministic
/// one. The owner serves until `user` drops its handle, so a lease still held then is dropped,
/// which only requests close; call [`IccHandle::close`] first when confirmation matters. `user`
/// must drop the handle before it returns: a handle smuggled out in `T` keeps the owner serving
/// forever.
///
/// # Errors
///
/// [`IccError::Runtime`] when `user` panicked.
pub fn run_with<B, F, T>(mut boundary: B, user: F) -> Result<T, IccError>
where
    B: Boundary,
    F: FnOnce(IccHandle) -> T + Send,
    T: Send,
{
    let (handle, inbox) = channel();
    std::thread::scope(|scope| {
        let user = scope.spawn(move || user(handle));
        serve(&mut boundary, inbox);
        user.join().map_err(|_| IccError::Runtime)
    })
}
