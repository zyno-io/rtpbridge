use std::fs::File;
use std::io::BufReader;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use rustls::ServerConfig;
use rustls_pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsAcceptor;
use tokio_tungstenite::accept_async_with_config;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tracing::{debug, error, info};
use uuid::Uuid;

use super::auth::HmacAuthenticator;
use super::connection::handle_connection;
use super::transport::{BoxedServerIo, PrefixedIo};
use crate::config::TlsConfig;
use crate::metrics::Metrics;
use crate::session::SessionManager;
use crate::shutdown::ShutdownCoordinator;

/// Build the single TLS mode used by every configured control listener.
pub fn build_tls_acceptor(tls: &TlsConfig) -> anyhow::Result<TlsAcceptor> {
    let cert_file = File::open(&tls.cert_path)
        .with_context(|| format!("open TLS certificate {:?}", tls.cert_path))?;
    let mut cert_reader = BufReader::new(cert_file);
    let certificates = CertificateDer::pem_reader_iter(&mut cert_reader)
        .collect::<Result<Vec<_>, _>>()
        .context("parse TLS certificate PEM")?;
    if certificates.is_empty() {
        anyhow::bail!("TLS certificate PEM contains no certificates");
    }

    let key_file = File::open(&tls.key_path)
        .with_context(|| format!("open TLS private key {:?}", tls.key_path))?;
    let mut key_reader = BufReader::new(key_file);
    let private_key =
        PrivateKeyDer::from_pem_reader(&mut key_reader).context("parse TLS private-key PEM")?;

    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certificates, private_key)
        .context("build TLS server configuration")?;
    Ok(TlsAcceptor::from(Arc::new(config)))
}

#[allow(clippy::too_many_arguments)]
pub async fn run_websocket_server(
    listen_addrs: Vec<SocketAddr>,
    tls_config: Option<TlsConfig>,
    authenticator: Option<Arc<HmacAuthenticator>>,
    manager: Arc<SessionManager>,
    shutdown: ShutdownCoordinator,
    metrics: Arc<Metrics>,
    recording_dir: PathBuf,
    ws_max_message_size_kb: usize,
    max_connections: usize,
    max_recording_download_bytes: u64,
    ws_ping_interval_secs: u64,
    event_channel_size: usize,
    critical_event_channel_size: usize,
) -> anyhow::Result<()> {
    let tls_acceptor = tls_config.as_ref().map(build_tls_acceptor).transpose()?;
    let mut listeners = Vec::with_capacity(listen_addrs.len());
    for addr in &listen_addrs {
        let listener = TcpListener::bind(addr).await?;
        if tls_acceptor.is_some() {
            info!(addr = %addr, "Control server listening (WSS + HTTPS)");
        } else {
            info!(addr = %addr, "Control server listening (WS + HTTP)");
        }
        listeners.push(listener);
    }

    // Limit concurrent connections to prevent file descriptor / memory exhaustion.
    // Tokio semaphores have a max permits ceiling; 0 means unlimited.
    let max_connections = if max_connections == 0 {
        tokio::sync::Semaphore::MAX_PERMITS
    } else {
        max_connections
    };
    let connection_semaphore = Arc::new(tokio::sync::Semaphore::new(max_connections));
    let tls_enabled = tls_acceptor.is_some();

    loop {
        tokio::select! {
            result = accept_any(&listeners) => {
                let (mut stream, peer_addr) = match result {
                    Ok(v) => v,
                    Err(e) => {
                        error!(error = %e, "failed to accept connection");
                        continue;
                    }
                };

                if shutdown.is_shutting_down() {
                    if !tls_enabled {
                        let _ = tokio::time::timeout(
                            Duration::from_secs(1),
                            stream.write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"),
                        ).await;
                    }
                    drop(stream);
                    continue;
                }

                let permit = match connection_semaphore.clone().try_acquire_owned() {
                    Ok(permit) => permit,
                    Err(_) => {
                        debug!(peer = %peer_addr, "connection rejected: max connections reached");
                        if !tls_enabled {
                            let _ = tokio::time::timeout(
                                Duration::from_secs(1),
                                stream.write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"),
                            ).await;
                        }
                        drop(stream);
                        continue;
                    }
                };

                let manager = Arc::clone(&manager);
                let shutdown = shutdown.clone();
                let metrics = Arc::clone(&metrics);
                let recording_dir = recording_dir.clone();
                let ws_max_size = ws_max_message_size_kb;
                let tls_acceptor = tls_acceptor.clone();
                let authenticator = authenticator.clone();

                tokio::spawn(async move {
                    // The permit travels with the connection: for control/HTTP it drops
                    // when handle_incoming returns; for an audio WS it is handed to the
                    // endpoint's IO task and held for the audio socket's lifetime.
                    let stream = match accept_transport(stream, tls_acceptor).await {
                        Ok(stream) => stream,
                        Err(e) => {
                            debug!(peer = %peer_addr, error = %e, "control TLS handshake failed");
                            return;
                        }
                    };
                    handle_incoming(stream, peer_addr, authenticator, manager, shutdown, metrics, recording_dir, ws_max_size, max_recording_download_bytes, ws_ping_interval_secs, event_channel_size, critical_event_channel_size, permit).await;
                });
            }
            _ = shutdown.wait_for_shutdown() => {
                info!("Shutdown signal received, stopping server");
                break;
            }
        }
    }

    Ok(())
}

/// Accept a connection from any of the given listeners.
async fn accept_any(listeners: &[TcpListener]) -> std::io::Result<(TcpStream, SocketAddr)> {
    // We use poll_fn to fairly poll all listeners in a single future.
    std::future::poll_fn(|cx| {
        for listener in listeners {
            if let std::task::Poll::Ready(result) = listener.poll_accept(cx) {
                return std::task::Poll::Ready(result);
            }
        }
        std::task::Poll::Pending
    })
    .await
}

/// Upgrade a TCP stream when the listener is in TLS mode. The boxed transport
/// gives the following HTTP/WebSocket code one concrete stream type.
async fn accept_transport(
    stream: TcpStream,
    tls_acceptor: Option<TlsAcceptor>,
) -> anyhow::Result<BoxedServerIo> {
    match tls_acceptor {
        Some(acceptor) => {
            let stream = tokio::time::timeout(Duration::from_secs(10), acceptor.accept(stream))
                .await
                .context("TLS handshake timed out")??;
            Ok(Box::new(stream))
        }
        None => Ok(Box::new(stream)),
    }
}

/// Parsed HTTP request metadata used to authorize a request before handing a
/// WebSocket upgrade to tungstenite.
struct RequestHead {
    method: String,
    target: String,
    authorization: Option<String>,
    host: Option<String>,
    has_origin: bool,
    is_websocket_upgrade: bool,
}

/// Handle an incoming TLS or plaintext connection: HTTP REST or WebSocket.
#[allow(clippy::too_many_arguments)]
async fn handle_incoming(
    mut stream: BoxedServerIo,
    peer_addr: SocketAddr,
    authenticator: Option<Arc<HmacAuthenticator>>,
    manager: Arc<SessionManager>,
    shutdown: ShutdownCoordinator,
    metrics: Arc<Metrics>,
    recording_dir: PathBuf,
    ws_max_message_size_kb: usize,
    max_recording_download_bytes: u64,
    ws_ping_interval_secs: u64,
    event_channel_size: usize,
    critical_event_channel_size: usize,
    permit: tokio::sync::OwnedSemaphorePermit,
) {
    let prefetched =
        match tokio::time::timeout(Duration::from_secs(5), read_request_head(&mut stream)).await {
            Ok(Ok(value)) => value,
            Ok(Err(e)) => {
                debug!(peer = %peer_addr, error = %e, "request header read failed");
                return;
            }
            Err(_) => {
                debug!(peer = %peer_addr, "connection timed out during request-header read");
                return;
            }
        };
    let request = match parse_request_head(&prefetched) {
        Some(request) => request,
        None => {
            debug!(peer = %peer_addr, "invalid HTTP request header");
            return;
        }
    };

    let audio = classify_audio_path(&request.target);
    let privileged = if request.is_websocket_upgrade {
        audio.is_none()
    } else {
        request_requires_hmac(&request.target)
    };
    if privileged
        && authenticator.is_none()
        && (request.has_origin
            || peer_addr.ip().to_canonical().is_loopback()
                && !is_loopback_host(request.host.as_deref()))
    {
        let response = http_json_response(
            "403 Forbidden",
            r#"{"error":"browser control requires an authenticated backend"}"#,
        );
        let _ = tokio::time::timeout(Duration::from_secs(5), stream.write_all(&response)).await;
        return;
    }
    if !request.is_websocket_upgrade {
        if request_requires_hmac(&request.target)
            && !is_authorized(
                &authenticator,
                request.authorization.as_deref(),
                &request.method,
                &request.target,
            )
        {
            let _ = tokio::time::timeout(
                Duration::from_secs(5),
                stream.write_all(&http_unauthorized_response()),
            )
            .await;
            return;
        }
        let operation = async {
            let path_only = request.target.split('?').next().unwrap_or(&request.target);
            if request.method == "GET"
                && path_only.starts_with("/recordings/")
                && path_only != "/recordings/"
            {
                stream_recording(
                    &mut stream,
                    path_only,
                    recording_dir.clone(),
                    max_recording_download_bytes,
                )
                .await
            } else {
                let response = handle_http_request(
                    &request.method,
                    &request.target,
                    &manager,
                    &metrics,
                    &recording_dir,
                    max_recording_download_bytes,
                )
                .await;
                let written = stream.write_all(&response).await;
                written.map_err(anyhow::Error::from)
            }
        };
        let _ = tokio::time::timeout(Duration::from_secs(120), operation).await;
        return;
    }

    // WebSocket audio connections retain their server-minted single-use token
    // capability. All other upgrades are privileged control connections.
    if audio.is_none()
        && !is_authorized(
            &authenticator,
            request.authorization.as_deref(),
            &request.method,
            &request.target,
        )
    {
        let _ = tokio::time::timeout(
            Duration::from_secs(5),
            stream.write_all(&http_unauthorized_response()),
        )
        .await;
        return;
    }

    let ws_max_bytes = ws_max_message_size_kb.saturating_mul(1024);
    let ws_config = WebSocketConfig::default()
        .max_message_size(Some(ws_max_bytes))
        .max_frame_size(Some(ws_max_bytes));
    let stream = PrefixedIo::new(prefetched, stream);
    let ws_result = tokio::time::timeout(
        Duration::from_secs(10),
        accept_async_with_config(stream, Some(ws_config)),
    )
    .await;
    match ws_result {
        Ok(Ok(ws)) => match audio {
            Some(token) => {
                crate::control::ws_audio::handle_audio_connection(ws, token, permit, &manager)
                    .await;
            }
            None => {
                handle_connection(
                    ws,
                    peer_addr,
                    manager,
                    shutdown,
                    ws_ping_interval_secs,
                    event_channel_size,
                    critical_event_channel_size,
                )
                .await;
            }
        },
        Ok(Err(e)) => {
            debug!(peer = %peer_addr, error = %e, "WebSocket handshake failed");
        }
        Err(_) => {
            debug!(peer = %peer_addr, "WebSocket handshake timed out");
        }
    }
}

/// Path prefix for WebSocket audio-plane connections.
const AUDIO_PATH_PREFIX: &str = "/audio/";
const MAX_REQUEST_HEADER_BYTES: usize = 8192;

/// Read and retain a complete HTTP request header. The retained bytes are
/// replayed to tungstenite after classification, including a pipelined first
/// WebSocket frame that arrived in the same read.
async fn read_request_head(stream: &mut BoxedServerIo) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::with_capacity(1024);
    loop {
        let mut buffer = [0_u8; 1024];
        let read = stream.read(&mut buffer).await?;
        if read == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "connection closed before HTTP headers completed",
            ));
        }
        bytes.extend_from_slice(&buffer[..read]);
        if bytes.len() > MAX_REQUEST_HEADER_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "HTTP request headers exceed limit",
            ));
        }
        if bytes.windows(4).any(|window| window == b"\r\n\r\n") {
            return Ok(bytes);
        }
    }
}

/// Classify a WS request path. Returns `None` for non-audio (control) paths;
/// `Some(Some(token))` for a well-formed `/audio/<uuid>`; `Some(None)` for an
/// `/audio/...` path whose token is malformed (still an audio-plane request, to
/// be rejected rather than mistaken for control).
fn classify_audio_path(path: &str) -> Option<Option<Uuid>> {
    let path = path.split('?').next().unwrap_or(path);
    let rest = path.strip_prefix(AUDIO_PATH_PREFIX)?;
    Some(Uuid::parse_str(rest).ok())
}

fn parse_request_head(data: &[u8]) -> Option<RequestHead> {
    let end = data.windows(4).position(|window| window == b"\r\n\r\n")?;
    let header = std::str::from_utf8(&data[..end]).ok()?;
    let mut lines = header.split("\r\n");
    let request_line = lines.next()?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?.to_string();
    let target = parts.next()?.to_string();
    if parts.next()? != "HTTP/1.1" || parts.next().is_some() {
        return None;
    }

    let mut authorization = None;
    let mut host = None;
    let mut has_origin = false;
    let mut is_websocket_upgrade = false;
    for line in lines {
        let (name, value) = line.split_once(':')?;
        if name.eq_ignore_ascii_case("authorization") {
            if authorization.is_some() {
                return None;
            }
            authorization = Some(value.trim().to_string());
        } else if name.eq_ignore_ascii_case("host") {
            if host.is_some() {
                return None;
            }
            host = Some(value.trim().to_string());
        } else if name.eq_ignore_ascii_case("origin") {
            has_origin = true;
        } else if name.eq_ignore_ascii_case("upgrade")
            && value.trim().eq_ignore_ascii_case("websocket")
        {
            is_websocket_upgrade = true;
        }
    }
    Some(RequestHead {
        method,
        target,
        authorization,
        host,
        has_origin,
        is_websocket_upgrade,
    })
}

fn is_loopback_host(host: Option<&str>) -> bool {
    use tokio_tungstenite::tungstenite::http::uri::Authority;
    let Some(authority) = host.and_then(|host| host.parse::<Authority>().ok()) else {
        return false;
    };
    let host = authority.host();
    let valid_suffix = authority.as_str().strip_prefix(host).is_some_and(|suffix| {
        suffix.is_empty()
            || suffix
                .strip_prefix(':')
                .is_some_and(|port| port.parse::<u16>().is_ok())
    });
    if !valid_suffix {
        return false;
    }
    host.eq_ignore_ascii_case("localhost")
        || host.eq_ignore_ascii_case("localhost.")
        || host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.to_canonical().is_loopback())
}

fn request_requires_hmac(target: &str) -> bool {
    let path = target.split('?').next().unwrap_or(target);
    !matches!(path, "/health" | "/metrics")
}

fn is_authorized(
    authenticator: &Option<Arc<HmacAuthenticator>>,
    authorization: Option<&str>,
    method: &str,
    target: &str,
) -> bool {
    authenticator
        .as_ref()
        .is_none_or(|auth| auth.authorize(authorization, method, target).is_ok())
}

fn http_unauthorized_response() -> Vec<u8> {
    b"HTTP/1.1 401 Unauthorized\r\n\
      WWW-Authenticate: HMAC-SHA256\r\n\
      Content-Type: application/json\r\n\
      Content-Length: 24\r\n\
      Connection: close\r\n\
      \r\n\
      {\"error\":\"unauthorized\"}"
        .to_vec()
}

fn http_json_response(status: &str, body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 {status}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n\
         {body}",
        body.len()
    )
    .into_bytes()
}

#[cfg(test)]
fn http_binary_response(status: &str, content_type: &str, body: &[u8]) -> Vec<u8> {
    let header = format!(
        "HTTP/1.1 {status}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n",
        body.len()
    );
    let mut response = header.into_bytes();
    response.extend_from_slice(body);
    response
}

/// Handle an HTTP REST request and return the full HTTP response
async fn handle_http_request(
    method: &str,
    path: &str,
    manager: &Arc<SessionManager>,
    metrics: &Metrics,
    recording_dir: &PathBuf,
    max_recording_download_bytes: u64,
) -> Vec<u8> {
    // Strip query string for path matching; full path still available for handlers
    let path_only = path.split('?').next().unwrap_or(path);

    if path_only == "/health" && method == "GET" {
        return http_json_response("200 OK", r#"{"status":"ok"}"#);
    }

    if path_only == "/metrics" && method == "GET" {
        return match metrics.encode() {
            Ok(body) => {
                let header = format!(
                    "HTTP/1.1 200 OK\r\n\
                     Content-Type: text/plain; version=0.0.4; charset=utf-8\r\n\
                     Content-Length: {}\r\n\
                     Connection: close\r\n\
                     \r\n\
                     {body}",
                    body.len()
                );
                header.into_bytes()
            }
            Err(e) => {
                tracing::error!(error = %e, "metrics encoding failed");
                http_json_response(
                    "500 Internal Server Error",
                    r#"{"error":"metrics encoding failed"}"#,
                )
            }
        };
    }

    if path_only == "/recordings" || path_only.starts_with("/recordings/") {
        return handle_recording_request(method, path, recording_dir, max_recording_download_bytes)
            .await;
    }

    if method != "GET" {
        return http_json_response(
            "405 Method Not Allowed",
            r#"{"error":"method not allowed"}"#,
        );
    }

    let (status, body) = if path_only == "/sessions" {
        let sessions = manager.list_sessions();
        (
            "200 OK",
            serde_json::to_string_pretty(&sessions).unwrap_or_else(|e| {
                tracing::warn!(error = %e, "failed to serialize session list");
                "[]".to_string()
            }),
        )
    } else if let Some(id_str) = path_only.strip_prefix("/sessions/") {
        match Uuid::parse_str(id_str) {
            Ok(session_id) => match manager.get_session_details(&session_id).await {
                Some((state, created_at, details)) => {
                    let response = serde_json::json!({
                        "session_id": session_id,
                        "state": state,
                        "created_at": created_at,
                        "endpoints": details.endpoints,
                        "recordings": details.recordings,
                        "vad_active": details.vad_active,
                        "fax_detect_active": details.fax_detect_active,
                    });
                    (
                        "200 OK",
                        serde_json::to_string_pretty(&response).unwrap_or_else(|e| {
                            tracing::warn!(error = %e, "failed to serialize session details");
                            "{}".to_string()
                        }),
                    )
                }
                None => (
                    "404 Not Found",
                    r#"{"error":"session not found"}"#.to_string(),
                ),
            },
            Err(_) => (
                "400 Bad Request",
                r#"{"error":"invalid session ID"}"#.to_string(),
            ),
        }
    } else {
        ("404 Not Found", r#"{"error":"not found"}"#.to_string())
    };

    http_json_response(status, &body)
}

/// Four downloads at a time, with one 64 KiB buffer per admitted transfer.
/// Snapshot length comes from the opened handle; later appends are excluded.
async fn stream_recording(
    stream: &mut BoxedServerIo,
    path: &str,
    base: PathBuf,
    maximum: u64,
) -> anyhow::Result<()> {
    use std::io::Read;
    static DOWNLOADS: std::sync::OnceLock<Arc<tokio::sync::Semaphore>> = std::sync::OnceLock::new();
    let admission = Arc::clone(DOWNLOADS.get_or_init(|| Arc::new(tokio::sync::Semaphore::new(4))))
        .try_acquire_owned();
    let Ok(admission) = admission else {
        let written = stream
            .write_all(&http_json_response(
                "503 Service Unavailable",
                r#"{"error":"DOWNLOAD_BUSY"}"#,
            ))
            .await;
        written?;
        return Ok(());
    };
    let relative = percent_encoding::percent_decode_str(&path["/recordings/".len()..])
        .decode_utf8()?
        .into_owned();
    let opened = crate::storage::run(move || {
        let file = crate::storage::open_beneath(&base, std::path::Path::new(&relative))?;
        let length = file.metadata()?.len();
        Ok((file, length, admission))
    })
    .await;
    let (mut file, mut remaining, mut admission) = match opened {
        Ok(opened) => opened,
        Err(_) => {
            let written = stream
                .write_all(&http_json_response(
                    "404 Not Found",
                    r#"{"error":"recording not found"}"#,
                ))
                .await;
            written?;
            return Ok(());
        }
    };
    if remaining > maximum {
        let written = stream
            .write_all(&http_json_response(
                "413 Payload Too Large",
                r#"{"error":"recording too large"}"#,
            ))
            .await;
        written?;
        return Ok(());
    }
    let header = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/vnd.tcpdump.pcap\r\nContent-Length: {remaining}\r\nConnection: close\r\n\r\n"
    );
    let written =
        tokio::time::timeout(Duration::from_secs(5), stream.write_all(header.as_bytes())).await;
    written??;
    while remaining > 0 {
        let count = remaining.min(64 * 1024) as usize;
        let read = crate::storage::run(move || {
            let mut buffer = vec![0; count];
            let count = file.read(&mut buffer)?;
            buffer.truncate(count);
            Ok((file, buffer, admission))
        });
        let read_result = tokio::time::timeout(Duration::from_secs(10), read).await;
        let (next_file, buffer, next_admission) = read_result??;
        file = next_file;
        admission = next_admission;
        anyhow::ensure!(!buffer.is_empty(), "recording truncated during download");
        remaining -= buffer.len() as u64;
        let written = tokio::time::timeout(Duration::from_secs(5), stream.write_all(&buffer)).await;
        written??;
    }
    Ok(())
}

/// Handle requests to /recordings and /recordings/<path>
async fn handle_recording_request(
    method: &str,
    path: &str,
    recording_dir: &PathBuf,
    _max_recording_download_bytes: u64,
) -> Vec<u8> {
    let base_dir = recording_dir;

    // Split path and query string
    let (path_part, query_string) = match path.find('?') {
        Some(i) => (&path[..i], Some(&path[i + 1..])),
        None => (path, None),
    };

    // GET /recordings or GET /recordings/ — list recordings
    let trimmed = path_part.trim_end_matches('/');
    if trimmed == "/recordings" {
        if method != "GET" {
            return http_json_response(
                "405 Method Not Allowed",
                r#"{"error":"method not allowed"}"#,
            );
        }
        return list_recordings(base_dir, query_string).await;
    }

    // /recordings/<file_path> — single file operations
    let rel_path = &path_part["/recordings/".len()..];
    if rel_path.is_empty() {
        return http_json_response("400 Bad Request", r#"{"error":"missing file path"}"#);
    }

    // URL-decode the path
    let rel_path = percent_encoding::percent_decode_str(rel_path)
        .decode_utf8()
        .map(|s| s.into_owned())
        .unwrap_or_else(|_| rel_path.to_string());

    // Path traversal protection: resolve and verify the path stays within recording_dir
    let full_path = base_dir.join(&rel_path);
    let canonical_base = match base_dir.canonicalize() {
        Ok(p) => p,
        Err(_) => {
            return http_json_response(
                "500 Internal Server Error",
                r#"{"error":"recording directory not accessible"}"#,
            );
        }
    };
    let canonical_path = match full_path.canonicalize() {
        Ok(p) => p,
        Err(_) => {
            return http_json_response("404 Not Found", r#"{"error":"recording not found"}"#);
        }
    };
    if !canonical_path.starts_with(&canonical_base) {
        return http_json_response(
            "403 Forbidden",
            r#"{"error":"path outside recording directory"}"#,
        );
    }

    match method {
        // GET file bodies are handled by stream_recording before this dispatcher.
        "GET" => http_json_response("400 Bad Request", r#"{"error":"streaming required"}"#),
        "DELETE" => {
            let base_dir = canonical_base;
            let relative = PathBuf::from(rel_path);
            let removed = crate::storage::run(move || {
                Ok(crate::storage::remove_beneath(&base_dir, &relative))
            })
            .await;
            match removed {
                Ok(Ok(())) => http_json_response("200 OK", r#"{"deleted":true}"#),
                Ok(Err(_)) => {
                    http_json_response("404 Not Found", r#"{"error":"recording not found"}"#)
                }
                Err(_) => http_json_response(
                    "500 Internal Server Error",
                    r#"{"error":"failed to delete recording"}"#,
                ),
            }
        }
        _ => http_json_response(
            "405 Method Not Allowed",
            r#"{"error":"method not allowed"}"#,
        ),
    }
}

/// Parse query string parameters into key-value pairs
fn parse_query_params(query: &str) -> Vec<(&str, &str)> {
    query
        .split('&')
        .filter_map(|pair| {
            let mut parts = pair.splitn(2, '=');
            let key = parts.next()?;
            let value = parts.next().unwrap_or("");
            Some((key, value))
        })
        .collect()
}

/// List .pcap files in recording_dir, with optional filtering and pagination.
/// Query params: startsWith=<prefix>, skip=<n> (default 0), limit=<n> (default 100)
async fn list_recordings(base_dir: &PathBuf, query_string: Option<&str>) -> Vec<u8> {
    let mut starts_with: Option<String> = None;
    let mut skip: usize = 0;
    let mut limit: usize = 100;

    if let Some(qs) = query_string {
        for (key, value) in parse_query_params(qs) {
            match key {
                "startsWith" => {
                    let decoded = percent_encoding::percent_decode_str(value)
                        .decode_utf8()
                        .map(|s| s.into_owned())
                        .unwrap_or_else(|_| value.to_string());
                    starts_with = Some(decoded);
                }
                "skip" => {
                    skip = value.parse().unwrap_or(0);
                }
                "limit" => {
                    limit = value.parse().unwrap_or(100);
                }
                _ => {}
            }
        }
    }

    // Cap limit to 1000 to prevent abuse
    limit = limit.min(1000);

    let mut entries = match collect_pcap_files(base_dir, base_dir, &starts_with, 0).await {
        Ok(entries) => entries,
        Err(_) => {
            return http_json_response(
                "500 Internal Server Error",
                r#"{"error":"failed to read recording directory"}"#,
            );
        }
    };

    entries.sort();

    let total = entries.len();
    let page: Vec<&String> = entries.iter().skip(skip).take(limit).collect();

    let response = serde_json::json!({
        "recordings": page,
        "total": total,
        "skip": skip,
        "limit": limit,
    });
    let body = serde_json::to_string_pretty(&response).unwrap_or_else(|_| "{}".to_string());
    http_json_response("200 OK", &body)
}

/// Recursively collect .pcap file paths relative to base_dir
const MAX_PCAP_SCAN_DEPTH: usize = 10;

async fn collect_pcap_files(
    base_dir: &PathBuf,
    dir: &PathBuf,
    starts_with: &Option<String>,
    depth: usize,
) -> std::io::Result<Vec<String>> {
    let mut results = Vec::new();
    if depth > MAX_PCAP_SCAN_DEPTH {
        return Ok(results);
    }
    let mut read_dir = tokio::fs::read_dir(dir).await?;

    while let Some(entry) = read_dir.next_entry().await? {
        let file_type = entry.file_type().await?;
        let path = entry.path();

        if file_type.is_dir() {
            if let Ok(mut sub) =
                Box::pin(collect_pcap_files(base_dir, &path, starts_with, depth + 1)).await
            {
                results.append(&mut sub);
            }
        } else if file_type.is_file()
            && let Some(ext) = path.extension()
            && ext == "pcap"
            && let Ok(rel) = path.strip_prefix(base_dir)
        {
            let rel_str = rel.to_string_lossy().to_string();
            if let Some(prefix) = starts_with
                && !rel_str.starts_with(prefix.as_str())
            {
                continue;
            }
            results.push(rel_str);
        }
    }

    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_http_request_head() {
        let input = b"GET /sessions?detail=1 HTTP/1.1\r\nHost: localhost\r\nAuthorization: HMAC-SHA256 1:sig\r\n\r\n";
        let result = parse_request_head(input).expect("request should parse");
        assert_eq!(result.method, "GET");
        assert_eq!(result.target, "/sessions?detail=1");
        assert_eq!(result.authorization.as_deref(), Some("HMAC-SHA256 1:sig"));
        assert!(!result.is_websocket_upgrade);
    }

    #[test]
    fn loopback_hosts_and_browser_headers_are_unambiguous() {
        for host in [
            "localhost",
            "LOCALHOST.:9100",
            "127.0.0.1:9100",
            "[::1]:9100",
            "[::ffff:127.0.0.1]:9100",
        ] {
            assert!(is_loopback_host(Some(host)), "{host}");
        }
        for host in [
            "attacker.invalid",
            "localhost.attacker.invalid",
            "192.0.2.1",
            "",
            "127.0.0.1:bad",
        ] {
            assert!(!is_loopback_host(Some(host)), "{host}");
        }
        assert!(!is_loopback_host(None));
        let request =
            parse_request_head(b"GET / HTTP/1.1\r\nHost: localhost\r\noRiGiN: null\r\n\r\n")
                .unwrap();
        assert!(request.has_origin);
        assert!(
            parse_request_head(
                b"GET / HTTP/1.1\r\nHost: localhost\r\nHost: attacker.invalid\r\n\r\n"
            )
            .is_none()
        );
        assert!(
            parse_request_head(
                b"GET / HTTP/1.1\r\nAuthorization: first\r\nAuthorization: second\r\n\r\n"
            )
            .is_none()
        );
    }

    #[test]
    fn detects_websocket_upgrade_and_audio_capability_path() {
        let input = b"GET /audio/550e8400-e29b-41d4-a716-446655440000 HTTP/1.1\r\nUpgrade: websocket\r\n\r\n";
        let request = parse_request_head(input).expect("request should parse");
        assert!(request.is_websocket_upgrade);
        assert!(matches!(
            classify_audio_path(&request.target),
            Some(Some(_))
        ));
    }

    #[test]
    fn leaves_only_observability_routes_unauthenticated() {
        assert!(!request_requires_hmac("/health"));
        assert!(!request_requires_hmac("/metrics?format=prometheus"));
        assert!(request_requires_hmac("/sessions"));
        assert!(request_requires_hmac("/recordings?limit=1"));
    }

    #[test]
    fn rejects_incomplete_or_non_http_request_headers() {
        assert!(parse_request_head(b"GET / HTTP/1.1\r\n").is_none());
        assert!(parse_request_head(b"not http at all\r\n\r\n").is_none());
    }

    #[test]
    fn test_http_json_response_format() {
        let resp = http_json_response("200 OK", r#"{"status":"ok"}"#);
        let resp_str = String::from_utf8(resp).unwrap();
        assert!(resp_str.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(resp_str.contains("Content-Type: application/json\r\n"));
        assert!(resp_str.contains("Content-Length: 15\r\n"));
        assert!(resp_str.contains(r#"{"status":"ok"}"#));
    }

    #[test]
    fn test_http_binary_response_format() {
        let body = vec![0xDE, 0xAD, 0xBE, 0xEF];
        let resp = http_binary_response("200 OK", "application/octet-stream", &body);
        let resp_str = String::from_utf8_lossy(&resp);
        assert!(resp_str.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(resp_str.contains("Content-Type: application/octet-stream\r\n"));
        assert!(resp_str.contains("Content-Length: 4\r\n"));
        // Verify the binary body is at the end
        assert!(resp.ends_with(&[0xDE, 0xAD, 0xBE, 0xEF]));
    }

    #[test]
    fn test_parse_query_params() {
        let params = parse_query_params("startsWith=foo&skip=10&limit=50");
        assert_eq!(params.len(), 3);
        assert!(params.contains(&("startsWith", "foo")));
        assert!(params.contains(&("skip", "10")));
        assert!(params.contains(&("limit", "50")));
    }

    #[test]
    fn test_parse_query_params_empty() {
        let params = parse_query_params("");
        // Empty string produces one entry with key="" and value=""
        assert_eq!(params.len(), 1);
        assert_eq!(params[0].0, "");
    }

    #[tokio::test]
    async fn test_list_recordings_pagination() {
        let dir = tempfile::tempdir().unwrap();
        // Create 5 .pcap files
        for i in 0..5 {
            std::fs::write(dir.path().join(format!("rec-{i}.pcap")), b"fake pcap").unwrap();
        }

        let base_dir = dir.path().to_path_buf();
        let resp = list_recordings(&base_dir, Some("skip=1&limit=2")).await;
        let body = extract_json_body(&resp);
        assert_eq!(body["total"], 5);
        assert_eq!(body["skip"], 1);
        assert_eq!(body["limit"], 2);
        let recordings = body["recordings"].as_array().unwrap();
        assert_eq!(recordings.len(), 2);
    }

    #[tokio::test]
    async fn test_list_recordings_starts_with_filter() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("alpha-1.pcap"), b"fake").unwrap();
        std::fs::write(dir.path().join("alpha-2.pcap"), b"fake").unwrap();
        std::fs::write(dir.path().join("beta-1.pcap"), b"fake").unwrap();

        let base_dir = dir.path().to_path_buf();
        let resp = list_recordings(&base_dir, Some("startsWith=alpha")).await;
        let body = extract_json_body(&resp);
        assert_eq!(body["total"], 2, "should only match alpha- files");
        let recordings = body["recordings"].as_array().unwrap();
        assert!(
            recordings
                .iter()
                .all(|r| r.as_str().unwrap().starts_with("alpha"))
        );
    }

    #[tokio::test]
    async fn test_recording_path_traversal_blocked() {
        let dir = tempfile::tempdir().unwrap();
        // Create a legitimate file
        std::fs::write(dir.path().join("legit.pcap"), b"pcap data").unwrap();

        let base_dir = dir.path().to_path_buf();
        let resp = handle_recording_request(
            "GET",
            "/recordings/../../../etc/passwd",
            &base_dir,
            512 * 1024 * 1024,
        )
        .await;
        let _body = extract_json_body(&resp);
        // Should be either 403 Forbidden or 404 Not Found (canonicalize will fail for nonexistent paths)
        let resp_str = String::from_utf8_lossy(&resp);
        assert!(
            resp_str.contains("403 Forbidden") || resp_str.contains("404 Not Found"),
            "path traversal should be blocked: {}",
            resp_str
        );
    }

    #[tokio::test]
    async fn recording_stream_snapshots_length_without_whole_file_buffering() {
        use std::io::Write;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("active.pcap");
        let expected = vec![0x5a; 2 * 1024 * 1024];
        std::fs::write(&path, &expected).unwrap();
        let (server, mut peer) = tokio::io::duplex(1024);
        let base = directory.path().to_path_buf();
        let download = tokio::spawn(async move {
            let mut stream: BoxedServerIo = Box::new(server);
            stream_recording(
                &mut stream,
                "/recordings/active.pcap",
                base,
                4 * 1024 * 1024,
            )
            .await
        });
        let mut received = Vec::new();
        while !received.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
            let byte = peer.read_u8().await;
            received.push(byte.unwrap());
        }
        let header_length = received.len();
        let mut append = std::fs::OpenOptions::new().append(true).open(path).unwrap();
        append.write_all(b"later append excluded").unwrap();
        let read = peer.read_to_end(&mut received).await;
        read.unwrap();
        let completed = download.await;
        completed.unwrap().unwrap();
        assert_eq!(&received[header_length..], expected);
        assert!(
            String::from_utf8_lossy(&received[..header_length]).contains("Content-Length: 2097152")
        );
    }

    /// Helper to extract JSON body from an HTTP response byte vector
    fn extract_json_body(resp: &[u8]) -> serde_json::Value {
        let resp_str = String::from_utf8_lossy(resp);
        let body_start = resp_str.find("\r\n\r\n").expect("no body separator") + 4;
        let body = resp_str[body_start..].trim();
        serde_json::from_str(body).unwrap_or_else(|e| panic!("not JSON: {e}\n{body}"))
    }
}
