use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::accept_async_with_config;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tracing::{debug, error, info};
use uuid::Uuid;

use super::connection::handle_connection;
use crate::metrics::Metrics;
use crate::session::SessionManager;
use crate::shutdown::ShutdownCoordinator;

#[allow(clippy::too_many_arguments)]
pub async fn run_websocket_server(
    listen_addrs: Vec<SocketAddr>,
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
    let mut listeners = Vec::with_capacity(listen_addrs.len());
    for addr in &listen_addrs {
        let listener = TcpListener::bind(addr).await?;
        info!(addr = %addr, "Control server listening (WS + HTTP)");
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
                    let _ = tokio::time::timeout(
                        Duration::from_secs(1),
                        stream.write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"),
                    ).await;
                    drop(stream);
                    continue;
                }

                let permit = match connection_semaphore.clone().try_acquire_owned() {
                    Ok(permit) => permit,
                    Err(_) => {
                        debug!(peer = %peer_addr, "connection rejected: max connections reached");
                        let _ = tokio::time::timeout(
                            Duration::from_secs(1),
                            stream.write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"),
                        ).await;
                        drop(stream);
                        continue;
                    }
                };

                let manager = Arc::clone(&manager);
                let shutdown = shutdown.clone();
                let metrics = Arc::clone(&metrics);
                let recording_dir = recording_dir.clone();
                let ws_max_size = ws_max_message_size_kb;

                tokio::spawn(async move {
                    handle_incoming(stream, peer_addr, manager, shutdown, metrics, recording_dir, ws_max_size, max_recording_download_bytes, ws_ping_interval_secs, event_channel_size, critical_event_channel_size).await;
                    drop(permit); // release connection slot
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

/// Handle an incoming TCP connection: either HTTP REST or WebSocket upgrade
#[allow(clippy::too_many_arguments)]
async fn handle_incoming(
    mut stream: TcpStream,
    peer_addr: SocketAddr,
    manager: Arc<SessionManager>,
    shutdown: ShutdownCoordinator,
    metrics: Arc<Metrics>,
    recording_dir: PathBuf,
    ws_max_message_size_kb: usize,
    max_recording_download_bytes: u64,
    ws_ping_interval_secs: u64,
    event_channel_size: usize,
    critical_event_channel_size: usize,
) {
    // Classify the connection as HTTP or WebSocket within 5 seconds.
    // Only the peek/classification is time-bounded; HTTP handlers run without timeout.
    enum Classification {
        Http(String, String),
        NotHttp,
        PeekFailed,
    }

    let classification = tokio::time::timeout(Duration::from_secs(5), async {
        let mut peek_buf = [0u8; 8192];
        let n = match stream.peek(&mut peek_buf).await {
            Ok(n) => n,
            Err(_) => return Classification::PeekFailed,
        };

        let request_line = String::from_utf8_lossy(&peek_buf[..n]);
        match extract_http_request(&request_line) {
            Some((method, path)) => Classification::Http(method, path),
            _ => Classification::NotHttp,
        }
    })
    .await;

    match classification {
        Ok(Classification::Http(method, path)) => {
            // HTTP route — consume the peeked request, handle it, and respond.
            let mut buf = vec![0u8; 4096];
            let _n = match stream.read(&mut buf).await {
                Ok(n) => n,
                Err(e) => {
                    debug!(peer = %peer_addr, error = %e, "HTTP request read failed");
                    return;
                }
            };

            let response = handle_http_request(
                &method,
                &path,
                &manager,
                &metrics,
                &recording_dir,
                max_recording_download_bytes,
            )
            .await;
            let _ = stream.write_all(&response).await;
            return;
        }
        Ok(Classification::PeekFailed) => return,
        Err(_) => {
            debug!(peer = %peer_addr, "connection timed out during HTTP classification");
            return;
        }
        Ok(Classification::NotHttp) => {
            // Fall through to WebSocket upgrade
        }
    }

    // WebSocket upgrade with timeout to prevent slowloris-style DoS on the handshake.
    let ws_max_bytes = ws_max_message_size_kb.saturating_mul(1024);
    let ws_config = WebSocketConfig::default()
        .max_message_size(Some(ws_max_bytes))
        .max_frame_size(Some(ws_max_bytes));
    let ws_result = tokio::time::timeout(
        Duration::from_secs(10),
        accept_async_with_config(stream, Some(ws_config)),
    )
    .await;
    match ws_result {
        Ok(Ok(ws)) => {
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
        Ok(Err(e)) => {
            debug!(peer = %peer_addr, error = %e, "WebSocket handshake failed");
        }
        Err(_) => {
            debug!(peer = %peer_addr, "WebSocket handshake timed out");
        }
    }
}

/// Extract the method and path from an HTTP request line, if not a WS upgrade
fn extract_http_request(data: &str) -> Option<(String, String)> {
    let first_line = data.lines().next()?;
    let parts: Vec<&str> = first_line.split_whitespace().collect();
    if parts.len() < 2 {
        return None;
    }

    let method = parts[0];
    if !matches!(method, "GET" | "DELETE") {
        return None;
    }

    // Check if this has an Upgrade header (WebSocket)
    if method == "GET" {
        let lower = data.to_ascii_lowercase();
        if lower.contains("upgrade: websocket") {
            return None;
        }
    }

    Some((method.to_string(), parts[1].to_string()))
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

/// Handle requests to /recordings and /recordings/<path>
async fn handle_recording_request(
    method: &str,
    path: &str,
    recording_dir: &PathBuf,
    max_recording_download_bytes: u64,
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
        "GET" => {
            // Check file size before reading to prevent OOM on large recordings
            match tokio::fs::metadata(&canonical_path).await {
                Ok(meta) if meta.len() > max_recording_download_bytes => {
                    return http_json_response(
                        "413 Payload Too Large",
                        &format!(
                            r#"{{"error":"recording too large ({} bytes, max {max_recording_download_bytes})"}}"#,
                            meta.len()
                        ),
                    );
                }
                Err(_) => {
                    return http_json_response(
                        "404 Not Found",
                        r#"{"error":"recording not found"}"#,
                    );
                }
                _ => {}
            }
            match tokio::fs::read(&canonical_path).await {
                Ok(data) => http_binary_response("200 OK", "application/vnd.tcpdump.pcap", &data),
                Err(_) => http_json_response("404 Not Found", r#"{"error":"recording not found"}"#),
            }
        }
        "DELETE" => match tokio::fs::remove_file(&canonical_path).await {
            Ok(()) => http_json_response("200 OK", r#"{"deleted":true}"#),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                http_json_response("404 Not Found", r#"{"error":"recording not found"}"#)
            }
            Err(_) => http_json_response(
                "500 Internal Server Error",
                r#"{"error":"failed to delete recording"}"#,
            ),
        },
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
        } else if file_type.is_file() {
            if let Some(ext) = path.extension() {
                if ext == "pcap" {
                    if let Ok(rel) = path.strip_prefix(base_dir) {
                        let rel_str = rel.to_string_lossy().to_string();
                        if let Some(prefix) = starts_with {
                            if !rel_str.starts_with(prefix.as_str()) {
                                continue;
                            }
                        }
                        results.push(rel_str);
                    }
                }
            }
        }
    }

    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_http_request_get() {
        let input = "GET /sessions HTTP/1.1\r\nHost: localhost\r\n";
        let result = extract_http_request(input);
        assert_eq!(result, Some(("GET".to_string(), "/sessions".to_string())));
    }

    #[test]
    fn test_extract_http_request_delete() {
        let input = "DELETE /recordings/test.pcap HTTP/1.1\r\nHost: localhost\r\n";
        let result = extract_http_request(input);
        assert_eq!(
            result,
            Some(("DELETE".to_string(), "/recordings/test.pcap".to_string()))
        );
    }

    #[test]
    fn test_extract_http_request_websocket_upgrade_excluded() {
        let input =
            "GET / HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n";
        let result = extract_http_request(input);
        assert_eq!(result, None, "WebSocket upgrade should be excluded");
    }

    #[test]
    fn test_extract_http_request_post_rejected() {
        let input = "POST /sessions HTTP/1.1\r\nHost: localhost\r\n";
        let result = extract_http_request(input);
        assert_eq!(result, None, "POST should be rejected");
    }

    #[test]
    fn test_extract_http_request_empty_input() {
        let result = extract_http_request("");
        assert_eq!(result, None);
    }

    #[test]
    fn test_extract_http_request_garbage() {
        let result = extract_http_request("not http at all");
        assert_eq!(result, None);
    }

    #[test]
    fn test_extract_http_request_path_with_query() {
        let input = "GET /recordings?startsWith=test&limit=10 HTTP/1.1\r\n";
        let result = extract_http_request(input);
        assert_eq!(
            result,
            Some((
                "GET".to_string(),
                "/recordings?startsWith=test&limit=10".to_string()
            ))
        );
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

    /// Helper to extract JSON body from an HTTP response byte vector
    fn extract_json_body(resp: &[u8]) -> serde_json::Value {
        let resp_str = String::from_utf8_lossy(resp);
        let body_start = resp_str.find("\r\n\r\n").expect("no body separator") + 4;
        let body = resp_str[body_start..].trim();
        serde_json::from_str(body).unwrap_or_else(|e| panic!("not JSON: {e}\n{body}"))
    }
}
