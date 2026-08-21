//! Common boxed transport for plaintext and TLS control connections.

use std::cmp;
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio_tungstenite::WebSocketStream;

/// Trait object boundary that lets the control pipeline treat a plain TCP and
/// a TLS-upgraded stream identically.
pub trait ServerIo: AsyncRead + AsyncWrite + Send + Unpin {}

impl<T> ServerIo for T where T: AsyncRead + AsyncWrite + Send + Unpin {}

pub type BoxedServerIo = Box<dyn ServerIo>;

/// Replays a prefetched HTTP request header to tungstenite after rtpbridge has
/// classified and authorized it.
pub struct PrefixedIo<S> {
    prefix: Vec<u8>,
    prefix_offset: usize,
    inner: S,
}

impl<S> std::fmt::Debug for PrefixedIo<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PrefixedIo")
            .field("prefetched_bytes", &self.prefix.len())
            .field("prefetched_offset", &self.prefix_offset)
            .field("inner", &"[ERASED]")
            .finish()
    }
}

impl<S> PrefixedIo<S> {
    pub fn new(prefix: Vec<u8>, inner: S) -> Self {
        Self {
            prefix,
            prefix_offset: 0,
            inner,
        }
    }
}

impl<S> AsyncRead for PrefixedIo<S>
where
    S: AsyncRead + Unpin,
{
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        if self.prefix_offset < self.prefix.len() && buf.remaining() > 0 {
            let start = self.prefix_offset;
            let len = cmp::min(self.prefix.len() - start, buf.remaining());
            buf.put_slice(&self.prefix[start..start + len]);
            self.prefix_offset = start + len;
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<S> AsyncWrite for PrefixedIo<S>
where
    S: AsyncWrite + Unpin,
{
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

pub type ServerWebSocket = WebSocketStream<PrefixedIo<BoxedServerIo>>;
