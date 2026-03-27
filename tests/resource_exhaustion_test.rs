mod helpers;

use serde_json::json;

use helpers::control_client::TestControlClient;
use helpers::test_server::TestServer;
use helpers::timing;

// ═══════════════════════════════════════════════════════════════════════════
// Recording path validation and lifecycle
// ═══════════════════════════════════════════════════════════════════════════

/// Recording with a path traversal attempt ("../escape/test.pcap") should be rejected,
/// and a relative (non-absolute) path should also be rejected.
#[tokio::test]
async fn test_recording_invalid_path_rejected() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;

    // Create an endpoint so the session has something to record
    client
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv"}),
        )
        .await;

    // Attempt path traversal with ".." — should be rejected
    let traversal_path = format!("{}/../escape/test.pcap", server.recording_dir);
    let resp = client
        .request(
            "recording.start",
            json!({"endpoint_id": null, "file_path": traversal_path}),
        )
        .await;
    assert!(
        resp["error"].is_object(),
        "path traversal should be rejected: {resp}"
    );
    let error_code = resp["error"]["code"].as_str().unwrap_or("");
    assert!(
        error_code == "INVALID_PARAMS" || error_code == "RECORDING_ERROR",
        "expected INVALID_PARAMS or RECORDING_ERROR for path traversal, got: {error_code}"
    );

    // Attempt relative path — should be rejected
    let resp = client
        .request(
            "recording.start",
            json!({"endpoint_id": null, "file_path": "test.pcap"}),
        )
        .await;
    assert!(
        resp["error"].is_object(),
        "relative path should be rejected: {resp}"
    );
    let error_code = resp["error"]["code"].as_str().unwrap_or("");
    assert_eq!(
        error_code, "INVALID_PARAMS",
        "expected INVALID_PARAMS for relative path, got: {error_code}"
    );
    let error_msg = resp["error"]["message"].as_str().unwrap_or("");
    assert!(
        error_msg.contains("absolute"),
        "error message should mention 'absolute': {error_msg}"
    );

    // A valid absolute path within the recording dir should succeed
    let valid_path = format!("{}/valid-recording.pcap", server.recording_dir);
    let result = client
        .request_ok(
            "recording.start",
            json!({"endpoint_id": null, "file_path": valid_path}),
        )
        .await;
    assert!(
        !result["recording_id"].as_str().unwrap().is_empty(),
        "valid recording path should succeed"
    );

    // Stop the recording to clean up
    let rec_id = result["recording_id"].as_str().unwrap().to_string();
    client
        .request_ok("recording.stop", json!({"recording_id": rec_id}))
        .await;

    client.request_ok("session.destroy", json!({})).await;
}

/// Recordings that are active when a session is destroyed should be stopped
/// automatically (no leaked file handles).
#[tokio::test]
async fn test_recording_stopped_on_session_destroy() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;

    // Create an endpoint
    client
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv"}),
        )
        .await;

    // Start two recordings
    let rec_path_1 = format!("{}/destroy-test-1.pcap", server.recording_dir);
    let rec_path_2 = format!("{}/destroy-test-2.pcap", server.recording_dir);

    let result1 = client
        .request_ok(
            "recording.start",
            json!({"endpoint_id": null, "file_path": rec_path_1}),
        )
        .await;
    assert!(!result1["recording_id"].as_str().unwrap().is_empty());

    let result2 = client
        .request_ok(
            "recording.start",
            json!({"endpoint_id": null, "file_path": rec_path_2}),
        )
        .await;
    assert!(!result2["recording_id"].as_str().unwrap().is_empty());

    // Verify session.info shows the recordings
    let info = client.request_ok("session.info", json!({})).await;
    let recordings = info["recordings"].as_array().unwrap();
    assert_eq!(
        recordings.len(),
        2,
        "session should have 2 active recordings"
    );

    // Destroy session without explicitly stopping recordings
    client.request_ok("session.destroy", json!({})).await;

    // Give the server a moment to finalize
    tokio::time::sleep(timing::scaled_ms(200)).await;

    // Verify the recording files were created on disk
    assert!(
        std::path::Path::new(&format!("{}/destroy-test-1.pcap", server.recording_dir)).exists(),
        "recording file 1 should exist on disk after session destroy"
    );
    assert!(
        std::path::Path::new(&format!("{}/destroy-test-2.pcap", server.recording_dir)).exists(),
        "recording file 2 should exist on disk after session destroy"
    );

    // Server should still be functional after the destroyed session
    let mut client2 = TestControlClient::connect(&server.addr).await;
    client2.request_ok("session.create", json!({})).await;
    let info = client2.request_ok("session.info", json!({})).await;
    assert!(!info["session_id"].as_str().unwrap().is_empty());
    client2.request_ok("session.destroy", json!({})).await;
}

// ═══════════════════════════════════════════════════════════════════════════
// Max sessions under concurrent load
// ═══════════════════════════════════════════════════════════════════════════

/// With max_sessions=5, spawn 10 concurrent tasks each trying to create a session.
/// Exactly 5 should succeed and 5 should fail with MAX_SESSIONS_REACHED.
#[tokio::test]
async fn test_max_sessions_under_concurrent_load() {
    let server = TestServer::builder().max_sessions(5).start().await;
    let addr = server.addr.clone();

    let num_tasks = 10;
    let mut handles = Vec::with_capacity(num_tasks);

    for _ in 0..num_tasks {
        let addr = addr.clone();
        let handle = tokio::spawn(async move {
            let mut client = TestControlClient::connect(&addr).await;
            let resp = client.request("session.create", json!({})).await;
            (client, resp)
        });
        handles.push(handle);
    }

    let mut successes = Vec::new();
    let mut failures = 0usize;

    for handle in handles {
        let (client, resp) = handle.await.expect("task should not panic");
        if resp.get("result").is_some() {
            successes.push(client);
        } else {
            assert!(
                resp["error"].is_object(),
                "expected error object for rejected session: {resp}"
            );
            let code = resp["error"]["code"].as_str().unwrap_or("");
            assert_eq!(
                code, "MAX_SESSIONS_REACHED",
                "expected MAX_SESSIONS_REACHED, got: {code}"
            );
            failures += 1;
        }
    }

    assert_eq!(
        successes.len(),
        5,
        "exactly 5 sessions should succeed, got {}",
        successes.len()
    );
    assert_eq!(
        failures, 5,
        "exactly 5 sessions should fail, got {failures}"
    );

    // Clean up all successful sessions
    for mut client in successes {
        client.request_ok("session.destroy", json!({})).await;
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// Max endpoints concurrent creation
// ═══════════════════════════════════════════════════════════════════════════

/// With max_endpoints_per_session=3, spawn 6 concurrent tasks each trying to
/// create an endpoint on the same session. No more than 3 should succeed.
/// Then verify a different session can still create endpoints (limit is per-session).
#[tokio::test]
async fn test_max_endpoints_concurrent_creation() {
    let server = TestServer::builder()
        .max_endpoints_per_session(3)
        .start()
        .await;

    // Session 1: create a session and try to add 6 endpoints concurrently.
    // Since all commands for a session are serialized through one channel,
    // we send them sequentially from one client (the session task processes
    // them one at a time). Exactly 3 should succeed.
    let mut client1 = TestControlClient::connect(&server.addr).await;
    client1.request_ok("session.create", json!({})).await;

    let mut ep_successes = 0usize;
    let mut ep_failures = 0usize;

    for _ in 0..6 {
        let resp = client1
            .request(
                "endpoint.create_offer",
                json!({"type": "rtp", "direction": "sendrecv"}),
            )
            .await;
        if resp.get("result").is_some() {
            ep_successes += 1;
        } else {
            assert!(
                resp["error"].is_object(),
                "expected error for rejected endpoint: {resp}"
            );
            let msg = resp["error"]["message"].as_str().unwrap_or("");
            assert!(
                msg.contains("MAX_ENDPOINTS_REACHED"),
                "expected MAX_ENDPOINTS_REACHED in error message, got: {msg}"
            );
            ep_failures += 1;
        }
    }

    assert_eq!(
        ep_successes, 3,
        "exactly 3 endpoints should succeed, got {ep_successes}"
    );
    assert_eq!(
        ep_failures, 3,
        "exactly 3 endpoint creations should fail, got {ep_failures}"
    );

    // Verify session 1 has exactly 3 endpoints
    let info = client1.request_ok("session.info", json!({})).await;
    assert_eq!(
        info["endpoints"].as_array().unwrap().len(),
        3,
        "session 1 should have exactly 3 endpoints"
    );

    // Session 2: a separate session should have its own independent endpoint limit
    let mut client2 = TestControlClient::connect(&server.addr).await;
    client2.request_ok("session.create", json!({})).await;

    // Should be able to create endpoints on session 2 even though session 1 is at its limit
    let result = client2
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv"}),
        )
        .await;
    assert!(
        !result["endpoint_id"].as_str().unwrap().is_empty(),
        "session 2 should be able to create endpoints independently"
    );

    let result = client2
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv"}),
        )
        .await;
    assert!(
        !result["endpoint_id"].as_str().unwrap().is_empty(),
        "session 2 should create a second endpoint"
    );

    let result = client2
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv"}),
        )
        .await;
    assert!(
        !result["endpoint_id"].as_str().unwrap().is_empty(),
        "session 2 should create a third endpoint"
    );

    // Session 2's 4th endpoint should also fail
    let resp = client2
        .request(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv"}),
        )
        .await;
    assert!(
        resp["error"].is_object(),
        "session 2 should also be limited to 3 endpoints: {resp}"
    );

    // Clean up
    client1.request_ok("session.destroy", json!({})).await;
    client2.request_ok("session.destroy", json!({})).await;
}

// ═══════════════════════════════════════════════════════════════════════════
// Request ID too long
// ═══════════════════════════════════════════════════════════════════════════

/// Sending a request with an "id" field that is 2000 characters long should
/// return an error with code INVALID_PARAMS and the id truncated to 1024 chars.
#[tokio::test]
async fn test_request_id_too_long() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;

    let long_id = "x".repeat(2000);
    let raw_request = format!(
        r#"{{"id":"{}","method":"session.create","params":{{}}}}"#,
        long_id
    );

    let resp = client.send_raw(&raw_request).await;

    // Should get an error response
    assert!(
        resp["error"].is_object(),
        "oversized request id should return error: {resp}"
    );
    assert_eq!(
        resp["error"]["code"].as_str().unwrap(),
        "INVALID_PARAMS",
        "error code should be INVALID_PARAMS"
    );
    let error_msg = resp["error"]["message"].as_str().unwrap_or("");
    assert!(
        error_msg.contains("maximum length") || error_msg.contains("1024"),
        "error message should mention maximum length: {error_msg}"
    );

    // The echoed id should be truncated to 1024 characters
    let resp_id = resp["id"].as_str().unwrap_or("");
    assert_eq!(
        resp_id.len(),
        1024,
        "response id should be truncated to 1024 chars, got {}",
        resp_id.len()
    );
    assert!(
        resp_id.chars().all(|c| c == 'x'),
        "truncated id should contain only 'x' characters"
    );

    // Server should still be functional after the oversized id
    let result = client.request_ok("session.create", json!({})).await;
    assert!(
        !result["session_id"].as_str().unwrap().is_empty(),
        "server should still accept valid requests after oversized id"
    );
    client.request_ok("session.destroy", json!({})).await;
}

// ═══════════════════════════════════════════════════════════════════════════
// Max recordings per session
// ═══════════════════════════════════════════════════════════════════════════

/// With max_recordings_per_session=2, the third recording.start should fail.
#[tokio::test]
async fn test_max_recordings_per_session_enforced() {
    let server = TestServer::builder()
        .max_recordings_per_session(2)
        .start()
        .await;
    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;

    // Create an endpoint
    client
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv"}),
        )
        .await;

    // First two recordings should succeed
    let path1 = format!("{}/rec-limit-1.pcap", server.recording_dir);
    let path2 = format!("{}/rec-limit-2.pcap", server.recording_dir);
    let path3 = format!("{}/rec-limit-3.pcap", server.recording_dir);

    let r1 = client
        .request_ok(
            "recording.start",
            json!({"endpoint_id": null, "file_path": path1}),
        )
        .await;
    assert!(!r1["recording_id"].as_str().unwrap().is_empty());

    let r2 = client
        .request_ok(
            "recording.start",
            json!({"endpoint_id": null, "file_path": path2}),
        )
        .await;
    assert!(!r2["recording_id"].as_str().unwrap().is_empty());

    // Third recording should be rejected
    let r3 = client
        .request(
            "recording.start",
            json!({"endpoint_id": null, "file_path": path3}),
        )
        .await;
    assert!(
        r3["error"].is_object(),
        "third recording should be rejected: {r3}"
    );
    let msg = r3["error"]["message"].as_str().unwrap_or("");
    assert!(
        msg.contains("Maximum concurrent recordings"),
        "error should mention recording limit: {msg}"
    );

    // Stop one recording, then the third should succeed
    let rec_id = r1["recording_id"].as_str().unwrap().to_string();
    client
        .request_ok("recording.stop", json!({"recording_id": rec_id}))
        .await;

    let r3_retry = client
        .request_ok(
            "recording.start",
            json!({"endpoint_id": null, "file_path": path3}),
        )
        .await;
    assert!(
        !r3_retry["recording_id"].as_str().unwrap().is_empty(),
        "recording should succeed after freeing a slot"
    );

    client.request_ok("session.destroy", json!({})).await;
}

// ═══════════════════════════════════════════════════════════════════════════
// Max WebSocket connections
// ═══════════════════════════════════════════════════════════════════════════

/// With max_connections=2, the 3rd WebSocket connection should be rejected with HTTP 503.
#[tokio::test]
async fn test_max_connections_enforced() {
    let server = TestServer::builder().max_connections(2).start().await;

    // Open 2 connections — should succeed
    let client1 = TestControlClient::connect(&server.addr).await;
    let _client2 = TestControlClient::connect(&server.addr).await;

    // 3rd connection should be rejected (HTTP 503, not a WebSocket upgrade)
    let ws_url = format!("ws://{}", server.addr);
    let result = tokio_tungstenite::connect_async(&ws_url).await;
    assert!(
        result.is_err(),
        "3rd connection should be rejected when max_connections=2"
    );

    // Explicitly close one connection to free a slot
    client1.close().await;
    tokio::time::sleep(timing::scaled_ms(1000)).await;

    // Now a new connection should succeed
    let ws_url = format!("ws://{}", server.addr);
    let result = tokio_tungstenite::connect_async(&ws_url).await;
    assert!(
        result.is_ok(),
        "connection should succeed after closing a slot"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// WebSocket ping keepalive
// ═══════════════════════════════════════════════════════════════════════════

/// With a 1-second ping interval, we should receive Ping frames from the server.
#[tokio::test]
async fn test_ws_ping_keepalive() {
    let server = TestServer::builder().ws_ping_interval_secs(1).start().await;

    // Connect with raw WebSocket to observe Ping frames
    let ws_url = format!("ws://{}", server.addr);
    let (ws_stream, _) = tokio_tungstenite::connect_async(&ws_url)
        .await
        .expect("WebSocket connect should succeed");

    let (_ws_tx, mut ws_rx) = futures_util::StreamExt::split(ws_stream);

    // Wait up to 3 seconds for a Ping frame
    let ping_received = tokio::time::timeout(std::time::Duration::from_secs(3), async {
        use futures_util::StreamExt;
        while let Some(msg) = ws_rx.next().await {
            match msg {
                Ok(tokio_tungstenite::tungstenite::Message::Ping(_)) => return true,
                Ok(_) => continue,
                Err(_) => return false,
            }
        }
        false
    })
    .await;

    assert!(
        ping_received.unwrap_or(false),
        "should receive a Ping frame within 3 seconds with 1s interval"
    );
}
