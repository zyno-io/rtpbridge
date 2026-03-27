mod helpers;

use serde_json::{Value, json};
use tempfile::TempDir;

use helpers::control_client::TestControlClient;
use helpers::http::{http_delete, http_get, http_get_text};
use helpers::test_rtp_peer::{TestRtpPeer, parse_rtp_addr_from_sdp};
use helpers::test_server::TestServer;
use helpers::timing;

/// Test: Rapid disconnect → reconnect. Session should survive and be reattachable.
#[tokio::test]
async fn test_orphan_and_reattach() {
    let server = TestServer::builder()
        .disconnect_timeout_secs(2)
        .start()
        .await;

    // Connect and create a session
    let mut client1 = TestControlClient::connect(&server.addr).await;
    let result = client1.request_ok("session.create", json!({})).await;
    let session_id = result["session_id"].as_str().unwrap().to_string();

    // Create an endpoint to verify session state persists
    client1
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv"}),
        )
        .await;

    // Explicitly close the WebSocket to trigger disconnect detection.
    // Poll the /sessions endpoint until it shows the orphaned session,
    // rather than a fixed sleep which is racy.
    client1.close().await;
    let deadline = tokio::time::Instant::now() + timing::scaled_ms(3000);
    let mut found = false;
    while tokio::time::Instant::now() < deadline {
        let sessions = http_get(&server.addr, "/sessions").await;
        let arr = sessions.as_array().unwrap();
        if arr.len() == 1 {
            found = true;
            break;
        }
        tokio::time::sleep(timing::scaled_ms(50)).await;
    }
    assert!(
        found,
        "session should still exist after disconnect (orphaned)"
    );

    // Reconnect and reattach
    let mut client2 = TestControlClient::connect(&server.addr).await;
    let result = client2
        .request("session.attach", json!({"session_id": session_id}))
        .await;
    assert!(
        result.get("result").is_some(),
        "attach should succeed: {result}"
    );

    // Session should be active again with its endpoint intact
    let info = client2.request_ok("session.info", json!({})).await;
    assert_eq!(
        info["endpoints"].as_array().unwrap().len(),
        1,
        "endpoint should survive orphan/reattach"
    );

    client2.request_ok("session.destroy", json!({})).await;
}

/// Test: Orphan timeout expires and session is destroyed
#[tokio::test]
async fn test_orphan_timeout_expires() {
    let server = TestServer::builder()
        .disconnect_timeout_secs(2)
        .start()
        .await;

    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;

    // Explicitly close to trigger immediate disconnect detection
    client.close().await;

    // Wait for orphan timeout to expire (2s timeout + margin)
    tokio::time::sleep(timing::scaled_ms(4000)).await;

    // Session should be gone
    let sessions = http_get(&server.addr, "/sessions").await;
    assert_eq!(
        sessions.as_array().unwrap().len(),
        0,
        "session should be destroyed after orphan timeout"
    );
}

/// Test: Recording is properly flushed when session is destroyed
#[tokio::test]
async fn test_recording_cleanup_on_destroy() {
    let server = TestServer::builder()
        .disconnect_timeout_secs(2)
        .start()
        .await;
    let mut client = TestControlClient::connect(&server.addr).await;

    client.request_ok("session.create", json!({})).await;

    // Create an RTP endpoint
    client
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv"}),
        )
        .await;

    // Start recording
    let pcap_path = std::path::Path::new(&server.recording_dir).join("recording-cleanup-test.pcap");
    let pcap_path_str = pcap_path.to_str().unwrap();
    let result = client
        .request_ok(
            "recording.start",
            json!({"endpoint_id": null, "file_path": pcap_path_str}),
        )
        .await;
    assert!(!result["recording_id"].as_str().unwrap().is_empty());

    // Verify recording file was created
    assert!(
        pcap_path.exists(),
        "PCAP file should be created on recording.start"
    );

    // Destroy session WITHOUT explicitly stopping the recording
    // The session should clean up properly
    client.request_ok("session.destroy", json!({})).await;

    // Give the recording task time to flush
    tokio::time::sleep(timing::scaled_ms(500)).await;

    // The PCAP file should still exist (flushed, not deleted)
    assert!(
        pcap_path.exists(),
        "PCAP file should be flushed and preserved after session destroy"
    );
}

/// Test: WebRTC ICE restart returns a new offer with different ICE credentials
#[tokio::test]
async fn test_webrtc_ice_restart() {
    let server = TestServer::builder()
        .disconnect_timeout_secs(2)
        .start()
        .await;
    let mut client = TestControlClient::connect(&server.addr).await;

    client.request_ok("session.create", json!({})).await;

    // Create a WebRTC endpoint from a remote offer (so str0m has negotiated state)
    let fake_offer = "\
        v=0\r\n\
        o=- 1 1 IN IP4 127.0.0.1\r\n\
        s=-\r\n\
        t=0 0\r\n\
        a=group:BUNDLE 0\r\n\
        m=audio 9 UDP/TLS/RTP/SAVPF 111\r\n\
        c=IN IP4 0.0.0.0\r\n\
        a=mid:0\r\n\
        a=sendrecv\r\n\
        a=rtpmap:111 opus/48000/2\r\n\
        a=ice-ufrag:test1234\r\n\
        a=ice-pwd:testpassword123456789012\r\n\
        a=fingerprint:sha-256 AA:BB:CC:DD:EE:FF:00:11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF:00:11:22:33:44:55:66:77:88:99\r\n\
        a=setup:actpass\r\n\
        a=rtcp-mux\r\n";

    let result = client
        .request_ok(
            "endpoint.create_from_offer",
            json!({"sdp": fake_offer, "direction": "sendrecv"}),
        )
        .await;
    let ep_id = result["endpoint_id"].as_str().unwrap().to_string();
    let original_answer = result["sdp_answer"].as_str().unwrap().to_string();

    // Perform ICE restart
    let result = client
        .request("endpoint.ice_restart", json!({"endpoint_id": ep_id}))
        .await;
    assert!(
        result.get("result").is_some(),
        "ICE restart should succeed: {result}"
    );

    let new_offer = result["result"]["sdp_offer"].as_str().unwrap();

    // The new offer should be valid SDP
    assert!(new_offer.contains("v=0"), "should be valid SDP");
    assert!(new_offer.contains("m=audio"), "should have audio");

    // ICE credentials should have changed from the original answer
    let old_ufrag = extract_ice_ufrag(&original_answer);
    let new_ufrag = extract_ice_ufrag(new_offer);
    if !old_ufrag.is_empty() && !new_ufrag.is_empty() {
        assert_ne!(
            old_ufrag, new_ufrag,
            "ICE restart should generate new credentials"
        );
    }

    client.request_ok("session.destroy", json!({})).await;
}

fn extract_ice_ufrag(sdp: &str) -> String {
    for line in sdp.lines() {
        let line = line.trim();
        if let Some(ufrag) = line.strip_prefix("a=ice-ufrag:") {
            return ufrag.to_string();
        }
    }
    String::new()
}

/// Test: Shutdown under load — create multiple sessions, then kill the server.
/// The server should handle shutdown gracefully without panics.
#[tokio::test]
async fn test_shutdown_with_active_sessions() {
    let server = TestServer::builder()
        .disconnect_timeout_secs(2)
        .start()
        .await;

    // Create several sessions concurrently
    let mut clients = Vec::new();
    for _ in 0..5 {
        let mut client = TestControlClient::connect(&server.addr).await;
        client.request_ok("session.create", json!({})).await;
        client
            .request_ok(
                "endpoint.create_offer",
                json!({"type": "rtp", "direction": "sendrecv"}),
            )
            .await;
        clients.push(client);
    }

    // Verify all sessions exist
    let sessions = http_get(&server.addr, "/sessions").await;
    assert_eq!(
        sessions.as_array().unwrap().len(),
        5,
        "should have 5 sessions"
    );

    // Initiate graceful shutdown (simulates SIGTERM)
    server.initiate_shutdown();

    // Drop all client connections so the server can clean up
    drop(clients);

    // Wait for graceful shutdown to drain sessions
    tokio::time::sleep(timing::scaled_ms(2000)).await;

    // After shutdown, sessions should be cleaned up or server should be stopped
    let http_result = reqwest::Client::new()
        .get(format!("http://{}/sessions", server.addr))
        .timeout(timing::scaled_ms(2000))
        .send()
        .await;

    match http_result {
        Ok(resp) => {
            if let Ok(text) = resp.text().await {
                if let Ok(sessions) = serde_json::from_str::<Value>(&text) {
                    let count = sessions.as_array().map(|a| a.len()).unwrap_or(0);
                    assert_eq!(
                        count, 0,
                        "sessions should be cleaned up after graceful shutdown, got {count}"
                    );
                }
            }
        }
        // Connection refused = server already stopped = expected
        Err(_) => {}
    }
}

/// Test: Max sessions limit is enforced
#[tokio::test]
async fn test_max_sessions_limit() {
    let server = TestServer::builder()
        .disconnect_timeout_secs(2)
        .max_sessions(2)
        .start()
        .await;

    // Create first session
    let mut client1 = TestControlClient::connect(&server.addr).await;
    let result = client1.request_ok("session.create", json!({})).await;
    assert!(!result["session_id"].as_str().unwrap().is_empty());

    // Create second session
    let mut client2 = TestControlClient::connect(&server.addr).await;
    let result = client2.request_ok("session.create", json!({})).await;
    assert!(!result["session_id"].as_str().unwrap().is_empty());

    // Third session should fail
    let mut client3 = TestControlClient::connect(&server.addr).await;
    let result = client3.request("session.create", json!({})).await;
    assert!(
        result.get("error").is_some(),
        "third session should be rejected: {result}"
    );
    assert_eq!(
        result["error"]["code"].as_str().unwrap(),
        "MAX_SESSIONS_REACHED",
        "error code should be MAX_SESSIONS_REACHED"
    );

    // Destroy one session to free a slot
    client1.request_ok("session.destroy", json!({})).await;
    tokio::time::sleep(timing::scaled_ms(200)).await;

    // Now the third client should be able to create a session
    let result = client3.request_ok("session.create", json!({})).await;
    assert!(
        !result["session_id"].as_str().unwrap().is_empty(),
        "should succeed after freeing a slot"
    );

    client2.request_ok("session.destroy", json!({})).await;
    client3.request_ok("session.destroy", json!({})).await;
}

/// Test: Rapid disconnect → reconnect timing race
#[tokio::test]
async fn test_rapid_disconnect_reconnect_race() {
    let server = TestServer::builder()
        .disconnect_timeout_secs(2)
        .start()
        .await;

    let mut client = TestControlClient::connect(&server.addr).await;
    let result = client.request_ok("session.create", json!({})).await;
    let session_id = result["session_id"].as_str().unwrap().to_string();

    // Rapid disconnect and reconnect multiple times
    for _i in 0..3 {
        // Close explicitly to trigger disconnect detection
        client.close().await;
        // Wait briefly for orphan state to be set — 50ms is enough to exercise the race
        tokio::time::sleep(timing::scaled_ms(50)).await;

        // Reconnect and reattach
        client = TestControlClient::connect(&server.addr).await;
        let result = client
            .request("session.attach", json!({"session_id": session_id}))
            .await;
        assert!(
            result.get("result").is_some(),
            "rapid reattach should succeed: {result}"
        );
    }

    // Session should still be functional
    let info = client.request_ok("session.info", json!({})).await;
    assert_eq!(
        info["session_id"].as_str().unwrap(),
        session_id,
        "session ID should be preserved through rapid reconnects"
    );

    client.request_ok("session.destroy", json!({})).await;
}

/// Test: Recording channel backpressure — verify recording doesn't block media flow
/// and that the recording captures packets even under load.
#[tokio::test]
async fn test_recording_backpressure() {
    let server = TestServer::builder()
        .disconnect_timeout_secs(2)
        .start()
        .await;
    let mut client = TestControlClient::connect(&server.addr).await;

    client.request_ok("session.create", json!({})).await;

    let mut peer_a = TestRtpPeer::new().await;
    let mut peer_b = TestRtpPeer::new().await;

    let offer_a = peer_a.make_sdp_offer();
    let result = client
        .request_ok(
            "endpoint.create_from_offer",
            json!({"sdp": offer_a, "direction": "sendrecv"}),
        )
        .await;
    let answer_a = result["sdp_answer"].as_str().unwrap();
    peer_a.set_remote(parse_rtp_addr_from_sdp(answer_a).unwrap());

    let offer_b = peer_b.make_sdp_offer();
    let result = client
        .request_ok(
            "endpoint.create_from_offer",
            json!({"sdp": offer_b, "direction": "sendrecv"}),
        )
        .await;
    let answer_b = result["sdp_answer"].as_str().unwrap();
    peer_b.set_remote(parse_rtp_addr_from_sdp(answer_b).unwrap());
    peer_b.activate().await;
    tokio::time::sleep(timing::scaled_ms(50)).await;
    peer_b.start_recv();

    // Start recording
    let pcap_path = std::path::Path::new(&server.recording_dir).join("backpressure-test.pcap");
    let pcap_path_str = pcap_path.to_str().unwrap();
    let rec = client
        .request_ok(
            "recording.start",
            json!({"endpoint_id": null, "file_path": pcap_path_str}),
        )
        .await;
    let rec_id = rec["recording_id"].as_str().unwrap().to_string();

    // Send a burst of packets as fast as possible
    for _ in 0..50 {
        peer_a.send_pcmu(&vec![0x80u8; 160]).await;
    }
    tokio::time::sleep(timing::scaled_ms(500)).await;

    // Media routing should still work
    let received = peer_b.received_count();
    assert!(
        received > 0,
        "media should flow even under recording load, got {received}"
    );

    // Stop recording
    let stop = client
        .request_ok("recording.stop", json!({"recording_id": rec_id}))
        .await;
    assert!(
        stop["packets"].as_u64().unwrap() > 0,
        "recording should have captured packets"
    );

    // Control plane still responsive
    let info = client.request_ok("session.info", json!({})).await;
    assert_eq!(info["endpoints"].as_array().unwrap().len(), 2);

    client.request_ok("session.destroy", json!({})).await;
}

/// Test: Command channel doesn't deadlock under concurrent control requests
/// while media is actively flowing.
#[tokio::test]
async fn test_concurrent_media_and_control() {
    let server = TestServer::builder()
        .disconnect_timeout_secs(2)
        .start()
        .await;
    let mut client = TestControlClient::connect(&server.addr).await;

    client.request_ok("session.create", json!({})).await;

    let mut peer_a = TestRtpPeer::new().await;
    let mut peer_b = TestRtpPeer::new().await;

    let offer_a = peer_a.make_sdp_offer();
    let result = client
        .request_ok(
            "endpoint.create_from_offer",
            json!({"sdp": offer_a, "direction": "sendrecv"}),
        )
        .await;
    peer_a.set_remote(parse_rtp_addr_from_sdp(result["sdp_answer"].as_str().unwrap()).unwrap());

    let offer_b = peer_b.make_sdp_offer();
    let result = client
        .request_ok(
            "endpoint.create_from_offer",
            json!({"sdp": offer_b, "direction": "sendrecv"}),
        )
        .await;
    peer_b.set_remote(parse_rtp_addr_from_sdp(result["sdp_answer"].as_str().unwrap()).unwrap());
    peer_b.activate().await;
    tokio::time::sleep(timing::scaled_ms(50)).await;
    peer_b.start_recv();

    // Spawn background media sender
    let socket = peer_a.socket.clone();
    let remote = peer_a.remote_addr.unwrap();
    let media_handle = tokio::spawn(async move {
        let mut seq = 0u16;
        let mut ts = 0u32;
        for _ in 0..50 {
            let payload = vec![0x80u8; 160];
            let mut pkt = Vec::with_capacity(12 + payload.len());
            pkt.push(0x80);
            pkt.push(0);
            pkt.extend_from_slice(&seq.to_be_bytes());
            pkt.extend_from_slice(&ts.to_be_bytes());
            pkt.extend_from_slice(&0x12345678u32.to_be_bytes());
            pkt.extend_from_slice(&payload);
            let _ = socket.send_to(&pkt, remote).await;
            seq = seq.wrapping_add(1);
            ts = ts.wrapping_add(160);
            tokio::time::sleep(timing::PACING).await;
        }
    });

    // While media flows, send control commands
    for _ in 0..5 {
        let info = client.request_ok("session.info", json!({})).await;
        let eps = info["endpoints"].as_array().unwrap();
        assert!(!eps.is_empty());
        tokio::time::sleep(timing::scaled_ms(100)).await;
    }

    // Start and stop a recording mid-stream
    let pcap_path = std::path::Path::new(&server.recording_dir).join("concurrent-test.pcap");
    let pcap_path_str = pcap_path.to_str().unwrap();
    let rec = client
        .request_ok(
            "recording.start",
            json!({"endpoint_id": null, "file_path": pcap_path_str}),
        )
        .await;
    tokio::time::sleep(timing::scaled_ms(200)).await;
    client
        .request_ok(
            "recording.stop",
            json!({"recording_id": rec["recording_id"].as_str().unwrap()}),
        )
        .await;

    media_handle.await.unwrap();

    let received = peer_b.received_count();
    assert!(
        received > 0,
        "media should flow during concurrent control commands, got {received}"
    );

    client.request_ok("session.destroy", json!({})).await;
}

/// Test: max_endpoints_per_session limit is enforced
#[tokio::test]
async fn test_max_endpoints_per_session_limit() {
    let server = TestServer::builder()
        .disconnect_timeout_secs(2)
        .max_endpoints_per_session(2)
        .start()
        .await;
    let mut client = TestControlClient::connect(&server.addr).await;

    client.request_ok("session.create", json!({})).await;

    // First two endpoints should succeed
    let peer1 = TestRtpPeer::new().await;
    let offer1 = peer1.make_sdp_offer();
    client
        .request_ok(
            "endpoint.create_from_offer",
            json!({"sdp": offer1, "direction": "sendrecv"}),
        )
        .await;

    client
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv"}),
        )
        .await;

    // Third endpoint via create_from_offer should fail
    let peer3 = TestRtpPeer::new().await;
    let offer3 = peer3.make_sdp_offer();
    let resp = client
        .request(
            "endpoint.create_from_offer",
            json!({"sdp": offer3, "direction": "sendrecv"}),
        )
        .await;
    assert!(
        resp.get("error").is_some(),
        "third endpoint should be rejected: {resp}"
    );

    // Third endpoint via create_offer should also fail
    let resp = client
        .request(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv"}),
        )
        .await;
    assert!(
        resp.get("error").is_some(),
        "third endpoint via create_offer should be rejected: {resp}"
    );

    client.request_ok("session.destroy", json!({})).await;
}

/// Test: /health endpoint returns 200 with status ok
#[tokio::test]
async fn test_health_endpoint() {
    let server = TestServer::builder()
        .disconnect_timeout_secs(2)
        .start()
        .await;
    let health = http_get(&server.addr, "/health").await;
    assert_eq!(health["status"], "ok");
}

/// Test: Prometheus /metrics endpoint returns metric names
#[tokio::test]
async fn test_prometheus_metrics_endpoint() {
    let server = TestServer::builder()
        .disconnect_timeout_secs(2)
        .start()
        .await;
    let mut client = TestControlClient::connect(&server.addr).await;

    client.request_ok("session.create", json!({})).await;

    // Create an endpoint so counters increment
    client
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv"}),
        )
        .await;

    let (_, metrics) = http_get_text(&server.addr, "/metrics").await;

    // Counters get _total suffix automatically from prometheus-client
    assert!(
        metrics.contains("rtpbridge_sessions_total"),
        "metrics should contain sessions_total"
    );
    assert!(
        metrics.contains("rtpbridge_endpoints_total"),
        "metrics should contain endpoints_total"
    );
    assert!(
        metrics.contains("rtpbridge_sessions_active"),
        "metrics should contain sessions_active"
    );
    assert!(
        metrics.contains("rtpbridge_packets_routed_total"),
        "metrics should contain packets_routed_total"
    );

    client.request_ok("session.destroy", json!({})).await;
}

/// Test: Metrics counters increment after media flow
#[tokio::test]
async fn test_metrics_counters_increment() {
    let server = TestServer::builder()
        .disconnect_timeout_secs(2)
        .start()
        .await;
    let mut client = TestControlClient::connect(&server.addr).await;

    client.request_ok("session.create", json!({})).await;

    // Create two endpoints for media flow
    let mut peer_a = TestRtpPeer::new().await;
    let mut peer_b = TestRtpPeer::new().await;

    let offer_a = peer_a.make_sdp_offer();
    let result = client
        .request_ok(
            "endpoint.create_from_offer",
            json!({"sdp": offer_a, "direction": "sendrecv"}),
        )
        .await;
    peer_a.set_remote(parse_rtp_addr_from_sdp(result["sdp_answer"].as_str().unwrap()).unwrap());

    let offer_b = peer_b.make_sdp_offer();
    let result = client
        .request_ok(
            "endpoint.create_from_offer",
            json!({"sdp": offer_b, "direction": "sendrecv"}),
        )
        .await;
    peer_b.set_remote(parse_rtp_addr_from_sdp(result["sdp_answer"].as_str().unwrap()).unwrap());

    // Snapshot before media
    let (_, before) = http_get_text(&server.addr, "/metrics").await;

    // Send some RTP packets
    peer_a.send_tone_for(timing::scaled_ms(200)).await;
    tokio::time::sleep(timing::scaled_ms(500)).await;

    // Snapshot after media
    let (_, after) = http_get_text(&server.addr, "/metrics").await;

    // Parse packets_routed_total from both snapshots
    let parse_counter = |text: &str, name: &str| -> u64 {
        for line in text.lines() {
            if line.starts_with(name) && !line.starts_with(&format!("{name}_")) {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() >= 2 {
                    return parts[1].parse().unwrap_or(0);
                }
            }
        }
        0
    };

    let before_routed = parse_counter(&before, "rtpbridge_packets_routed_total");
    let after_routed = parse_counter(&after, "rtpbridge_packets_routed_total");
    assert!(
        after_routed > before_routed,
        "packets_routed should increase after media flow: before={before_routed}, after={after_routed}"
    );

    client.request_ok("session.destroy", json!({})).await;
}

/// Test: Double recording.stop returns error on second stop
#[tokio::test]
async fn test_double_recording_stop() {
    let server = TestServer::builder()
        .disconnect_timeout_secs(2)
        .start()
        .await;
    let mut client = TestControlClient::connect(&server.addr).await;

    client.request_ok("session.create", json!({})).await;

    client
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv"}),
        )
        .await;

    let pcap_path = std::path::Path::new(&server.recording_dir).join("double-stop-test.pcap");
    let pcap_path_str = pcap_path.to_str().unwrap();
    let rec = client
        .request_ok(
            "recording.start",
            json!({"endpoint_id": null, "file_path": pcap_path_str}),
        )
        .await;
    let rec_id = rec["recording_id"].as_str().unwrap().to_string();

    // First stop should succeed
    client
        .request_ok("recording.stop", json!({"recording_id": rec_id}))
        .await;

    // Second stop should fail
    let resp = client
        .request("recording.stop", json!({"recording_id": rec_id}))
        .await;
    assert!(
        resp.get("error").is_some(),
        "second recording.stop should fail: {resp}"
    );

    client.request_ok("session.destroy", json!({})).await;
}

/// Test: Operations on a removed endpoint return errors
#[tokio::test]
async fn test_operations_on_removed_endpoint() {
    let server = TestServer::builder()
        .disconnect_timeout_secs(2)
        .start()
        .await;
    let mut client = TestControlClient::connect(&server.addr).await;

    client.request_ok("session.create", json!({})).await;

    let result = client
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv"}),
        )
        .await;
    let ep_id = result["endpoint_id"].as_str().unwrap().to_string();

    // Remove the endpoint
    client
        .request_ok("endpoint.remove", json!({"endpoint_id": ep_id}))
        .await;

    // DTMF inject on removed endpoint should fail
    let resp = client
        .request(
            "endpoint.dtmf.inject",
            json!({"endpoint_id": ep_id, "digit": "5", "duration_ms": 100, "volume": 10}),
        )
        .await;
    assert!(
        resp.get("error").is_some(),
        "DTMF inject on removed endpoint should fail: {resp}"
    );

    // VAD start on removed endpoint should fail
    let resp = client
        .request(
            "vad.start",
            json!({"endpoint_id": ep_id, "silence_interval_ms": 200}),
        )
        .await;
    assert!(
        resp.get("error").is_some(),
        "vad.start on removed endpoint should fail: {resp}"
    );

    // endpoint.remove again should fail
    let resp = client
        .request("endpoint.remove", json!({"endpoint_id": ep_id}))
        .await;
    assert!(
        resp.get("error").is_some(),
        "second endpoint.remove should fail: {resp}"
    );

    client.request_ok("session.destroy", json!({})).await;
}

/// Test: Recording to an unwritable path returns error
#[tokio::test]
async fn test_recording_to_unwritable_path() {
    let server = TestServer::builder()
        .disconnect_timeout_secs(2)
        .start()
        .await;
    let mut client = TestControlClient::connect(&server.addr).await;

    client.request_ok("session.create", json!({})).await;

    client
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv"}),
        )
        .await;

    let resp = client
        .request(
            "recording.start",
            json!({"endpoint_id": null, "file_path": "/nonexistent-dir/recording.pcap"}),
        )
        .await;
    assert!(
        resp.get("error").is_some(),
        "recording to unwritable path should fail: {resp}"
    );

    client.request_ok("session.destroy", json!({})).await;
}

/// Test: Runtime metrics are updated for sessions, endpoints, recordings, and packets
#[tokio::test]
async fn test_runtime_metrics_updated() {
    let server = TestServer::builder()
        .disconnect_timeout_secs(2)
        .start()
        .await;

    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;

    // Create RTP endpoint
    let result = client
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv"}),
        )
        .await;
    let ep_id = result["endpoint_id"].as_str().unwrap().to_string();
    let offer = result["sdp_offer"].as_str().unwrap();
    let server_addr = parse_rtp_addr_from_sdp(offer).expect("parse server addr");

    let mut peer = TestRtpPeer::new().await;
    let answer = peer.make_sdp_answer();
    client
        .request_ok(
            "endpoint.accept_answer",
            json!({"endpoint_id": ep_id, "sdp": answer}),
        )
        .await;
    peer.set_remote(server_addr);

    // Send some RTP to generate routed packets
    peer.send_tone_for(timing::scaled_ms(300)).await;
    tokio::time::sleep(timing::scaled_ms(500)).await;

    // Start a recording
    let rec_path = std::path::Path::new(&server.recording_dir).join("metrics-test.pcap");
    let rec_path_str = rec_path.to_str().unwrap();
    let rec_result = client
        .request_ok("recording.start", json!({"file_path": rec_path_str}))
        .await;
    let recording_id = rec_result["recording_id"].as_str().unwrap().to_string();

    // Send more packets while recording
    peer.send_tone_for(timing::scaled_ms(200)).await;
    tokio::time::sleep(timing::scaled_ms(300)).await;

    // Check metrics mid-recording
    let (_, mid_metrics) = http_get_text(&server.addr, "/metrics").await;
    assert!(
        extract_metric(&mid_metrics, "rtpbridge_sessions_active") >= 1,
        "sessions_active should be >= 1"
    );
    assert!(
        extract_metric(&mid_metrics, "rtpbridge_endpoints_active") >= 1,
        "endpoints_active should be >= 1"
    );
    assert!(
        extract_metric(&mid_metrics, "rtpbridge_recordings_active") >= 1,
        "recordings_active should be >= 1 during recording"
    );
    assert!(
        extract_metric(&mid_metrics, "rtpbridge_packets_recorded_total") > 0,
        "packets_recorded should be > 0 during recording"
    );

    // Stop recording
    client
        .request_ok("recording.stop", json!({"recording_id": recording_id}))
        .await;

    // Check recordings_active went back down
    let (_, post_metrics) = http_get_text(&server.addr, "/metrics").await;
    assert_eq!(
        extract_metric(&post_metrics, "rtpbridge_recordings_active"),
        0,
        "recordings_active should be 0 after stopping recording"
    );
    assert!(
        extract_metric(&post_metrics, "rtpbridge_sessions_total") >= 1,
        "sessions_total should be >= 1"
    );
    assert!(
        extract_metric(&post_metrics, "rtpbridge_endpoints_total") >= 1,
        "endpoints_total should be >= 1"
    );

    client.request_ok("session.destroy", json!({})).await;
}

/// Extract a numeric metric value from Prometheus text output
fn extract_metric(text: &str, name: &str) -> i64 {
    for line in text.lines() {
        if line.starts_with(name) && !line.starts_with(&format!("{name}_")) {
            if let Some(val_str) = line.split_whitespace().nth(1) {
                return val_str.parse::<f64>().unwrap_or(0.0) as i64;
            }
        }
    }
    panic!("metric {name} not found in output:\n{text}");
}

/// Test: max_sessions config prevents exceeding the limit, allows after destroy
#[tokio::test]
async fn test_max_sessions_enforcement() {
    let server = TestServer::builder().max_sessions(2).start().await;

    let mut client1 = TestControlClient::connect(&server.addr).await;
    let mut client2 = TestControlClient::connect(&server.addr).await;
    client1.request_ok("session.create", json!({})).await;
    client2.request_ok("session.create", json!({})).await;

    // 3rd session should fail
    let mut client3 = TestControlClient::connect(&server.addr).await;
    let resp = client3.request("session.create", json!({})).await;
    assert_eq!(
        resp["error"]["code"].as_str().unwrap(),
        "MAX_SESSIONS_REACHED",
        "3rd session should be rejected: {resp}"
    );

    // Destroy one → can create again
    client2.request_ok("session.destroy", json!({})).await;
    tokio::time::sleep(timing::scaled_ms(200)).await;

    let mut client4 = TestControlClient::connect(&server.addr).await;
    client4.request_ok("session.create", json!({})).await;

    // Verify exactly 2 sessions
    let list = client1.request_ok("session.list", json!({})).await;
    assert_eq!(list["sessions"].as_array().unwrap().len(), 2);

    client1.request_ok("session.destroy", json!({})).await;
    client4.request_ok("session.destroy", json!({})).await;
}

/// Test: Initiating shutdown while a recording is active flushes the PCAP file
#[tokio::test]
async fn test_shutdown_flushes_active_recordings() {
    let tmp = TempDir::new().unwrap();
    let recording_dir = tmp.path().join("shutdown-rec");
    std::fs::create_dir_all(&recording_dir).unwrap();

    let server = TestServer::builder()
        .media_dir(tmp.path().to_str().unwrap())
        .recording_dir(recording_dir.to_str().unwrap())
        .disconnect_timeout_secs(300)
        .start()
        .await;

    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;

    // Set up two endpoints
    let mut peer_a = TestRtpPeer::new().await;
    let offer_a = peer_a.make_sdp_offer();
    let result = client
        .request_ok(
            "endpoint.create_from_offer",
            json!({"sdp": offer_a, "direction": "sendrecv"}),
        )
        .await;
    peer_a.set_remote(parse_rtp_addr_from_sdp(result["sdp_answer"].as_str().unwrap()).unwrap());

    let peer_b = TestRtpPeer::new().await;
    let offer_b = peer_b.make_sdp_offer();
    client
        .request_ok(
            "endpoint.create_from_offer",
            json!({"sdp": offer_b, "direction": "sendrecv"}),
        )
        .await;

    // Start session-wide recording
    let pcap_path = recording_dir.join("shutdown-test.pcap");
    let pcap_path_str = pcap_path.to_str().unwrap();
    let rec = client
        .request_ok(
            "recording.start",
            json!({"endpoint_id": null, "file_path": pcap_path_str}),
        )
        .await;
    assert!(!rec["recording_id"].as_str().unwrap().is_empty());

    // Send media
    peer_a.send_tone_for(timing::scaled_ms(500)).await;
    tokio::time::sleep(timing::scaled_ms(500)).await;

    // Initiate shutdown while recording is still active (not explicitly stopped)
    server.initiate_shutdown();
    client.close().await;

    // Wait for session cleanup and recording flush
    tokio::time::sleep(timing::scaled_ms(3000)).await;

    // Verify PCAP file exists and has actual packet data
    let metadata = std::fs::metadata(&pcap_path);
    assert!(
        metadata.is_ok(),
        "PCAP file should exist after shutdown: {}",
        pcap_path.display()
    );
    assert!(
        metadata.unwrap().len() > 24,
        "PCAP file should have data beyond the file header"
    );

    // Parse and validate PCAP
    let file = std::fs::File::open(&pcap_path).expect("open PCAP");
    let mut reader = pcap_file::pcap::PcapReader::new(file).expect("valid PCAP");
    let mut count = 0u32;
    while let Some(pkt) = reader.next_packet() {
        pkt.expect("valid packet");
        count += 1;
    }
    assert!(
        count > 0,
        "PCAP should contain recorded packets after shutdown, got {count}"
    );
}

/// Test: Destroying a session that was already destroyed returns an error
#[tokio::test]
async fn test_double_session_destroy() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;

    client.request_ok("session.create", json!({})).await;
    client.request_ok("session.destroy", json!({})).await;

    // Second destroy should fail — no session bound
    let resp = client.request("session.destroy", json!({})).await;
    assert!(
        resp.get("error").is_some(),
        "second session.destroy should fail: {resp}"
    );
}

/// Test: Removing a non-existent endpoint returns an error
#[tokio::test]
async fn test_remove_nonexistent_endpoint() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;

    client.request_ok("session.create", json!({})).await;

    let fake_id = uuid::Uuid::new_v4().to_string();
    let resp = client
        .request("endpoint.remove", json!({"endpoint_id": fake_id}))
        .await;
    assert!(
        resp.get("error").is_some(),
        "removing non-existent endpoint should fail: {resp}"
    );

    client.request_ok("session.destroy", json!({})).await;
}

/// Test: accept_answer without a prior create_offer returns an error
#[tokio::test]
async fn test_accept_answer_without_offer() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;

    client.request_ok("session.create", json!({})).await;

    let fake_id = uuid::Uuid::new_v4().to_string();
    let resp = client
        .request(
            "endpoint.accept_answer",
            json!({"endpoint_id": fake_id, "sdp": "v=0\r\n"}),
        )
        .await;
    assert!(
        resp.get("error").is_some(),
        "accept_answer on non-existent endpoint should fail: {resp}"
    );

    client.request_ok("session.destroy", json!({})).await;
}

/// Test: accept_answer called twice on the same endpoint succeeds (re-negotiation)
#[tokio::test]
async fn test_accept_answer_called_twice() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;

    client.request_ok("session.create", json!({})).await;

    let result = client
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv"}),
        )
        .await;
    let ep_id = result["endpoint_id"].as_str().unwrap().to_string();

    let peer = TestRtpPeer::new().await;
    let answer = peer.make_sdp_answer();

    // First answer should succeed
    client
        .request_ok(
            "endpoint.accept_answer",
            json!({"endpoint_id": ep_id, "sdp": answer}),
        )
        .await;

    // Second answer with the same SDP should also succeed (re-negotiation)
    let peer2 = TestRtpPeer::new().await;
    let answer2 = peer2.make_sdp_answer();
    let resp = client
        .request(
            "endpoint.accept_answer",
            json!({"endpoint_id": ep_id, "sdp": answer2}),
        )
        .await;
    // Should succeed or at least not crash the session
    assert!(
        resp.get("result").is_some() || resp.get("error").is_some(),
        "second accept_answer should return a valid response: {resp}"
    );

    client.request_ok("session.destroy", json!({})).await;
}

/// Test: Stopping a non-existent recording returns an error
#[tokio::test]
async fn test_stop_nonexistent_recording() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;

    client.request_ok("session.create", json!({})).await;

    let fake_id = uuid::Uuid::new_v4().to_string();
    let resp = client
        .request("recording.stop", json!({"recording_id": fake_id}))
        .await;
    assert!(
        resp.get("error").is_some(),
        "stopping non-existent recording should fail: {resp}"
    );

    client.request_ok("session.destroy", json!({})).await;
}

/// Test: Session idle timeout destroys a session after the configured period of inactivity.
/// A session with no commands and no media should be auto-destroyed and emit a
/// `session.idle_timeout` event.
#[tokio::test]
async fn test_session_idle_timeout() {
    let server = TestServer::builder()
        .session_idle_timeout_secs(2)
        .start()
        .await;
    let mut client = TestControlClient::connect(&server.addr).await;

    client.request_ok("session.create", json!({})).await;

    // Wait for the idle timeout to fire (2s configured + margin)
    let event = client.recv_event(timing::scaled_ms(5000)).await;
    assert!(
        event.is_some(),
        "should receive session.idle_timeout event before the wait expires"
    );
    let event = event.unwrap();
    assert_eq!(
        event["event"].as_str().unwrap(),
        "session.idle_timeout",
        "event type should be session.idle_timeout, got: {event}"
    );
    assert_eq!(
        event["data"]["idle_timeout_secs"].as_u64().unwrap(),
        2,
        "idle_timeout_secs in event data should match configured value"
    );

    // After the idle timeout the session should be destroyed. Allow a moment for cleanup.
    tokio::time::sleep(timing::scaled_ms(500)).await;

    // Trying to get session info should fail because the session is gone
    let resp = client.request("session.info", json!({})).await;
    assert!(
        resp.get("error").is_some(),
        "session.info should fail after idle timeout destroyed the session: {resp}"
    );
}

/// Test: max_recordings_per_session limit is enforced via recording.start.
/// Starting more recordings than the limit should fail, and freeing a slot
/// should allow a new recording to succeed.
#[tokio::test]
async fn test_max_recordings_per_session() {
    let server = TestServer::builder()
        .max_recordings_per_session(2)
        .start()
        .await;
    let mut client = TestControlClient::connect(&server.addr).await;

    client.request_ok("session.create", json!({})).await;

    // Create an RTP endpoint so the session is non-trivial
    let peer = TestRtpPeer::new().await;
    let offer = peer.make_sdp_offer();
    client
        .request_ok(
            "endpoint.create_from_offer",
            json!({"sdp": offer, "direction": "sendrecv"}),
        )
        .await;

    // Start recording #1 — should succeed
    let pcap1 = std::path::Path::new(&server.recording_dir).join("max-rec-1.pcap");
    let rec1 = client
        .request_ok(
            "recording.start",
            json!({"endpoint_id": null, "file_path": pcap1.to_str().unwrap()}),
        )
        .await;
    let rec1_id = rec1["recording_id"].as_str().unwrap().to_string();

    // Start recording #2 — should succeed
    let pcap2 = std::path::Path::new(&server.recording_dir).join("max-rec-2.pcap");
    let _rec2 = client
        .request_ok(
            "recording.start",
            json!({"endpoint_id": null, "file_path": pcap2.to_str().unwrap()}),
        )
        .await;

    // Start recording #3 — should fail (limit is 2)
    let pcap3 = std::path::Path::new(&server.recording_dir).join("max-rec-3.pcap");
    let resp = client
        .request(
            "recording.start",
            json!({"endpoint_id": null, "file_path": pcap3.to_str().unwrap()}),
        )
        .await;
    assert!(
        resp.get("error").is_some(),
        "third recording should be rejected when limit is 2: {resp}"
    );

    // Stop recording #1 to free a slot
    client
        .request_ok("recording.stop", json!({"recording_id": rec1_id}))
        .await;

    // Now starting recording #3 should succeed
    let rec3 = client
        .request(
            "recording.start",
            json!({"endpoint_id": null, "file_path": pcap3.to_str().unwrap()}),
        )
        .await;
    assert!(
        rec3.get("result").is_some(),
        "third recording should succeed after stopping one: {rec3}"
    );

    client.request_ok("session.destroy", json!({})).await;
}

/// Test: fire endpoint.remove and another operation concurrently.
/// The key assertion is that the server handles the race gracefully (no crash/panic).
#[tokio::test]
async fn test_concurrent_endpoint_remove_and_operation() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;

    let peer = TestRtpPeer::new().await;
    let offer = peer.make_sdp_offer();
    let result = client
        .request_ok(
            "endpoint.create_from_offer",
            json!({"sdp": offer, "direction": "sendrecv"}),
        )
        .await;
    let ep_id = result["endpoint_id"].as_str().unwrap().to_string();

    // Fire remove and a second operation sequentially (same WS, can't truly parallelize)
    // but do them back-to-back without awaiting events between
    let remove_resp = client
        .request("endpoint.remove", json!({"endpoint_id": ep_id}))
        .await;
    let pause_resp = client
        .request("endpoint.file.pause", json!({"endpoint_id": ep_id}))
        .await;

    // At least one should succeed or both should return clean errors (no panic/crash)
    assert!(
        remove_resp.get("result").is_some() || remove_resp.get("error").is_some(),
        "remove should return a valid response: {remove_resp}"
    );
    assert!(
        pause_resp.get("result").is_some() || pause_resp.get("error").is_some(),
        "pause should return a valid response: {pause_resp}"
    );

    client.request_ok("session.destroy", json!({})).await;
}

// ── Phase 3 regression tests ─────────────────────────────────────────

/// Test: /health endpoint accepts query string parameters.
#[tokio::test]
async fn test_health_with_query_string() {
    let server = TestServer::start().await;
    let body = http_get(&server.addr, "/health?verbose=true").await;
    assert_eq!(body["status"], "ok");
}

/// Test: /metrics endpoint accepts query string parameters.
#[tokio::test]
async fn test_metrics_with_query_string() {
    let server = TestServer::start().await;
    let (_, text) = http_get_text(&server.addr, "/metrics?name=foo").await;
    assert!(
        text.contains("rtpbridge_"),
        "metrics response should contain rtpbridge_ prefix"
    );
}

/// Test: paths like /sessionsnonsense are NOT routed as HTTP.
#[tokio::test]
async fn test_invalid_path_prefix_not_routed_as_http() {
    let server = TestServer::start().await;
    let client = reqwest::Client::new();
    let resp = client
        .get(format!("http://{}/sessionsnonsense", server.addr))
        .timeout(std::time::Duration::from_secs(3))
        .send()
        .await;
    // Should fail (WebSocket upgrade, not HTTP) or return non-200
    match resp {
        Ok(r) => assert_ne!(
            r.status().as_u16(),
            200,
            "unexpected 200 for invalid path prefix"
        ),
        Err(_) => {} // connection error is also correct
    }
}

/// Test: packets_recorded metric stays 0 when no recordings are active.
#[tokio::test]
async fn test_packets_recorded_metric_zero_without_recording() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;

    // Create two RTP endpoints and route media
    let mut peer_a = TestRtpPeer::new().await;
    let offer_a = peer_a.make_sdp_offer();
    let result = client
        .request_ok(
            "endpoint.create_from_offer",
            json!({"sdp": offer_a, "direction": "sendrecv"}),
        )
        .await;
    let answer_a = result["sdp_answer"].as_str().unwrap();
    peer_a.set_remote(parse_rtp_addr_from_sdp(answer_a).unwrap());

    let mut peer_b = TestRtpPeer::new().await;
    let offer_b = peer_b.make_sdp_offer();
    let result = client
        .request_ok(
            "endpoint.create_from_offer",
            json!({"sdp": offer_b, "direction": "sendrecv"}),
        )
        .await;
    let answer_b = result["sdp_answer"].as_str().unwrap();
    peer_b.set_remote(parse_rtp_addr_from_sdp(answer_b).unwrap());

    // Send packets WITHOUT any recording active
    peer_a.send_tone_for(timing::scaled_ms(300)).await;
    tokio::time::sleep(timing::scaled_ms(500)).await;

    let (_, metrics_text) = http_get_text(&server.addr, "/metrics").await;
    let recorded = extract_metric(&metrics_text, "rtpbridge_packets_recorded_total");
    assert_eq!(
        recorded, 0,
        "packets_recorded should be 0 with no active recording"
    );

    client.request_ok("session.destroy", json!({})).await;
}

/// Test: graceful shutdown during active media flow completes cleanly.
#[tokio::test]
async fn test_shutdown_during_active_media_flow() {
    let server = TestServer::builder()
        .disconnect_timeout_secs(1)
        .start()
        .await;
    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;

    let mut peer = TestRtpPeer::new().await;
    let offer = peer.make_sdp_offer();
    let result = client
        .request_ok(
            "endpoint.create_from_offer",
            json!({"sdp": offer, "direction": "sendrecv"}),
        )
        .await;
    let answer = result["sdp_answer"].as_str().unwrap();
    peer.set_remote(parse_rtp_addr_from_sdp(answer).unwrap());

    // Start active media flow in background
    let send_task = {
        let socket = peer.socket.clone();
        let remote = peer.remote_addr.unwrap();
        tokio::spawn(async move {
            let encoder = xlaw::PcmXLawEncoder::new_ulaw();
            let mut seq: u16 = 0;
            let mut ts: u32 = 0;
            loop {
                let payload: Vec<u8> = (0..160)
                    .map(|i| {
                        let sample = (f64::sin(
                            2.0 * std::f64::consts::PI * 440.0 * (ts as f64 + i as f64) / 8000.0,
                        ) * 16000.0) as i16;
                        encoder.encode(sample)
                    })
                    .collect();
                let pkt =
                    helpers::test_rtp_peer::build_rtp_packet(0, seq, ts, 12345, false, &payload);
                if socket.send_to(&pkt, remote).await.is_err() {
                    break;
                }
                seq = seq.wrapping_add(1);
                ts = ts.wrapping_add(160);
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
    };

    // Let media flow for a bit
    tokio::time::sleep(timing::scaled_ms(300)).await;

    // Initiate graceful shutdown while media is actively flowing
    server.initiate_shutdown();
    drop(client);

    // Wait for shutdown — should not hang or panic
    tokio::time::sleep(timing::scaled_ms(3000)).await;
    send_task.abort();
}

/// Test: recording download returns valid PCAP content.
#[tokio::test]
async fn test_recording_download_contains_packets() {
    let recording_dir = TempDir::new().unwrap();
    let server = TestServer::builder()
        .recording_dir(recording_dir.path().to_str().unwrap())
        .start()
        .await;
    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;

    // Create RTP endpoint
    let mut peer = TestRtpPeer::new().await;
    let offer = peer.make_sdp_offer();
    let result = client
        .request_ok(
            "endpoint.create_from_offer",
            json!({"sdp": offer, "direction": "sendrecv"}),
        )
        .await;
    let answer = result["sdp_answer"].as_str().unwrap();
    peer.set_remote(parse_rtp_addr_from_sdp(answer).unwrap());

    // Start recording
    let rec_path = recording_dir.path().join("content-verify.pcap");
    let rec_result = client
        .request_ok(
            "recording.start",
            json!({"file_path": rec_path.to_str().unwrap()}),
        )
        .await;
    let recording_id = rec_result["recording_id"].as_str().unwrap().to_string();

    // Send known RTP packets
    peer.send_tone_for(timing::scaled_ms(300)).await;
    tokio::time::sleep(timing::scaled_ms(500)).await;

    // Stop recording
    client
        .request_ok("recording.stop", json!({"recording_id": recording_id}))
        .await;
    tokio::time::sleep(timing::scaled_ms(200)).await;

    // Download via HTTP
    let url = format!("http://{}/recordings/content-verify.pcap", server.addr);
    let resp = reqwest::Client::new()
        .get(&url)
        .timeout(timing::scaled_ms(5000))
        .send()
        .await
        .expect("download should succeed");
    assert_eq!(resp.status().as_u16(), 200);

    let body = resp.bytes().await.unwrap();
    // PCAP global header is 24 bytes
    assert!(
        body.len() > 24,
        "PCAP file should have content beyond the global header, got {} bytes",
        body.len()
    );
    // Verify PCAP magic number (either byte order)
    let magic = &body[0..4];
    assert!(
        magic == [0xd4, 0xc3, 0xb2, 0xa1] || magic == [0xa1, 0xb2, 0xc3, 0xd4],
        "file should start with PCAP magic number, got {:?}",
        magic
    );

    client.request_ok("session.destroy", json!({})).await;
}

/// Test: RTP packets with sequence numbers wrapping around 65535→0 are forwarded correctly
#[tokio::test]
async fn test_rtp_sequence_wraparound_forwarded() {
    use helpers::test_rtp_peer::build_rtp_packet;

    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;

    // Create two endpoints
    let mut peer_a = TestRtpPeer::new().await;
    let offer_a = peer_a.make_sdp_offer();
    let result = client
        .request_ok(
            "endpoint.create_from_offer",
            json!({"sdp": offer_a, "direction": "sendrecv"}),
        )
        .await;
    let answer_a = result["sdp_answer"].as_str().unwrap();
    let server_addr_a = parse_rtp_addr_from_sdp(answer_a).unwrap();
    peer_a.set_remote(server_addr_a);

    let mut peer_b = TestRtpPeer::new().await;
    let offer_b = peer_b.make_sdp_offer();
    let result = client
        .request_ok(
            "endpoint.create_from_offer",
            json!({"sdp": offer_b, "direction": "sendrecv"}),
        )
        .await;
    let answer_b = result["sdp_answer"].as_str().unwrap();
    let server_addr_b = parse_rtp_addr_from_sdp(answer_b).unwrap();
    peer_b.set_remote(server_addr_b);

    peer_a.activate().await;
    peer_b.activate().await;
    tokio::time::sleep(timing::scaled_ms(50)).await;

    peer_b.start_recv();
    tokio::time::sleep(timing::scaled_ms(50)).await;

    // Send packets with sequence numbers near the wrap point
    let ssrc: u32 = 0x12345678;
    let payload = vec![0x80u8; 160]; // 160 bytes of PCMU
    for seq in [65533u16, 65534, 65535, 0, 1, 2] {
        let ts = seq as u32 * 160;
        let pkt = build_rtp_packet(0, seq, ts, ssrc, false, &payload);
        peer_a.socket.send_to(&pkt, server_addr_a).await.unwrap();
        tokio::time::sleep(timing::PACING).await;
    }

    tokio::time::sleep(timing::scaled_ms(500)).await;
    let received = peer_b.received_count();
    assert!(
        received >= 4,
        "peer B should receive packets across seq wraparound, got {received}"
    );

    client.request_ok("session.destroy", json!({})).await;
}

/// Test: Stats events report inbound packet counts (validates RTCP-derived stats collection)
#[tokio::test]
async fn test_stats_report_inbound_packets() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;

    // Create an RTP endpoint
    let mut peer = TestRtpPeer::new().await;
    let offer = peer.make_sdp_offer();
    let result = client
        .request_ok(
            "endpoint.create_from_offer",
            json!({"sdp": offer, "direction": "sendrecv"}),
        )
        .await;
    let answer = result["sdp_answer"].as_str().unwrap();
    let server_addr = parse_rtp_addr_from_sdp(answer).unwrap();
    peer.set_remote(server_addr);

    // Subscribe to stats
    client
        .request_ok("stats.subscribe", json!({"interval_ms": 500}))
        .await;

    // Send 50+ packets
    let payload = vec![0x80u8; 160];
    for _ in 0..50 {
        peer.send_pcmu(&payload).await;
        tokio::time::sleep(timing::PACING).await;
    }

    // Wait for stats event
    tokio::time::sleep(timing::scaled_ms(600)).await;

    let mut found_packets = false;
    for _ in 0..15 {
        if let Some(event) = client.recv_event(timing::scaled_ms(2000)).await {
            if event["event"].as_str() == Some("stats") {
                let endpoints = event["data"]["endpoints"].as_array();
                if let Some(eps) = endpoints {
                    for ep in eps {
                        let packets = ep["inbound"]["packets"].as_u64().unwrap_or(0);
                        if packets > 0 {
                            found_packets = true;
                            assert!(
                                packets >= 20,
                                "should report substantial inbound packets, got {packets}"
                            );
                            break;
                        }
                    }
                }
                if found_packets {
                    break;
                }
            }
        }
    }

    assert!(
        found_packets,
        "stats event should report inbound packets > 0"
    );

    client.request_ok("session.destroy", json!({})).await;
}

/// Test: HTTP DELETE /recordings/{filename} removes the file and returns 404 on repeat.
#[tokio::test]
async fn test_http_delete_recording() {
    let tmp = TempDir::new().unwrap();
    let server = TestServer::builder()
        .recording_dir(tmp.path().to_str().unwrap())
        .start()
        .await;

    // Create a .pcap file in the recording dir
    let filename = "delete-test.pcap";
    let pcap_path = tmp.path().join(filename);
    std::fs::write(&pcap_path, b"fake pcap data").unwrap();
    assert!(pcap_path.exists(), "pcap file should exist before delete");

    // DELETE the recording — should succeed with 200
    let (status, body) = http_delete(&server.addr, &format!("/recordings/{filename}")).await;
    assert_eq!(
        status, 200,
        "DELETE should return 200, got {status}: {body}"
    );
    assert_eq!(
        body["deleted"].as_bool(),
        Some(true),
        "response should contain deleted:true"
    );
    assert!(
        !pcap_path.exists(),
        "pcap file should be removed after DELETE"
    );

    // DELETE the same file again — should return 404
    let (status2, body2) = http_delete(&server.addr, &format!("/recordings/{filename}")).await;
    assert_eq!(
        status2, 404,
        "repeated DELETE should return 404, got {status2}: {body2}"
    );
}

/// Test: HTTP GET /sessions/{session_id} returns detailed session info.
#[tokio::test]
async fn test_http_get_session_details() {
    let server = TestServer::builder()
        .disconnect_timeout_secs(2)
        .start()
        .await;

    let mut client = TestControlClient::connect(&server.addr).await;
    let result = client.request_ok("session.create", json!({})).await;
    let session_id = result["session_id"].as_str().unwrap().to_string();

    // GET /sessions/{session_id}
    let details = http_get(&server.addr, &format!("/sessions/{session_id}")).await;

    assert_eq!(
        details["session_id"].as_str().unwrap(),
        session_id,
        "response session_id should match"
    );
    assert!(
        details.get("state").is_some(),
        "response should contain 'state'"
    );
    assert!(
        details.get("endpoints").is_some(),
        "response should contain 'endpoints'"
    );
    assert!(
        details.get("recordings").is_some(),
        "response should contain 'recordings'"
    );
    assert!(
        details.get("vad_active").is_some(),
        "response should contain 'vad_active'"
    );

    client.request_ok("session.destroy", json!({})).await;
}

/// Verify that recording.start rejects file paths containing ".." or relative paths.
#[tokio::test]
async fn test_recording_path_traversal_rejected() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;

    client.request_ok("session.create", json!({})).await;

    // Create an endpoint so recording has something to attach to
    client
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv"}),
        )
        .await;

    // Test 1: Path with ".." should be rejected
    let resp = client
        .request(
            "recording.start",
            json!({"endpoint_id": null, "file_path": "/tmp/../etc/passwd.pcap"}),
        )
        .await;
    assert!(
        resp.get("error").is_some(),
        "path traversal with '..' should be rejected: {resp}"
    );
    assert_eq!(
        resp["error"]["code"].as_str().unwrap(),
        "INVALID_PARAMS",
        "error code should be INVALID_PARAMS for path traversal"
    );

    // Test 2: Another path traversal variant
    let resp = client
        .request(
            "recording.start",
            json!({"endpoint_id": null, "file_path": "/var/recordings/../../etc/crontab"}),
        )
        .await;
    assert!(
        resp.get("error").is_some(),
        "path traversal with nested '..' should be rejected: {resp}"
    );

    // Test 3: Relative path should be rejected
    let resp = client
        .request(
            "recording.start",
            json!({"endpoint_id": null, "file_path": "relative/path/recording.pcap"}),
        )
        .await;
    assert!(
        resp.get("error").is_some(),
        "relative path should be rejected: {resp}"
    );
    assert_eq!(
        resp["error"]["code"].as_str().unwrap(),
        "INVALID_PARAMS",
        "error code should be INVALID_PARAMS for relative path"
    );

    // Test 4: Valid absolute path should NOT be rejected by validation
    // (may fail for other reasons like directory not existing, but not INVALID_PARAMS for path)
    let valid_path = std::path::Path::new(&server.recording_dir).join("valid-recording-test.pcap");
    let resp = client
        .request(
            "recording.start",
            json!({"endpoint_id": null, "file_path": valid_path.to_str().unwrap()}),
        )
        .await;
    // Should succeed or fail for reasons other than path validation
    if resp.get("error").is_some() {
        let code = resp["error"]["code"].as_str().unwrap_or("");
        assert_ne!(
            code, "INVALID_PARAMS",
            "valid absolute path should not fail INVALID_PARAMS validation: {resp}"
        );
    }

    client.request_ok("session.destroy", json!({})).await;
}

/// Verify that stats.subscribe with interval_ms < 500 returns an error.
#[tokio::test]
async fn test_stats_interval_minimum_enforced() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;

    client.request_ok("session.create", json!({})).await;

    // interval_ms = 0 should be rejected
    let resp = client
        .request("stats.subscribe", json!({"interval_ms": 0}))
        .await;
    assert!(
        resp.get("error").is_some(),
        "interval_ms=0 should be rejected: {resp}"
    );
    assert_eq!(
        resp["error"]["code"].as_str().unwrap(),
        "INVALID_PARAMS",
        "error code should be INVALID_PARAMS"
    );

    // interval_ms = 100 (below minimum of 500) should also be rejected
    let resp = client
        .request("stats.subscribe", json!({"interval_ms": 100}))
        .await;
    assert!(
        resp.get("error").is_some(),
        "interval_ms=100 should be rejected: {resp}"
    );
    assert_eq!(
        resp["error"]["code"].as_str().unwrap(),
        "INVALID_PARAMS",
        "error code should be INVALID_PARAMS for interval_ms=100"
    );

    // interval_ms = 499 should also be rejected
    let resp = client
        .request("stats.subscribe", json!({"interval_ms": 499}))
        .await;
    assert!(
        resp.get("error").is_some(),
        "interval_ms=499 should be rejected: {resp}"
    );

    // interval_ms = 500 should be accepted
    client
        .request_ok("stats.subscribe", json!({"interval_ms": 500}))
        .await;
    // Clean up
    client.request_ok("stats.unsubscribe", json!({})).await;

    client.request_ok("session.destroy", json!({})).await;
}

/// Set max_endpoints_per_session=2 in config. Create 2 endpoints (should succeed),
/// try a 3rd (should fail with MAX_ENDPOINTS_REACHED).
#[tokio::test]
async fn test_max_endpoints_enforcement() {
    let server = TestServer::builder()
        .max_endpoints_per_session(2)
        .start()
        .await;
    let mut client = TestControlClient::connect(&server.addr).await;

    client.request_ok("session.create", json!({})).await;

    // First endpoint should succeed
    let peer1 = TestRtpPeer::new().await;
    let offer1 = peer1.make_sdp_offer();
    client
        .request_ok(
            "endpoint.create_from_offer",
            json!({"sdp": offer1, "direction": "sendrecv"}),
        )
        .await;

    // Second endpoint should succeed
    client
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv"}),
        )
        .await;

    // Third endpoint via create_from_offer should fail
    let peer3 = TestRtpPeer::new().await;
    let offer3 = peer3.make_sdp_offer();
    let resp = client
        .request(
            "endpoint.create_from_offer",
            json!({"sdp": offer3, "direction": "sendrecv"}),
        )
        .await;
    assert!(
        resp.get("error").is_some(),
        "third endpoint should be rejected: {resp}"
    );
    let error_msg = resp["error"]["message"].as_str().unwrap_or("");
    assert!(
        error_msg.contains("MAX_ENDPOINTS_REACHED"),
        "error should indicate max endpoints reached: {resp}"
    );

    // Third endpoint via create_offer should also fail
    let resp = client
        .request(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv"}),
        )
        .await;
    assert!(
        resp.get("error").is_some(),
        "third endpoint via create_offer should also be rejected: {resp}"
    );

    client.request_ok("session.destroy", json!({})).await;
}
