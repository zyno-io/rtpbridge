use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tracing::{Instrument, debug, debug_span, info, info_span, trace, warn};

use super::handler::{ConnectionState, handle_request};
use super::protocol::{Event, Request, Response};
use crate::session::SessionManager;
use crate::shutdown::ShutdownCoordinator;

/// Maximum allowed length for a request ID (bytes). Prevents memory abuse from oversized IDs
/// that are echoed back in every response.
const MAX_REQUEST_ID_LEN: usize = 1024;

pub async fn handle_connection(
    ws: WebSocketStream<TcpStream>,
    peer_addr: std::net::SocketAddr,
    manager: Arc<SessionManager>,
    shutdown: ShutdownCoordinator,
    ws_ping_interval_secs: u64,
    event_channel_size: usize,
    critical_event_channel_size: usize,
) {
    let span = info_span!("ws_conn", %peer_addr);
    handle_connection_inner(
        ws,
        peer_addr,
        manager,
        shutdown,
        ws_ping_interval_secs,
        event_channel_size,
        critical_event_channel_size,
    )
    .instrument(span)
    .await
}

async fn handle_connection_inner(
    ws: WebSocketStream<TcpStream>,
    peer_addr: std::net::SocketAddr,
    manager: Arc<SessionManager>,
    shutdown: ShutdownCoordinator,
    ws_ping_interval_secs: u64,
    event_channel_size: usize,
    critical_event_channel_size: usize,
) {
    info!("WebSocket connection established");

    let (mut ws_tx, mut ws_rx) = ws.split();
    // Bounded event channel to prevent OOM under back-pressure.
    // 256 events ≈ generous buffer; dropped events are tracked and the
    // client is notified via an events.dropped notification.
    let (event_tx, mut event_rx) = mpsc::channel::<Event>(event_channel_size);
    let (critical_event_tx, mut critical_event_rx) =
        mpsc::channel::<Event>(critical_event_channel_size);
    let dropped_events = Arc::new(AtomicU64::new(0));
    let mut state = ConnectionState::new();

    // Periodic WebSocket ping to detect dead connections behind NATs/load balancers
    let mut ping_interval =
        tokio::time::interval(std::time::Duration::from_secs(ws_ping_interval_secs));
    ping_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // Skip the first immediate tick
    ping_interval.tick().await;

    /// Send the session.orphaned event directly on the WebSocket.
    /// Best-effort: errors are silently ignored since the connection is closing.
    async fn send_orphan_event(
        ws_tx: &mut futures_util::stream::SplitSink<WebSocketStream<TcpStream>, Message>,
        state: &ConnectionState,
        manager: &SessionManager,
    ) {
        if state.session_id.is_some() {
            let orphan_event = Event::new(
                "session.orphaned",
                super::protocol::SessionOrphanedData {
                    timeout_remaining_ms: manager.disconnect_timeout_ms(),
                },
            );
            match serde_json::to_string(&orphan_event) {
                Ok(json) => {
                    let _ = ws_tx.send(Message::Text(json.into())).await;
                }
                Err(e) => {
                    warn!(error = %e, "failed to serialize orphan event");
                }
            }
        }
    }

    loop {
        tokio::select! {
            biased;

            // Critical events get priority delivery
            Some(event) = critical_event_rx.recv() => {
                let json = serde_json::to_string(&event).unwrap_or_else(|e| {
                    warn!(error = %e, "failed to serialize critical event");
                    r#"{"event":"error","data":{}}"#.to_string()
                });
                trace!(payload = %json, "ws send critical event");
                if ws_tx.send(Message::Text(json.into())).await.is_err() {
                    break;
                }
            }

            // Incoming WS message from client
            msg = ws_rx.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        trace!(payload = %text, "ws recv");
                        // NOTE: serde_json has no recursion depth limit. Deeply nested JSON could
                        // cause stack overflow, bounded by the WebSocket message size limit.
                        let req: Request = match serde_json::from_str(&text) {
                            Ok(r) => r,
                            Err(e) => {
                                // Best-effort ID extraction: try to pull "id" from the raw JSON
                                // so the client can correlate the error to their request.
                                let req_id = extract_request_id_best_effort(&text);
                                let resp = Response::err(
                                    req_id,
                                    "PARSE_ERROR",
                                    format!("Invalid JSON: {e}"),
                                );
                                let json = serde_json::to_string(&resp).unwrap_or_else(|_| r#"{"error":{"code":"INTERNAL_ERROR","message":"serialize error"}}"#.to_string());
                                if ws_tx.send(Message::Text(json.into())).await.is_err() {
                                    break;
                                }
                                continue;
                            }
                        };

                        if req.id.len() > MAX_REQUEST_ID_LEN {
                            let truncated_id = truncate_to_byte_boundary(&req.id, MAX_REQUEST_ID_LEN);
                            let resp = Response::err(
                                truncated_id,
                                "INVALID_PARAMS",
                                format!("request id exceeds maximum length of {MAX_REQUEST_ID_LEN}"),
                            );
                            let json = serde_json::to_string(&resp).unwrap_or_else(|_| r#"{"error":{"code":"INTERNAL_ERROR","message":"serialize error"}}"#.to_string());
                            if ws_tx.send(Message::Text(json.into())).await.is_err() {
                                break;
                            }
                            continue;
                        }

                        let req_span = debug_span!("ws_req",
                            id = %req.id,
                            method = %req.method,
                            session_id = tracing::field::Empty,
                        );
                        if let Some(sid) = state.session_id {
                            req_span.record("session_id", tracing::field::display(sid));
                        }
                        let _req_guard = req_span.enter();
                        debug!("request");

                        // Check if server is shutting down for session.create
                        if req.method == "session.create" && shutdown.is_shutting_down() {
                            let resp = Response::err(
                                req.id,
                                "SHUTTING_DOWN",
                                "Server is shutting down, not accepting new sessions",
                            );
                            let json = serde_json::to_string(&resp).unwrap_or_else(|_| r#"{"error":{"code":"INTERNAL_ERROR","message":"serialize error"}}"#.to_string());
                            if ws_tx.send(Message::Text(json.into())).await.is_err() {
                                break;
                            }
                            continue;
                        }

                        let resp = handle_request(req, &mut state, &event_tx, &critical_event_tx, &dropped_events, &manager).await;
                        let json = serde_json::to_string(&resp).unwrap_or_else(|_| r#"{"error":{"code":"INTERNAL_ERROR","message":"serialize error"}}"#.to_string());
                        trace!(payload = %json, "ws send response");
                        if ws_tx.send(Message::Text(json.into())).await.is_err() {
                            break;
                        }

                        // Drain any events generated by this request so they're
                        // delivered immediately after the response (guarantees ordering).
                        // Safety: this runs synchronously in the same select arm before
                        // the loop returns to the top, so no interleaving is possible.
                        // Critical events first, then normal events.
                        if !drain_pending_events(&mut ws_tx, &mut critical_event_rx, &mut event_rx, &dropped_events).await {
                            break;
                        }
                    }
                    Some(Ok(Message::Close(_))) => {
                        // Client initiated close — attempt orphan event delivery.
                        // Best-effort: tungstenite may reject data frames after
                        // receiving a Close frame, so this send can silently fail.
                        send_orphan_event(&mut ws_tx, &state, &manager).await;
                        break;
                    }
                    None => {
                        // TCP connection dropped — ws_tx is still functional from
                        // our side, but the remote peer is gone. Orphan event
                        // delivery is best-effort; nobody may be listening.
                        send_orphan_event(&mut ws_tx, &state, &manager).await;
                        break;
                    }
                    Some(Ok(Message::Ping(data))) => {
                        if ws_tx.send(Message::Pong(data)).await.is_err() {
                            break;
                        }
                    }
                    Some(Ok(_)) => {
                        // Binary, Pong, Frame — ignore
                    }
                    Some(Err(e)) => {
                        warn!(peer = %peer_addr, error = %e, "WebSocket error");
                        break;
                    }
                }
            }

            // Outgoing events to client
            Some(event) = event_rx.recv() => {
                // Check if events were dropped since last delivery and notify client
                let dropped = dropped_events.swap(0, Ordering::Relaxed);
                if dropped > 0 {
                    warn!(count = dropped, "notifying client of dropped events");
                    let drop_event = super::protocol::Event::new(
                        "events.dropped",
                        serde_json::json!({ "count": dropped }),
                    );
                    if let Ok(json) = serde_json::to_string(&drop_event)
                        && ws_tx.send(Message::Text(json.into())).await.is_err() {
                            break;
                        }
                }

                let json = serde_json::to_string(&event).unwrap_or_else(|e| {
                    warn!(error = %e, "failed to serialize event");
                    r#"{"event":"error","data":{}}"#.to_string()
                });
                trace!(payload = %json, "ws send event");
                if ws_tx.send(Message::Text(json.into())).await.is_err() {
                    break;
                }
            }

            // Periodic ping for keepalive / dead connection detection
            _ = ping_interval.tick() => {
                if ws_tx.send(Message::Ping(vec![].into())).await.is_err() {
                    break;
                }
            }
        }
    }

    // Connection closed — detach session if bound
    if let Some(session_id) = state.session_id {
        info!(session_id = %session_id, "connection dropped, orphaning session");
        manager.detach_session(&session_id);
    }

    info!("WebSocket connection closed");
}

/// Drain pending events from both critical and normal channels.
/// Also flushes any accumulated dropped-event notifications so the client
/// learns about drops immediately after the request that caused them.
/// Returns `false` if a WebSocket send failed (connection is dead).
async fn drain_pending_events(
    ws_tx: &mut futures_util::stream::SplitSink<WebSocketStream<TcpStream>, Message>,
    critical_rx: &mut mpsc::Receiver<Event>,
    event_rx: &mut mpsc::Receiver<Event>,
    dropped_events: &AtomicU64,
) -> bool {
    // Flush dropped-events counter before draining, so the client sees the
    // notification before the events that follow.
    let dropped = dropped_events.swap(0, Ordering::Relaxed);
    if dropped > 0 {
        warn!(
            count = dropped,
            "notifying client of dropped events (drain path)"
        );
        let drop_event =
            super::protocol::Event::new("events.dropped", serde_json::json!({ "count": dropped }));
        if let Ok(json) = serde_json::to_string(&drop_event)
            && ws_tx.send(Message::Text(json.into())).await.is_err()
        {
            return false;
        }
    }

    while let Ok(event) = critical_rx.try_recv() {
        let json = serde_json::to_string(&event).unwrap_or_else(|e| {
            warn!(error = %e, "failed to serialize critical event");
            r#"{"event":"error","data":{}}"#.to_string()
        });
        trace!(payload = %json, "ws send critical event");
        if ws_tx.send(Message::Text(json.into())).await.is_err() {
            return false;
        }
    }
    while let Ok(event) = event_rx.try_recv() {
        let json = serde_json::to_string(&event).unwrap_or_else(|e| {
            warn!(error = %e, "failed to serialize event");
            r#"{"event":"error","data":{}}"#.to_string()
        });
        trace!(payload = %json, "ws send event");
        if ws_tx.send(Message::Text(json.into())).await.is_err() {
            return false;
        }
    }
    true
}

/// Best-effort extraction of the "id" field from raw JSON text.
/// Used when the full Request deserialization fails, so we can still
/// correlate the error response to the client's request.
fn extract_request_id_best_effort(text: &str) -> String {
    serde_json::from_str::<serde_json::Value>(text)
        .ok()
        .and_then(|v| v.get("id")?.as_str().map(String::from))
        .filter(|s| s.len() <= MAX_REQUEST_ID_LEN)
        .unwrap_or_default()
}

/// Truncate a string to at most `max_bytes` bytes, respecting UTF-8 char boundaries.
fn truncate_to_byte_boundary(s: &str, max_bytes: usize) -> String {
    let mut end = max_bytes.min(s.len());
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- Fallback error JSON consistency ---

    const FALLBACK_ERROR_JSON: &str =
        r#"{"error":{"code":"INTERNAL_ERROR","message":"serialize error"}}"#;

    #[test]
    fn test_fallback_error_json_is_valid_and_uses_internal_error() {
        let parsed: serde_json::Value = serde_json::from_str(FALLBACK_ERROR_JSON).unwrap();
        assert_eq!(parsed["error"]["code"], "INTERNAL_ERROR");
        assert_eq!(parsed["error"]["message"], "serialize error");
    }

    // --- Request ID truncation ---

    #[test]
    fn test_max_request_id_len_constant() {
        assert_eq!(MAX_REQUEST_ID_LEN, 1024);
    }

    #[test]
    fn test_truncate_ascii_at_boundary() {
        let s = "a".repeat(2000);
        let t = truncate_to_byte_boundary(&s, MAX_REQUEST_ID_LEN);
        assert_eq!(t.len(), 1024);
    }

    #[test]
    fn test_truncate_multibyte_respects_char_boundary() {
        // Each emoji is 4 bytes. 256 emojis = 1024 bytes exactly.
        let s = "\u{1F600}".repeat(257); // 1028 bytes
        let t = truncate_to_byte_boundary(&s, MAX_REQUEST_ID_LEN);
        // Should truncate to 256 chars (1024 bytes), not split mid-char
        assert_eq!(t.len(), 1024);
        assert_eq!(t.chars().count(), 256);
    }

    #[test]
    fn test_truncate_at_mid_char_boundary() {
        // 2-byte chars: "ñ" is 2 bytes. 513 of them = 1026 bytes.
        let s = "ñ".repeat(513);
        let t = truncate_to_byte_boundary(&s, MAX_REQUEST_ID_LEN);
        // 1024 is even, ñ is 2 bytes, so 512 chars = 1024 bytes
        assert_eq!(t.len(), 1024);
        assert_eq!(t.chars().count(), 512);
    }

    // --- Best-effort ID extraction ---

    #[test]
    fn test_extract_id_from_partial_json() {
        assert_eq!(extract_request_id_best_effort(r#"{"id":"abc"}"#), "abc");
    }

    #[test]
    fn test_extract_id_with_extra_fields() {
        assert_eq!(
            extract_request_id_best_effort(r#"{"id":"req-1","method":"test","params":{}}"#),
            "req-1"
        );
    }

    #[test]
    fn test_extract_id_missing_returns_empty() {
        assert_eq!(extract_request_id_best_effort(r#"{"method":"test"}"#), "");
    }

    #[test]
    fn test_extract_id_non_string_returns_empty() {
        assert_eq!(extract_request_id_best_effort(r#"{"id":42}"#), "");
    }

    #[test]
    fn test_extract_id_oversized_returns_empty() {
        let long_id = "x".repeat(MAX_REQUEST_ID_LEN + 1);
        let json = format!(r#"{{"id":"{}"}}"#, long_id);
        assert_eq!(extract_request_id_best_effort(&json), "");
    }

    #[test]
    fn test_extract_id_invalid_json_returns_empty() {
        assert_eq!(extract_request_id_best_effort("not json at all"), "");
    }
}
