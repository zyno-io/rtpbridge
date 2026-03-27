#![allow(dead_code)]

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// WebSocket JSON client for testing the rtpbridge control protocol.
pub struct TestControlClient {
    ws_tx: futures_util::stream::SplitSink<WsStream, Message>,
    resp_rx: mpsc::UnboundedReceiver<Value>,
    event_rx: mpsc::UnboundedReceiver<Value>,
}

impl TestControlClient {
    /// Connect to rtpbridge WebSocket server with retry logic to handle
    /// startup race conditions (server may not be listening yet).
    pub async fn connect(addr: &str) -> Self {
        let url = format!("ws://{addr}");
        let retries = 20;
        let mut ws = None;
        for i in 0..retries {
            match connect_async(&url).await {
                Ok((stream, _)) => {
                    ws = Some(stream);
                    break;
                }
                Err(_) if i < retries - 1 => {
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
                Err(e) => panic!("WS connect failed after {retries} retries: {e}"),
            }
        }
        let (ws_tx, mut ws_rx) = ws.expect("WS connect failed").split();

        let (resp_tx, resp_rx) = mpsc::unbounded_channel::<Value>();
        let (event_tx, event_rx) = mpsc::unbounded_channel::<Value>();

        // Background task to route incoming messages to responses or events
        tokio::spawn(async move {
            while let Some(Ok(msg)) = ws_rx.next().await {
                if let Message::Text(text) = msg {
                    if let Ok(val) = serde_json::from_str::<Value>(&text) {
                        if val.get("event").is_some() {
                            let _ = event_tx.send(val);
                        } else {
                            let _ = resp_tx.send(val);
                        }
                    }
                }
            }
        });

        Self {
            ws_tx,
            resp_rx,
            event_rx,
        }
    }

    /// Send a request and wait for the response (with timeout)
    pub async fn request(&mut self, method: &str, params: Value) -> Value {
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed).to_string();
        let req = json!({ "id": id, "method": method, "params": params });

        self.ws_tx
            .send(Message::Text(req.to_string().into()))
            .await
            .expect("WS send failed");

        tokio::time::timeout(Duration::from_secs(5), self.resp_rx.recv())
            .await
            .expect("response timeout")
            .expect("response channel closed")
    }

    /// Wait for the next event (with timeout)
    pub async fn recv_event(&mut self, timeout: Duration) -> Option<Value> {
        tokio::time::timeout(timeout, self.event_rx.recv())
            .await
            .ok()
            .flatten()
    }

    /// Wait for an event of a specific type, skipping unrelated events (with timeout)
    pub async fn recv_event_type(&mut self, event_type: &str, timeout: Duration) -> Option<Value> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return None;
            }
            match tokio::time::timeout(remaining, self.event_rx.recv()).await {
                Ok(Some(event)) => {
                    if event["event"].as_str() == Some(event_type) {
                        return Some(event);
                    }
                    // Skip non-matching event and continue
                }
                _ => return None,
            }
        }
    }

    /// Send request and assert success, returning the result object
    pub async fn request_ok(&mut self, method: &str, params: Value) -> Value {
        let resp = self.request(method, params).await;
        assert!(
            resp.get("result").is_some(),
            "expected success for {method}: {resp}"
        );
        resp["result"].clone()
    }

    /// Send a raw text message and wait for a response (for malformed input testing)
    pub async fn send_raw(&mut self, text: &str) -> Value {
        self.ws_tx
            .send(Message::Text(text.into()))
            .await
            .expect("WS send failed");

        tokio::time::timeout(Duration::from_secs(5), self.resp_rx.recv())
            .await
            .expect("response timeout")
            .expect("response channel closed")
    }

    /// Explicitly close the WebSocket connection (sends close frame).
    /// Use this when you need the server to detect the disconnect promptly.
    pub async fn close(mut self) {
        let _ = self.ws_tx.close().await;
    }
}
