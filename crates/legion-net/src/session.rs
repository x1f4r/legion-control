//! Multiplexed newline-delimited JSON-RPC 2.0 client session, backed by `legion-link` and adapted
//! for `legion-net`'s child-process connector [`Pipe`].

use std::time::Duration;

use legion_core::CancellationToken;
pub use legion_link::session::{CallError, OpenError, Session, SessionInfo, cancel_reply_timeout};

use crate::connector::Pipe;

/// Opens a [`Session`] over a spawned connector [`Pipe`], completing the `hello` handshake within
/// `timeout`.
pub(crate) async fn open_pipe(
    pipe: Pipe,
    machine: &str,
    system: Option<String>,
    timeout: Duration,
    cancel: CancellationToken,
) -> Result<Session, OpenError> {
    let (stdin, stdout, handle) = pipe.into_parts();
    Session::open_transport(
        stdout,
        stdin,
        Box::new(handle),
        machine,
        system,
        timeout,
        cancel,
    )
    .await
}
