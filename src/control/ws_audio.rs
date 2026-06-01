//! WebSocket audio-plane rendezvous.
//!
//! `endpoint.create_websocket` (control plane) creates a [`WebSocketEndpoint`] in
//! `Connecting` state and registers a single-use `connect_token` here. The audio peer
//! then dials in to `/audio/<connect_token>`; [`handle_audio_connection`] resolves the
//! token to the owning session and hands the upgraded socket to that session's task,
//! which binds it to the endpoint.
//!
//! [`WebSocketEndpoint`]: crate::session::endpoint_websocket::WebSocketEndpoint

use std::sync::Arc;

use dashmap::DashMap;
use tokio::sync::{OwnedSemaphorePermit, oneshot};
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tracing::debug;
use uuid::Uuid;

use crate::control::protocol::{EndpointId, SessionId};
use crate::session::SessionManager;
use crate::session::endpoint_websocket::AudioWsStream;
use crate::session::media_session::SessionCommand;

/// Lightweight routing entry: token -> (owning session, endpoint).
pub struct WsAudioTicket {
    pub cmd_tx: tokio::sync::mpsc::Sender<SessionCommand>,
    pub session_id: SessionId,
    pub endpoint_id: EndpointId,
}

/// Process-wide map of pending WS audio connect tokens.
#[derive(Default)]
pub struct WsAudioRegistry {
    tickets: DashMap<Uuid, WsAudioTicket>,
}

impl WsAudioRegistry {
    pub fn new() -> Self {
        Self {
            tickets: DashMap::new(),
        }
    }

    pub fn insert(&self, token: Uuid, ticket: WsAudioTicket) {
        self.tickets.insert(token, ticket);
    }

    /// Atomically remove and return a ticket (single-use semantics).
    pub fn take(&self, token: &Uuid) -> Option<WsAudioTicket> {
        self.tickets.remove(token).map(|(_, t)| t)
    }

    /// Remove a ticket without consuming it (endpoint teardown).
    pub fn remove(&self, token: &Uuid) {
        self.tickets.remove(token);
    }

    /// Remove every ticket belonging to a session. Used for session teardown
    /// (including the panic/abort cleanup guard), so tokens can't outlive the
    /// session task that would service them.
    pub fn remove_session(&self, session_id: &SessionId) {
        self.tickets.retain(|_, t| t.session_id != *session_id);
    }
}

/// Drive an inbound audio WebSocket: resolve its token and hand the socket to the
/// owning session task. The connection-limit `permit` travels with the socket so it
/// is held for the audio connection's lifetime (released when the IO task ends).
pub async fn handle_audio_connection(
    mut ws: AudioWsStream,
    token: Option<Uuid>,
    permit: OwnedSemaphorePermit,
    manager: &Arc<SessionManager>,
) {
    let token = match token {
        Some(t) => t,
        None => {
            debug!("ws audio: malformed connect token in path");
            close_policy(&mut ws, "malformed connect token").await;
            return;
        }
    };
    let ticket = match manager.ws_audio_registry().take(&token) {
        Some(t) => t,
        None => {
            debug!(%token, "ws audio: unknown or already-used connect token");
            close_policy(&mut ws, "unknown or used connect token").await;
            return;
        }
    };

    let (reply_tx, reply_rx) = oneshot::channel();
    let endpoint_id = ticket.endpoint_id;
    let cmd = SessionCommand::AttachWebSocketAudio {
        reply: reply_tx,
        endpoint_id,
        ws: Box::new(ws),
        permit,
    };

    // On send failure the command (and thus ws + permit) is dropped, closing the socket.
    if ticket.cmd_tx.send(cmd).await.is_err() {
        debug!(%endpoint_id, "ws audio: owning session gone before attach");
        return;
    }

    match reply_rx.await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => debug!(%endpoint_id, error = %e, "ws audio: attach rejected"),
        Err(_) => debug!(%endpoint_id, "ws audio: session dropped attach reply"),
    }
}

/// Close an audio socket with a 1008 (policy violation) close frame.
async fn close_policy(ws: &mut AudioWsStream, reason: &str) {
    let _ = ws
        .close(Some(CloseFrame {
            code: CloseCode::Policy,
            reason: reason.to_string().into(),
        }))
        .await;
}
