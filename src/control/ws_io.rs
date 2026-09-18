//! One bounded writer per socket. Cancellation and deadlines apply to writes,
//! flushes and close frames, independently of control handlers and media input.
use super::transport::ServerWebSocket;
use futures_util::{SinkExt, stream::SplitSink};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;
use tokio_util::sync::CancellationToken;

struct Write {
    message: Message,
    reply: oneshot::Sender<anyhow::Result<()>>,
    _bytes: OwnedSemaphorePermit,
}
#[derive(Clone)]
pub(crate) struct Writer {
    tx: mpsc::Sender<Write>,
    bytes: Arc<Semaphore>,
    cancel: CancellationToken,
}
impl Writer {
    pub fn spawn(
        mut sink: SplitSink<ServerWebSocket, Message>,
        cancel: CancellationToken,
    ) -> (Self, tokio::task::JoinHandle<()>) {
        let (tx, mut rx) = mpsc::channel::<Write>(16);
        let writer = Self {
            tx,
            bytes: Arc::new(Semaphore::new(1024 * 1024)),
            cancel: cancel.clone(),
        };
        let task = tokio::spawn(async move {
            loop {
                let write = tokio::select! {
                    _ = cancel.cancelled() => break,
                    write = rx.recv() => match write { Some(write) => write, None => break },
                };
                let result = tokio::select! {
                    _ = cancel.cancelled() => break,
                    result = tokio::time::timeout(Duration::from_secs(5), sink.send(write.message)) => {
                        result.map_err(anyhow::Error::from).and_then(|result| result.map_err(anyhow::Error::from))
                    }
                };
                let failed = result.is_err();
                let _ = write.reply.send(result);
                if failed {
                    break;
                }
            }
            cancel.cancel();
        });
        (writer, task)
    }
    pub async fn send(&self, message: Message) -> anyhow::Result<()> {
        let size = u32::try_from(message.len().saturating_add(64))?;
        let bytes = Arc::clone(&self.bytes)
            .try_acquire_many_owned(size)
            .map_err(|_| anyhow::anyhow!("WS_BACKPRESSURE"))?;
        let (reply, received) = oneshot::channel();
        let write = Write {
            message,
            reply,
            _bytes: bytes,
        };
        let operation = async {
            let sent = self.tx.send(write).await;
            sent.map_err(|_| anyhow::anyhow!("WebSocket closed"))?;
            let result = received.await;
            result.map_err(|_| anyhow::anyhow!("WebSocket closed"))?
        };
        tokio::select! {
            _ = self.cancel.cancelled() => Err(anyhow::anyhow!("WebSocket closed")),
            result = tokio::time::timeout(Duration::from_secs(5), operation) => {
                let result = result.map_err(anyhow::Error::from).and_then(|result| result);
                if result.is_err() { self.cancel.cancel(); }
                result
            }
        }
    }
}

/// Ensures network children terminate even if their supervisor is aborted.
pub(crate) struct NetworkTasks {
    pub cancel: CancellationToken,
    pub tasks: Vec<tokio::task::JoinHandle<()>>,
}
impl Drop for NetworkTasks {
    fn drop(&mut self) {
        self.cancel.cancel();
        for task in &self.tasks {
            task.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::transport::{BoxedServerIo, PrefixedIo};
    use futures_util::StreamExt;
    use tokio_tungstenite::{WebSocketStream, tungstenite::protocol::Role};

    #[tokio::test(start_paused = true)]
    async fn stalled_writer_releases_byte_budget_and_cancels_connection() {
        let (socket, _unread_peer) = tokio::io::duplex(1);
        let io = PrefixedIo::new(Vec::new(), Box::new(socket) as BoxedServerIo);
        let ws = WebSocketStream::from_raw_socket(io, Role::Server, None).await;
        let (sink, _stream) = ws.split();
        let cancel = CancellationToken::new();
        let (writer, task) = Writer::spawn(sink, cancel.clone());
        let start = tokio::time::Instant::now();
        let result = writer
            .send(Message::Binary(vec![1; 128 * 1024].into()))
            .await;
        assert!(result.is_err());
        let joined = task.await;
        joined.unwrap();
        assert!(cancel.is_cancelled());
        assert_eq!(writer.bytes.available_permits(), 1024 * 1024);
        assert!(start.elapsed() <= Duration::from_secs(5));
    }

    #[tokio::test]
    async fn dropping_supervisor_interrupts_pending_socket_write() {
        let (socket, _unread_peer) = tokio::io::duplex(1);
        let io = PrefixedIo::new(Vec::new(), Box::new(socket) as BoxedServerIo);
        let ws = WebSocketStream::from_raw_socket(io, Role::Server, None).await;
        let (sink, _stream) = ws.split();
        let cancel = CancellationToken::new();
        let (writer, task) = Writer::spawn(sink, cancel.clone());
        let supervisor = NetworkTasks {
            cancel: cancel.clone(),
            tasks: vec![task],
        };
        let sender = tokio::spawn(async move {
            writer
                .send(Message::Binary(vec![1; 128 * 1024].into()))
                .await
        });
        tokio::task::yield_now().await;
        drop(supervisor);
        let joined = tokio::time::timeout(Duration::from_secs(1), sender).await;
        assert!(joined.unwrap().unwrap().is_err());
        assert!(cancel.is_cancelled());
    }
}
