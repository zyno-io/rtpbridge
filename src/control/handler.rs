use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};

use super::protocol::*;
use crate::session::{SessionCommand, SessionManager};

const CMD_TIMEOUT: Duration = Duration::from_secs(5);

/// Send a command to the session task and await the reply, with a timeout.
async fn send_and_recv<T>(
    cmd_tx: &mpsc::Sender<SessionCommand>,
    cmd: SessionCommand,
    reply_rx: oneshot::Receiver<T>,
    id: &str,
) -> Result<T, Response> {
    match tokio::time::timeout(CMD_TIMEOUT, cmd_tx.send(cmd)).await {
        Ok(Ok(())) => {}
        Ok(Err(_)) => {
            return Err(Response::err(
                id.to_string(),
                "SESSION_GONE",
                "Session task ended",
            ));
        }
        Err(_) => {
            return Err(Response::err(
                id.to_string(),
                "SESSION_UNRESPONSIVE",
                "Session did not accept command within timeout",
            ));
        }
    }

    match tokio::time::timeout(CMD_TIMEOUT, reply_rx).await {
        Ok(Ok(val)) => Ok(val),
        Ok(Err(_)) => Err(Response::err(
            id.to_string(),
            "SESSION_GONE",
            "Session task ended",
        )),
        Err(_) => Err(Response::err(
            id.to_string(),
            "SESSION_UNRESPONSIVE",
            "Session did not reply within timeout",
        )),
    }
}

/// State for a single control connection's bound session
pub struct ConnectionState {
    pub session_id: Option<SessionId>,
    pub cmd_tx: Option<mpsc::Sender<SessionCommand>>,
}

impl Default for ConnectionState {
    fn default() -> Self {
        Self::new()
    }
}

impl ConnectionState {
    pub fn new() -> Self {
        Self {
            session_id: None,
            cmd_tx: None,
        }
    }

    fn require_session(&self, id: &str) -> Result<&mpsc::Sender<SessionCommand>, Response> {
        self.cmd_tx.as_ref().ok_or_else(|| {
            Response::err(
                id.to_string(),
                "NO_SESSION",
                "No session bound. Call session.create or session.attach first.",
            )
        })
    }
}

pub async fn handle_request(
    req: Request,
    state: &mut ConnectionState,
    event_tx: &mpsc::Sender<Event>,
    critical_event_tx: &mpsc::Sender<Event>,
    dropped_events: &Arc<AtomicU64>,
    manager: &Arc<SessionManager>,
) -> Response {
    let id = req.id.clone();

    if id.is_empty() {
        return Response::err(id, "INVALID_REQUEST", "request id must be non-empty");
    }

    match req.method.as_str() {
        "session.create" => {
            handle_session_create(
                id,
                state,
                event_tx,
                critical_event_tx,
                dropped_events,
                manager,
            )
            .await
        }
        "session.attach" => {
            handle_session_attach(
                id,
                req.params,
                state,
                event_tx,
                critical_event_tx,
                dropped_events,
                manager,
            )
            .await
        }
        "session.destroy" => handle_session_destroy(id, state, manager).await,
        "session.info" => handle_session_info(id, state, manager).await,
        "session.list" => handle_session_list(id, manager),

        "endpoint.create_from_offer" => {
            handle_endpoint_create_from_offer(id, req.params, state, manager).await
        }
        "endpoint.create_offer" => handle_endpoint_create_offer(id, req.params, state).await,
        "endpoint.accept_answer" => {
            handle_endpoint_accept_answer(id, req.params, state, manager).await
        }
        "endpoint.remove" => handle_endpoint_remove(id, req.params, state).await,

        "endpoint.dtmf.inject" => handle_dtmf_inject(id, req.params, state).await,

        "recording.start" => handle_recording_start(id, req.params, state, manager).await,
        "recording.stop" => handle_recording_stop(id, req.params, state).await,
        "vad.start" => handle_vad_start(id, req.params, state).await,
        "vad.stop" => handle_vad_stop(id, req.params, state).await,

        "endpoint.create_with_file" => handle_create_with_file(id, req.params, state).await,
        "endpoint.file.seek" => handle_file_seek(id, req.params, state).await,
        "endpoint.file.pause" => handle_file_pause(id, req.params, state).await,
        "endpoint.file.resume" => handle_file_resume(id, req.params, state).await,

        "endpoint.create_tone" => handle_create_tone(id, req.params, state).await,

        "endpoint.ice_restart" => handle_ice_restart(id, req.params, state).await,
        "endpoint.srtp_rekey" => handle_srtp_rekey(id, req.params, state).await,

        "stats.subscribe" => handle_stats_subscribe(id, req.params, state).await,
        "stats.unsubscribe" => handle_stats_unsubscribe(id, state).await,

        "endpoint.transfer" => handle_endpoint_transfer(id, req.params, state, manager).await,
        "session.bridge" => handle_session_bridge(id, req.params, state, manager).await,

        _ => Response::err(
            id,
            "UNKNOWN_METHOD",
            format!("Unknown method: {}", req.method),
        ),
    }
}

async fn handle_session_create(
    id: String,
    state: &mut ConnectionState,
    event_tx: &mpsc::Sender<Event>,
    critical_event_tx: &mpsc::Sender<Event>,
    dropped_events: &Arc<AtomicU64>,
    manager: &Arc<SessionManager>,
) -> Response {
    if manager.is_shutting_down() {
        return Response::err(id, "SHUTTING_DOWN", "Server is shutting down");
    }

    if state.session_id.is_some() {
        return Response::err(
            id,
            "SESSION_ALREADY_BOUND",
            "This connection already has a session",
        );
    }

    let session_id = match manager.create_session() {
        Ok(id) => id,
        Err(code) => return Response::err(id, code, "Cannot create session"),
    };

    // Attach via direct command since session starts in Active state
    let cmd_tx = match manager.get_session_cmd_tx(&session_id) {
        Some(tx) => tx,
        None => {
            // Session task died immediately after creation
            return Response::err(id, "SESSION_GONE", "Session task ended unexpectedly");
        }
    };

    match tokio::time::timeout(
        CMD_TIMEOUT,
        cmd_tx.send(SessionCommand::Attach {
            event_tx: event_tx.clone(),
            critical_event_tx: critical_event_tx.clone(),
            dropped_events: Arc::clone(dropped_events),
        }),
    )
    .await
    {
        Ok(Ok(())) => {}
        Ok(Err(_)) => {
            let _ = manager.destroy_session(&session_id);
            return Response::err(id, "SESSION_GONE", "Session task ended unexpectedly");
        }
        Err(_) => {
            let _ = manager.destroy_session(&session_id);
            return Response::err(
                id,
                "SESSION_UNRESPONSIVE",
                "Session did not accept command within timeout",
            );
        }
    }

    state.session_id = Some(session_id);
    state.cmd_tx = Some(cmd_tx);

    Response::ok(id, SessionCreateResult { session_id })
}

async fn handle_session_attach(
    id: String,
    params: serde_json::Value,
    state: &mut ConnectionState,
    event_tx: &mpsc::Sender<Event>,
    critical_event_tx: &mpsc::Sender<Event>,
    dropped_events: &Arc<AtomicU64>,
    manager: &Arc<SessionManager>,
) -> Response {
    if state.session_id.is_some() {
        return Response::err(
            id,
            "SESSION_ALREADY_BOUND",
            "Connection already has a session",
        );
    }

    let params: SessionAttachParams = match serde_json::from_value(params) {
        Ok(p) => p,
        Err(e) => return Response::err(id, "INVALID_PARAMS", e.to_string()),
    };

    match manager.attach_session(
        &params.session_id,
        event_tx.clone(),
        critical_event_tx.clone(),
        Arc::clone(dropped_events),
    ) {
        Ok(cmd_tx) => {
            state.session_id = Some(params.session_id);
            state.cmd_tx = Some(cmd_tx);
            Response::ok(id, serde_json::json!({}))
        }
        Err(code) => Response::err(id, code, format!("Cannot attach: {code}")),
    }
}

async fn handle_session_destroy(
    id: String,
    state: &mut ConnectionState,
    manager: &Arc<SessionManager>,
) -> Response {
    let session_id = match state.session_id.take() {
        Some(sid) => sid,
        None => return Response::err(id, "NO_SESSION", "No session bound"),
    };
    state.cmd_tx = None;

    match manager.destroy_session(&session_id) {
        Ok(()) => Response::ok(id, serde_json::json!({})),
        Err(code) => Response::err(id, code, "Failed to destroy session"),
    }
}

async fn handle_session_info(
    id: String,
    state: &ConnectionState,
    manager: &Arc<SessionManager>,
) -> Response {
    let session_id = match &state.session_id {
        Some(sid) => *sid,
        None => return Response::err(id, "NO_SESSION", "No session bound"),
    };

    match manager.get_session_details(&session_id).await {
        Some((sess_state, created_at, details)) => Response::ok(
            id,
            SessionInfoResult {
                session_id,
                state: sess_state,
                created_at,
                endpoints: details.endpoints,
                recordings: details.recordings,
                vad_active: details.vad_active,
            },
        ),
        None => Response::err(id, "SESSION_GONE", "Session no longer exists"),
    }
}

fn handle_session_list(id: String, manager: &Arc<SessionManager>) -> Response {
    Response::ok(
        id,
        SessionListResult {
            sessions: manager.list_sessions(),
        },
    )
}

// ── Endpoint handlers ───────────────────────────────────────────────────

async fn handle_endpoint_create_from_offer(
    id: String,
    params: serde_json::Value,
    state: &mut ConnectionState,
    manager: &Arc<SessionManager>,
) -> Response {
    let cmd_tx = match state.require_session(&id) {
        Ok(tx) => tx.clone(),
        Err(resp) => return resp,
    };

    let params: EndpointCreateFromOfferParams = match serde_json::from_value(params) {
        Ok(p) => p,
        Err(e) => return Response::err(id, "INVALID_PARAMS", e.to_string()),
    };

    // Reject oversized SDP to prevent memory exhaustion
    let max_sdp_size = manager.max_sdp_size();
    if params.sdp.len() > max_sdp_size {
        return Response::err(id, "INVALID_PARAMS", "SDP too large");
    }

    let (reply_tx, reply_rx) = oneshot::channel();
    match send_and_recv(
        &cmd_tx,
        SessionCommand::CreateFromOffer {
            reply: reply_tx,
            sdp: params.sdp,
            direction: params.direction,
        },
        reply_rx,
        &id,
    )
    .await
    {
        Ok(Ok((endpoint_id, sdp_answer))) => Response::ok(
            id,
            EndpointCreateFromOfferResult {
                endpoint_id,
                sdp_answer,
            },
        ),
        Ok(Err(e)) => Response::err(id, "ENDPOINT_ERROR", e.to_string()),
        Err(resp) => resp,
    }
}

async fn handle_endpoint_create_offer(
    id: String,
    params: serde_json::Value,
    state: &mut ConnectionState,
) -> Response {
    let cmd_tx = match state.require_session(&id) {
        Ok(tx) => tx.clone(),
        Err(resp) => return resp,
    };

    let params: EndpointCreateOfferParams = match serde_json::from_value(params) {
        Ok(p) => p,
        Err(e) => return Response::err(id, "INVALID_PARAMS", e.to_string()),
    };

    let (reply_tx, reply_rx) = oneshot::channel();
    match send_and_recv(
        &cmd_tx,
        SessionCommand::CreateOffer {
            reply: reply_tx,
            direction: params.direction,
            endpoint_type: params.endpoint_type,
            srtp: params.srtp,
            codecs: params.codecs,
        },
        reply_rx,
        &id,
    )
    .await
    {
        Ok(Ok((endpoint_id, sdp_offer))) => Response::ok(
            id,
            EndpointCreateOfferResult {
                endpoint_id,
                sdp_offer,
            },
        ),
        Ok(Err(e)) => Response::err(id, "ENDPOINT_ERROR", e.to_string()),
        Err(resp) => resp,
    }
}

async fn handle_endpoint_accept_answer(
    id: String,
    params: serde_json::Value,
    state: &mut ConnectionState,
    manager: &Arc<SessionManager>,
) -> Response {
    let cmd_tx = match state.require_session(&id) {
        Ok(tx) => tx.clone(),
        Err(resp) => return resp,
    };

    let params: EndpointAcceptAnswerParams = match serde_json::from_value(params) {
        Ok(p) => p,
        Err(e) => return Response::err(id, "INVALID_PARAMS", e.to_string()),
    };

    let max_sdp_size = manager.max_sdp_size();
    if params.sdp.len() > max_sdp_size {
        return Response::err(id, "INVALID_PARAMS", "SDP too large");
    }

    let (reply_tx, reply_rx) = oneshot::channel();
    match send_and_recv(
        &cmd_tx,
        SessionCommand::AcceptAnswer {
            reply: reply_tx,
            endpoint_id: params.endpoint_id,
            sdp: params.sdp,
        },
        reply_rx,
        &id,
    )
    .await
    {
        Ok(Ok(())) => Response::ok(id, serde_json::json!({})),
        Ok(Err(e)) => Response::err(id, "ENDPOINT_ERROR", e.to_string()),
        Err(resp) => resp,
    }
}

async fn handle_endpoint_remove(
    id: String,
    params: serde_json::Value,
    state: &mut ConnectionState,
) -> Response {
    let cmd_tx = match state.require_session(&id) {
        Ok(tx) => tx.clone(),
        Err(resp) => return resp,
    };

    let params: EndpointRemoveParams = match serde_json::from_value(params) {
        Ok(p) => p,
        Err(e) => return Response::err(id, "INVALID_PARAMS", e.to_string()),
    };

    let (reply_tx, reply_rx) = oneshot::channel();
    match send_and_recv(
        &cmd_tx,
        SessionCommand::RemoveEndpoint {
            reply: reply_tx,
            endpoint_id: params.endpoint_id,
        },
        reply_rx,
        &id,
    )
    .await
    {
        Ok(Ok(())) => Response::ok(id, serde_json::json!({})),
        Ok(Err(e)) => Response::err(id, "ENDPOINT_ERROR", e.to_string()),
        Err(resp) => resp,
    }
}

async fn handle_dtmf_inject(
    id: String,
    params: serde_json::Value,
    state: &mut ConnectionState,
) -> Response {
    let cmd_tx = match state.require_session(&id) {
        Ok(tx) => tx.clone(),
        Err(resp) => return resp,
    };

    let params: DtmfInjectParams = match serde_json::from_value(params) {
        Ok(p) => p,
        Err(e) => return Response::err(id, "INVALID_PARAMS", e.to_string()),
    };

    if params.digit.chars().count() != 1 {
        return Response::err(
            id,
            "INVALID_PARAMS",
            "digit must be exactly one character (0-9, *, #, A-D)",
        );
    }

    // Cap duration to prevent DoS — 10 seconds is far beyond any real DTMF use case
    const MAX_DTMF_DURATION_MS: u32 = 10_000;
    if params.duration_ms == 0 {
        return Response::err(id, "INVALID_PARAMS", "duration_ms must be > 0");
    }
    if params.duration_ms > MAX_DTMF_DURATION_MS {
        return Response::err(
            id,
            "INVALID_PARAMS",
            format!("duration_ms must not exceed {MAX_DTMF_DURATION_MS}"),
        );
    }

    let digit = match params.digit.chars().next() {
        Some(d) if matches!(d, '0'..='9' | '*' | '#' | 'A'..='D' | 'a'..='d') => {
            d.to_ascii_uppercase()
        }
        _ => {
            return Response::err(id, "INVALID_PARAMS", "digit must be one of 0-9, *, #, A-D");
        }
    };

    if params.volume > 63 {
        return Response::err(
            id,
            "INVALID_PARAMS",
            "volume must be 0-63 (RFC 4733 6-bit field)",
        );
    }

    let (reply_tx, reply_rx) = oneshot::channel();
    match send_and_recv(
        &cmd_tx,
        SessionCommand::DtmfInject {
            reply: reply_tx,
            endpoint_id: params.endpoint_id,
            digit,
            duration_ms: params.duration_ms,
            volume: params.volume,
        },
        reply_rx,
        &id,
    )
    .await
    {
        Ok(Ok(())) => Response::ok(id, serde_json::json!({})),
        Ok(Err(e)) => Response::err(id, "ENDPOINT_ERROR", e.to_string()),
        Err(resp) => resp,
    }
}

/// Validate a recording file path: must be within the configured recording_dir.
///
/// Returns the resolved (canonicalized) path to use for file creation. This
/// eliminates the TOCTOU race between validation and file creation — the
/// caller uses the resolved path directly, so a symlink swap after this
/// check cannot redirect the file outside recording_dir.
fn validate_recording_path(file_path: &str, recording_dir: &Path) -> Result<String, String> {
    let path = Path::new(file_path);

    if !path.is_absolute() {
        return Err("recording file_path must be absolute".into());
    }

    // Reject any path containing ".." components to prevent traversal
    for component in path.components() {
        if matches!(component, std::path::Component::ParentDir) {
            return Err("recording file_path must not contain '..' components".into());
        }
    }

    // Resolve symlinks in the parent directory to prevent symlink escapes.
    // The file itself doesn't exist yet, but the parent must exist for
    // File::create to succeed — so we canonicalize the parent and
    // re-attach the filename.
    let canonical_dir = recording_dir.canonicalize().map_err(|e| {
        format!(
            "cannot resolve recording_dir ({}): {e}",
            recording_dir.display()
        )
    })?;

    let parent = path
        .parent()
        .ok_or_else(|| "file_path has no parent directory".to_string())?;
    let filename = path
        .file_name()
        .ok_or_else(|| "file_path has no filename".to_string())?;

    let resolved_parent = parent.canonicalize().map_err(|e| {
        format!(
            "cannot resolve parent directory '{}': {e}",
            parent.display()
        )
    })?;

    let resolved_path = resolved_parent.join(filename);

    if !resolved_path.starts_with(&canonical_dir) {
        return Err(format!(
            "recording file_path resolves outside recording_dir ({})",
            recording_dir.display()
        ));
    }

    resolved_path
        .to_str()
        .ok_or_else(|| "resolved path is not valid UTF-8".to_string())
        .map(|s| s.to_string())
}

async fn handle_recording_start(
    id: String,
    params: serde_json::Value,
    state: &mut ConnectionState,
    manager: &Arc<SessionManager>,
) -> Response {
    let cmd_tx = match state.require_session(&id) {
        Ok(tx) => tx.clone(),
        Err(resp) => return resp,
    };

    let params: RecordingStartParams = match serde_json::from_value(params) {
        Ok(p) => p,
        Err(e) => return Response::err(id, "INVALID_PARAMS", e.to_string()),
    };

    // Validate the recording path and get the resolved (canonicalized) path.
    // Using the resolved path for file creation eliminates the TOCTOU race
    // between this check and the actual File::create_new in the session task.
    let resolved_path = match validate_recording_path(&params.file_path, manager.recording_dir()) {
        Ok(p) => p,
        Err(msg) => return Response::err(id, "INVALID_PARAMS", msg),
    };

    let (reply_tx, reply_rx) = oneshot::channel();
    match send_and_recv(
        &cmd_tx,
        SessionCommand::RecordingStart {
            reply: reply_tx,
            endpoint_id: params.endpoint_id,
            file_path: resolved_path,
        },
        reply_rx,
        &id,
    )
    .await
    {
        Ok(Ok(recording_id)) => Response::ok(id, RecordingStartResult { recording_id }),
        Ok(Err(e)) => Response::err(id, "RECORDING_ERROR", e.to_string()),
        Err(resp) => resp,
    }
}

async fn handle_recording_stop(
    id: String,
    params: serde_json::Value,
    state: &mut ConnectionState,
) -> Response {
    let cmd_tx = match state.require_session(&id) {
        Ok(tx) => tx.clone(),
        Err(resp) => return resp,
    };

    let params: RecordingStopParams = match serde_json::from_value(params) {
        Ok(p) => p,
        Err(e) => return Response::err(id, "INVALID_PARAMS", e.to_string()),
    };

    let (reply_tx, reply_rx) = oneshot::channel();
    match send_and_recv(
        &cmd_tx,
        SessionCommand::RecordingStop {
            reply: reply_tx,
            recording_id: params.recording_id,
        },
        reply_rx,
        &id,
    )
    .await
    {
        Ok(Ok((file_path, duration_ms, packets, dropped_packets))) => Response::ok(
            id,
            RecordingStopResult {
                file_path,
                duration_ms,
                packets,
                dropped_packets,
            },
        ),
        Ok(Err(e)) => Response::err(id, "RECORDING_ERROR", e.to_string()),
        Err(resp) => resp,
    }
}

async fn handle_vad_start(
    id: String,
    params: serde_json::Value,
    state: &mut ConnectionState,
) -> Response {
    let cmd_tx = match state.require_session(&id) {
        Ok(tx) => tx.clone(),
        Err(resp) => return resp,
    };

    let params: VadStartParams = match serde_json::from_value(params) {
        Ok(p) => p,
        Err(e) => return Response::err(id, "INVALID_PARAMS", e.to_string()),
    };

    if params.speech_threshold.is_nan()
        || params.speech_threshold < 0.0
        || params.speech_threshold > 1.0
    {
        return Response::err(id, "INVALID_PARAMS", "speech_threshold must be 0.0-1.0");
    }
    if params.silence_interval_ms < 100 {
        return Response::err(id, "INVALID_PARAMS", "silence_interval_ms must be >= 100");
    }
    if params.silence_interval_ms > 3_600_000 {
        return Response::err(
            id,
            "INVALID_PARAMS",
            "silence_interval_ms must be <= 3600000 (1 hour)",
        );
    }

    let (reply_tx, reply_rx) = oneshot::channel();
    match send_and_recv(
        &cmd_tx,
        SessionCommand::VadStart {
            reply: reply_tx,
            endpoint_id: params.endpoint_id,
            silence_interval_ms: params.silence_interval_ms,
            speech_threshold: params.speech_threshold,
        },
        reply_rx,
        &id,
    )
    .await
    {
        Ok(Ok(())) => Response::ok(id, serde_json::json!({})),
        Ok(Err(e)) => Response::err(id, "VAD_ERROR", e.to_string()),
        Err(resp) => resp,
    }
}

async fn handle_vad_stop(
    id: String,
    params: serde_json::Value,
    state: &mut ConnectionState,
) -> Response {
    let cmd_tx = match state.require_session(&id) {
        Ok(tx) => tx.clone(),
        Err(resp) => return resp,
    };

    let params: VadStopParams = match serde_json::from_value(params) {
        Ok(p) => p,
        Err(e) => return Response::err(id, "INVALID_PARAMS", e.to_string()),
    };

    let (reply_tx, reply_rx) = oneshot::channel();
    match send_and_recv(
        &cmd_tx,
        SessionCommand::VadStop {
            reply: reply_tx,
            endpoint_id: params.endpoint_id,
        },
        reply_rx,
        &id,
    )
    .await
    {
        Ok(Ok(())) => Response::ok(id, serde_json::json!({})),
        Ok(Err(e)) => Response::err(id, "VAD_ERROR", e.to_string()),
        Err(resp) => resp,
    }
}

async fn handle_create_with_file(
    id: String,
    params: serde_json::Value,
    state: &mut ConnectionState,
) -> Response {
    let cmd_tx = match state.require_session(&id) {
        Ok(tx) => tx.clone(),
        Err(resp) => return resp,
    };

    let params: EndpointCreateWithFileParams = match serde_json::from_value(params) {
        Ok(p) => p,
        Err(e) => return Response::err(id, "INVALID_PARAMS", e.to_string()),
    };

    if params.timeout_ms == 0 {
        return Response::err(id, "INVALID_PARAMS", "timeout_ms must be > 0");
    }
    if params.timeout_ms > 60_000 {
        return Response::err(
            id,
            "INVALID_PARAMS",
            "timeout_ms must be at most 60000 (1 minute)",
        );
    }
    if let Some(count) = params.loop_count {
        if count > 10_000 {
            return Response::err(id, "INVALID_PARAMS", "loop_count must be at most 10000");
        }
    }

    let (reply_tx, reply_rx) = oneshot::channel();
    match send_and_recv(
        &cmd_tx,
        SessionCommand::CreateWithFile {
            reply: reply_tx,
            source: params.source,
            start_ms: params.start_ms,
            loop_count: params.loop_count,
            cache_ttl_secs: params.cache_ttl_secs,
            timeout_ms: params.timeout_ms,
            shared: params.shared,
        },
        reply_rx,
        &id,
    )
    .await
    {
        Ok(Ok(endpoint_id)) => Response::ok(id, EndpointCreateWithFileResult { endpoint_id }),
        Ok(Err(e)) => Response::err(id, "ENDPOINT_ERROR", e.to_string()),
        Err(resp) => resp,
    }
}

async fn handle_create_tone(
    id: String,
    params: serde_json::Value,
    state: &mut ConnectionState,
) -> Response {
    let cmd_tx = match state.require_session(&id) {
        Ok(tx) => tx.clone(),
        Err(resp) => return resp,
    };

    let params: EndpointCreateToneParams = match serde_json::from_value(params) {
        Ok(p) => p,
        Err(e) => return Response::err(id, "INVALID_PARAMS", e.to_string()),
    };

    if let Some(freq) = params.frequency {
        if !(20.0..=20000.0).contains(&freq) {
            return Response::err(id, "INVALID_PARAMS", "frequency must be 20-20000 Hz");
        }
    }

    let tone_type = params.tone;
    let (reply_tx, reply_rx) = oneshot::channel();
    match send_and_recv(
        &cmd_tx,
        SessionCommand::CreateWithTone {
            reply: reply_tx,
            tone_type: params.tone,
            frequency: params.frequency,
            duration_ms: params.duration_ms,
        },
        reply_rx,
        &id,
    )
    .await
    {
        Ok(Ok(endpoint_id)) => Response::ok(
            id,
            EndpointCreateToneResult {
                endpoint_id,
                tone: tone_type,
            },
        ),
        Ok(Err(e)) => Response::err(id, "ENDPOINT_ERROR", e.to_string()),
        Err(resp) => resp,
    }
}

async fn handle_file_seek(
    id: String,
    params: serde_json::Value,
    state: &mut ConnectionState,
) -> Response {
    let cmd_tx = match state.require_session(&id) {
        Ok(tx) => tx.clone(),
        Err(resp) => return resp,
    };

    let params: FileSeekParams = match serde_json::from_value(params) {
        Ok(p) => p,
        Err(e) => return Response::err(id, "INVALID_PARAMS", e.to_string()),
    };

    let (reply_tx, reply_rx) = oneshot::channel();
    match send_and_recv(
        &cmd_tx,
        SessionCommand::FileSeek {
            reply: reply_tx,
            endpoint_id: params.endpoint_id,
            position_ms: params.position_ms,
        },
        reply_rx,
        &id,
    )
    .await
    {
        Ok(Ok(())) => Response::ok(id, serde_json::json!({})),
        Ok(Err(e)) => Response::err(id, "ENDPOINT_ERROR", e.to_string()),
        Err(resp) => resp,
    }
}

async fn handle_file_pause(
    id: String,
    params: serde_json::Value,
    state: &mut ConnectionState,
) -> Response {
    let cmd_tx = match state.require_session(&id) {
        Ok(tx) => tx.clone(),
        Err(resp) => return resp,
    };

    let params: FilePauseParams = match serde_json::from_value(params) {
        Ok(p) => p,
        Err(e) => return Response::err(id, "INVALID_PARAMS", e.to_string()),
    };

    let (reply_tx, reply_rx) = oneshot::channel();
    match send_and_recv(
        &cmd_tx,
        SessionCommand::FilePause {
            reply: reply_tx,
            endpoint_id: params.endpoint_id,
        },
        reply_rx,
        &id,
    )
    .await
    {
        Ok(Ok(())) => Response::ok(id, serde_json::json!({})),
        Ok(Err(e)) => Response::err(id, "ENDPOINT_ERROR", e.to_string()),
        Err(resp) => resp,
    }
}

async fn handle_file_resume(
    id: String,
    params: serde_json::Value,
    state: &mut ConnectionState,
) -> Response {
    let cmd_tx = match state.require_session(&id) {
        Ok(tx) => tx.clone(),
        Err(resp) => return resp,
    };

    let params: FileResumeParams = match serde_json::from_value(params) {
        Ok(p) => p,
        Err(e) => return Response::err(id, "INVALID_PARAMS", e.to_string()),
    };

    let (reply_tx, reply_rx) = oneshot::channel();
    match send_and_recv(
        &cmd_tx,
        SessionCommand::FileResume {
            reply: reply_tx,
            endpoint_id: params.endpoint_id,
        },
        reply_rx,
        &id,
    )
    .await
    {
        Ok(Ok(())) => Response::ok(id, serde_json::json!({})),
        Ok(Err(e)) => Response::err(id, "ENDPOINT_ERROR", e.to_string()),
        Err(resp) => resp,
    }
}

async fn handle_ice_restart(
    id: String,
    params: serde_json::Value,
    state: &mut ConnectionState,
) -> Response {
    let cmd_tx = match state.require_session(&id) {
        Ok(tx) => tx.clone(),
        Err(resp) => return resp,
    };

    let params: EndpointIceRestartParams = match serde_json::from_value(params) {
        Ok(p) => p,
        Err(e) => return Response::err(id, "INVALID_PARAMS", e.to_string()),
    };

    let (reply_tx, reply_rx) = oneshot::channel();
    match send_and_recv(
        &cmd_tx,
        SessionCommand::IceRestart {
            reply: reply_tx,
            endpoint_id: params.endpoint_id,
        },
        reply_rx,
        &id,
    )
    .await
    {
        Ok(Ok(sdp_offer)) => Response::ok(id, EndpointIceRestartResult { sdp_offer }),
        Ok(Err(e)) => Response::err(id, "ENDPOINT_ERROR", e.to_string()),
        Err(resp) => resp,
    }
}

async fn handle_stats_subscribe(
    id: String,
    params: serde_json::Value,
    state: &mut ConnectionState,
) -> Response {
    let cmd_tx = match state.require_session(&id) {
        Ok(tx) => tx.clone(),
        Err(resp) => return resp,
    };

    let params: StatsSubscribeParams = match serde_json::from_value(params) {
        Ok(p) => p,
        Err(e) => return Response::err(id, "INVALID_PARAMS", e.to_string()),
    };

    if params.interval_ms < 500 {
        return Response::err(id, "INVALID_PARAMS", "interval_ms must be at least 500");
    }
    if params.interval_ms > 3_600_000 {
        return Response::err(
            id,
            "INVALID_PARAMS",
            "interval_ms must be at most 3600000 (1 hour)",
        );
    }

    let (reply_tx, reply_rx) = oneshot::channel();
    match send_and_recv(
        &cmd_tx,
        SessionCommand::StatsSubscribe {
            reply: reply_tx,
            interval_ms: params.interval_ms,
        },
        reply_rx,
        &id,
    )
    .await
    {
        Ok(Ok(())) => Response::ok(id, serde_json::json!({})),
        Ok(Err(e)) => Response::err(id, "STATS_ERROR", e.to_string()),
        Err(resp) => resp,
    }
}

async fn handle_stats_unsubscribe(id: String, state: &mut ConnectionState) -> Response {
    let cmd_tx = match state.require_session(&id) {
        Ok(tx) => tx.clone(),
        Err(resp) => return resp,
    };

    let (reply_tx, reply_rx) = oneshot::channel();
    match send_and_recv(
        &cmd_tx,
        SessionCommand::StatsUnsubscribe { reply: reply_tx },
        reply_rx,
        &id,
    )
    .await
    {
        Ok(Ok(())) => Response::ok(id, serde_json::json!({})),
        Ok(Err(e)) => Response::err(id, "STATS_ERROR", e.to_string()),
        Err(resp) => resp,
    }
}

async fn handle_srtp_rekey(
    id: String,
    params: serde_json::Value,
    state: &mut ConnectionState,
) -> Response {
    let cmd_tx = match state.require_session(&id) {
        Ok(tx) => tx.clone(),
        Err(resp) => return resp,
    };

    let params: EndpointSrtpRekeyParams = match serde_json::from_value(params) {
        Ok(p) => p,
        Err(e) => return Response::err(id, "INVALID_PARAMS", e.to_string()),
    };

    let (reply_tx, reply_rx) = oneshot::channel();
    match send_and_recv(
        &cmd_tx,
        SessionCommand::SrtpRekey {
            reply: reply_tx,
            endpoint_id: params.endpoint_id,
        },
        reply_rx,
        &id,
    )
    .await
    {
        Ok(Ok(sdp)) => Response::ok(id, EndpointSrtpRekeyResult { sdp }),
        Ok(Err(e)) => Response::err(id, "ENDPOINT_ERROR", e.to_string()),
        Err(resp) => resp,
    }
}

// ── Endpoint Transfer ───────────────────────────────────────────────────

async fn handle_endpoint_transfer(
    id: String,
    params: serde_json::Value,
    state: &mut ConnectionState,
    manager: &Arc<SessionManager>,
) -> Response {
    let source_cmd_tx = match state.require_session(&id) {
        Ok(tx) => tx.clone(),
        Err(resp) => return resp,
    };
    let source_session_id = state.session_id.unwrap();

    let params: EndpointTransferParams = match serde_json::from_value(params) {
        Ok(p) => p,
        Err(e) => return Response::err(id, "INVALID_PARAMS", e.to_string()),
    };

    if params.target_session_id == source_session_id {
        return Response::err(id, "INVALID_PARAMS", "Cannot transfer to the same session");
    }

    let target_cmd_tx = match manager.get_session_cmd_tx(&params.target_session_id) {
        Some(tx) => tx,
        None => return Response::err(id, "SESSION_NOT_FOUND", "Target session not found"),
    };

    // Extract from source
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    let bundle = match send_and_recv(
        &source_cmd_tx,
        SessionCommand::ExtractEndpoint {
            reply: reply_tx,
            endpoint_id: params.endpoint_id,
        },
        reply_rx,
        &id,
    )
    .await
    {
        Ok(Ok(bundle)) => bundle,
        Ok(Err(e)) => return Response::err(id, "ENDPOINT_ERROR", e.to_string()),
        Err(resp) => return resp,
    };

    // Insert into target
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    match send_and_recv(
        &target_cmd_tx,
        SessionCommand::InsertEndpoint {
            reply: reply_tx,
            bundle: Box::new(bundle),
        },
        reply_rx,
        &id,
    )
    .await
    {
        Ok(Ok(())) => Response::ok(
            id,
            EndpointTransferResult {
                endpoint_id: params.endpoint_id,
                target_session_id: params.target_session_id,
            },
        ),
        Ok(Err((e, bundle))) => {
            // Rollback: re-insert into source
            let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
            let _ = send_and_recv(
                &source_cmd_tx,
                SessionCommand::InsertEndpoint {
                    reply: reply_tx,
                    bundle: Box::new(bundle),
                },
                reply_rx,
                &id,
            )
            .await;
            Response::err(id, "TRANSFER_FAILED", e.to_string())
        }
        Err(resp) => resp,
    }
}

// ── Session Bridge ──────────────────────────────────────────────────────

async fn handle_session_bridge(
    id: String,
    params: serde_json::Value,
    state: &mut ConnectionState,
    manager: &Arc<SessionManager>,
) -> Response {
    let source_cmd_tx = match state.require_session(&id) {
        Ok(tx) => tx.clone(),
        Err(resp) => return resp,
    };
    let source_session_id = state.session_id.unwrap();

    let params: SessionBridgeParams = match serde_json::from_value(params) {
        Ok(p) => p,
        Err(e) => return Response::err(id, "INVALID_PARAMS", e.to_string()),
    };

    if params.target_session_id == source_session_id {
        return Response::err(id, "INVALID_PARAMS", "Cannot bridge to the same session");
    }

    let target_cmd_tx = match manager.get_session_cmd_tx(&params.target_session_id) {
        Some(tx) => tx,
        None => return Response::err(id, "SESSION_NOT_FOUND", "Target session not found"),
    };

    // Get packet_tx from both sessions
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    let source_packet_tx = match send_and_recv(
        &source_cmd_tx,
        SessionCommand::GetPacketTx { reply: reply_tx },
        reply_rx,
        &id,
    )
    .await
    {
        Ok(tx) => tx,
        Err(resp) => return resp,
    };

    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    let target_packet_tx = match send_and_recv(
        &target_cmd_tx,
        SessionCommand::GetPacketTx { reply: reply_tx },
        reply_rx,
        &id,
    )
    .await
    {
        Ok(tx) => tx,
        Err(resp) => return resp,
    };

    // Generate endpoint IDs
    let ep_a_id = EndpointId::new_v4();
    let ep_b_id = EndpointId::new_v4();

    // Create bridge endpoints
    use crate::session::endpoint_bridge::BridgeEndpoint;

    let bridge_a = BridgeEndpoint::new(
        ep_a_id,
        params.direction,
        target_packet_tx,
        ep_b_id,
        params.target_session_id,
        target_cmd_tx.clone(),
    );

    let bridge_b = BridgeEndpoint::new(
        ep_b_id,
        params.direction,
        source_packet_tx,
        ep_a_id,
        source_session_id,
        source_cmd_tx.clone(),
    );

    // Insert bridge_a into source session
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    match send_and_recv(
        &source_cmd_tx,
        SessionCommand::InsertBridgeEndpoint {
            reply: reply_tx,
            bridge: bridge_a,
        },
        reply_rx,
        &id,
    )
    .await
    {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => return Response::err(id, "ENDPOINT_ERROR", e.to_string()),
        Err(resp) => return resp,
    }

    // Insert bridge_b into target session
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    match send_and_recv(
        &target_cmd_tx,
        SessionCommand::InsertBridgeEndpoint {
            reply: reply_tx,
            bridge: bridge_b,
        },
        reply_rx,
        &id,
    )
    .await
    {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => {
            // Rollback: remove bridge_a from source
            let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
            let _ = send_and_recv(
                &source_cmd_tx,
                SessionCommand::RemoveEndpoint {
                    reply: reply_tx,
                    endpoint_id: ep_a_id,
                },
                reply_rx,
                &id,
            )
            .await;
            return Response::err(id, "ENDPOINT_ERROR", e.to_string());
        }
        Err(resp) => return resp,
    }

    Response::ok(
        id,
        SessionBridgeResult {
            endpoint_id: ep_a_id,
            target_endpoint_id: ep_b_id,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Create a temp dir under /tmp for recording path tests
    fn setup_rec_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join("rtpbridge-recpath-test");
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn test_validate_recording_path_within_dir_accepted() {
        let rec_dir = setup_rec_dir();
        let sub = rec_dir.join("sub");
        std::fs::create_dir_all(&sub).unwrap();

        let file_in_root = rec_dir.join("recording.pcap");
        assert!(validate_recording_path(file_in_root.to_str().unwrap(), &rec_dir).is_ok());

        let file_in_sub = sub.join("call.pcap");
        assert!(validate_recording_path(file_in_sub.to_str().unwrap(), &rec_dir).is_ok());
    }

    #[test]
    fn test_validate_recording_path_outside_dir_rejected() {
        let rec_dir = setup_rec_dir();
        let result = validate_recording_path("/var/recordings/test.pcap", &rec_dir);
        let err = result.unwrap_err();
        assert!(
            err.contains("resolves outside recording_dir") || err.contains("cannot resolve"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_validate_recording_path_rejects_relative_paths() {
        let rec_dir = setup_rec_dir();
        let result = validate_recording_path("recordings/test.pcap", &rec_dir);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("must be absolute"));

        let result = validate_recording_path("test.pcap", &rec_dir);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("must be absolute"));
    }

    #[test]
    fn test_validate_recording_path_rejects_traversal() {
        let rec_dir = setup_rec_dir();
        let result = validate_recording_path("/tmp/../etc/crontab", &rec_dir);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("'..'"));
    }

    #[test]
    fn test_validate_recording_path_allows_dots_in_filenames() {
        let rec_dir = setup_rec_dir();
        let dotfile = rec_dir.join("my.recording.pcap");
        assert!(validate_recording_path(dotfile.to_str().unwrap(), &rec_dir).is_ok());
    }

    #[test]
    fn test_validate_recording_path_rejects_relative_traversal() {
        let rec_dir = setup_rec_dir();
        let result = validate_recording_path("../../../etc/crontab", &rec_dir);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("must be absolute"));
    }

    #[test]
    fn test_validate_recording_path_rejects_symlink_escape() {
        let rec_dir = setup_rec_dir();

        // Create a symlink inside rec_dir that points outside it
        let escape_target = std::env::temp_dir().join("rtpbridge-recpath-escape-target");
        std::fs::create_dir_all(&escape_target).unwrap();
        let symlink_path = rec_dir.join("escape_link");
        // Remove stale symlink if it exists
        let _ = std::fs::remove_file(&symlink_path);
        std::os::unix::fs::symlink(&escape_target, &symlink_path).unwrap();

        // A file through the symlink lexically looks like it's inside rec_dir,
        // but canonicalize reveals it's actually outside
        let escaped_file = symlink_path.join("pwned.pcap");
        let result = validate_recording_path(escaped_file.to_str().unwrap(), &rec_dir);
        assert!(
            result.is_err(),
            "symlink escaping rec_dir must be rejected, got: {:?}",
            result
        );
        assert!(
            result
                .unwrap_err()
                .contains("resolves outside recording_dir")
        );

        // Cleanup
        std::fs::remove_file(&symlink_path).ok();
        std::fs::remove_dir_all(&escape_target).ok();
    }

    #[test]
    fn test_validate_recording_path_rejects_prefix_overlap() {
        // /tmp-evil/... must not match /tmp as rec_dir
        let rec_dir = setup_rec_dir();
        let evil_dir = std::env::temp_dir().join("rtpbridge-recpath-test-evil");
        std::fs::create_dir_all(&evil_dir).unwrap();
        let evil_file = evil_dir.join("recording.pcap");
        let result = validate_recording_path(evil_file.to_str().unwrap(), &rec_dir);
        assert!(result.is_err(), "prefix overlap must not fool starts_with");
        std::fs::remove_dir_all(&evil_dir).ok();
    }

    #[test]
    fn test_validate_recording_path_nonexistent_parent_rejected() {
        let rec_dir = setup_rec_dir();
        let deep = rec_dir.join("no/such/dir/file.pcap");
        let result = validate_recording_path(deep.to_str().unwrap(), &rec_dir);
        assert!(
            result.is_err(),
            "nonexistent parent should fail canonicalize"
        );
        assert!(result.unwrap_err().contains("cannot resolve parent"));
    }

    #[test]
    fn test_validate_recording_path_returns_resolved_path() {
        // The returned path must be the canonicalized version, not the
        // user-supplied string. This closes the TOCTOU race: the caller
        // uses the resolved path for File::create_new, so a symlink swap
        // after validation cannot redirect the file outside recording_dir.
        let rec_dir = setup_rec_dir();
        let sub = rec_dir.join("resolved_sub");
        std::fs::create_dir_all(&sub).unwrap();

        let user_path = sub.join("test.pcap");
        let resolved = validate_recording_path(user_path.to_str().unwrap(), &rec_dir).unwrap();

        // The resolved path should be fully canonicalized (no symlinks,
        // no relative components). On macOS /tmp → /private/tmp, so
        // the resolved path may differ from the input.
        let expected = sub.canonicalize().unwrap().join("test.pcap");
        assert_eq!(
            resolved,
            expected.to_str().unwrap(),
            "returned path must be the canonicalized version"
        );
    }

    #[test]
    fn test_validate_recording_path_symlink_resolved_before_return() {
        // Create a symlink inside rec_dir that points to a subdir also
        // inside rec_dir. The returned path should follow the symlink
        // and return the canonical target — proving that the path used
        // for file creation won't follow a *later* symlink swap.
        let rec_dir = setup_rec_dir();
        let real_sub = rec_dir.join("real_target");
        std::fs::create_dir_all(&real_sub).unwrap();
        let link = rec_dir.join("valid_link");
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(&real_sub, &link).unwrap();

        let user_path = link.join("file.pcap");
        let resolved = validate_recording_path(user_path.to_str().unwrap(), &rec_dir).unwrap();

        // The resolved path should point through real_target, not through
        // the symlink — confirming the symlink was resolved at validation time.
        let canonical_target = real_sub.canonicalize().unwrap().join("file.pcap");
        assert_eq!(
            resolved,
            canonical_target.to_str().unwrap(),
            "symlink should be resolved at validation time, not deferred to file creation"
        );

        std::fs::remove_file(&link).ok();
        std::fs::remove_dir_all(&real_sub).ok();
    }
}
