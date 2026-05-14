//! `axum::serve::Listener` wrapper that diverts Icecast SOURCE-protocol
//! uploads to `source_protocol::handle_source_connection` and forwards
//! everything else (listener GETs, health checks, etc.) to axum/hyper.
//!
//! See `source_protocol.rs` for *why* the source side has to bypass hyper.

use crate::source_protocol::handle_source_connection;
use crate::state::AppState;
use axum::extract::connect_info::Connected;
use axum::serve::{IncomingStream, Listener};
use std::io;
use std::net::SocketAddr;
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};
use tracing::warn;

/// Time we'll wait for a freshly-accepted connection to deliver enough bytes
/// to classify it as either a source upload or a regular HTTP request. Long
/// enough to cover sluggish clients, short enough that slow-loris peers don't
/// pile up.
const CLASSIFY_TIMEOUT: Duration = Duration::from_secs(10);

pub struct StreamListener {
    inner: TcpListener,
    state: AppState,
}

impl StreamListener {
    pub fn new(inner: TcpListener, state: AppState) -> Self {
        Self { inner, state }
    }
}

impl Listener for StreamListener {
    type Io = TcpStream;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            let (socket, addr) = match self.inner.accept().await {
                Ok(v) => v,
                Err(e) => {
                    warn!("accept error on stream port: {e}");
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    continue;
                }
            };

            match classify(&socket).await {
                Classification::Source => {
                    let state = self.state.clone();
                    tokio::spawn(async move {
                        if let Err(e) = handle_source_connection(socket, addr, state).await {
                            warn!("source handler error from {addr}: {e}");
                        }
                    });
                    continue;
                }
                Classification::Other => return (socket, addr),
            }
        }
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        self.inner.local_addr()
    }
}

/// Newtype around the listener peer's `SocketAddr`.
///
/// Needed because axum's `Connected` trait is foreign and can't be implemented
/// directly on `SocketAddr` (orphan rule) for our custom listener.
/// `Connected` is implemented for `StreamListener` here and for `TcpListener`
/// just below — the latter lets the e2e tests pass `TcpListener` to `axum::serve`
/// without going through the dispatcher when they don't need source handling.
#[derive(Clone, Copy, Debug)]
pub struct PeerAddr(pub SocketAddr);

impl std::ops::Deref for PeerAddr {
    type Target = SocketAddr;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Connected<IncomingStream<'_, StreamListener>> for PeerAddr {
    fn connect_info(stream: IncomingStream<'_, StreamListener>) -> Self {
        Self(*stream.remote_addr())
    }
}

impl Connected<IncomingStream<'_, TcpListener>> for PeerAddr {
    fn connect_info(stream: IncomingStream<'_, TcpListener>) -> Self {
        Self(*stream.remote_addr())
    }
}

enum Classification {
    /// `PUT ` or `SOURCE ` — handle via `source_protocol`.
    Source,
    /// Anything else, including connections that didn't deliver enough bytes
    /// to classify; let hyper/axum produce whatever response it would normally.
    Other,
}

/// Non-destructively peek at the first bytes of a connection to decide if
/// it's a source upload. Falls back to `Other` on timeout / read error /
/// short reads — those connections still flow to hyper which produces the
/// usual error response.
async fn classify(socket: &TcpStream) -> Classification {
    // 7 bytes covers both "PUT " and "SOURCE " plus the space.
    let mut buf = [0u8; 8];
    let n = match tokio::time::timeout(CLASSIFY_TIMEOUT, socket.peek(&mut buf)).await {
        Ok(Ok(n)) => n,
        _ => return Classification::Other,
    };
    let is_source = (n >= 4 && &buf[..4] == b"PUT ")
        || (n >= 7 && &buf[..7] == b"SOURCE ");
    if is_source {
        Classification::Source
    } else {
        Classification::Other
    }
}
