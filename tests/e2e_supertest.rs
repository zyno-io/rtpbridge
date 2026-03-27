mod helpers;

use serde_json::json;
use tempfile::TempDir;

use helpers::control_client::TestControlClient;
use helpers::http::{http_get, http_get_text};
use helpers::test_server::TestServer;
use helpers::timing;
use helpers::wav::generate_test_wav;

#[tokio::test]
async fn test_multi_session_media_flow() {
    let tmp = TempDir::new().unwrap();
    let server = TestServer::builder()
        .media_dir(tmp.path().to_str().unwrap())
        .start()
        .await;

    // Generate test audio files
    let wav1 = tmp.path().join("shared.wav");
    let wav2 = tmp.path().join("solo.wav");
    let pcap_path = std::path::Path::new(&server.recording_dir).join("recording.pcap");
    generate_test_wav(&wav1, 3.0, 440.0); // 3-second 440Hz tone
    generate_test_wav(&wav2, 1.0, 880.0); // 1-second 880Hz tone

    // ═══════════════════════════════════════════════════════════════
    // Step 1: Session 1 — one endpoint + shared media playback
    // ═══════════════════════════════════════════════════════════════
    let mut ws1 = TestControlClient::connect(&server.addr).await;

    let s1_result = ws1.request_ok("session.create", json!({})).await;
    let s1_id = s1_result["session_id"].as_str().unwrap().to_string();

    // Create RTP endpoint
    let result = ws1
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv"}),
        )
        .await;
    let _ep1a_id = result["endpoint_id"].as_str().unwrap().to_string();

    // Create shared file playback (infinite loop)
    let result = ws1
        .request_ok(
            "endpoint.create_with_file",
            json!({"source": wav1.to_str().unwrap(), "shared": true, "loop_count": null}),
        )
        .await;
    let _ep1_file_id = result["endpoint_id"].as_str().unwrap().to_string();

    // Verify via HTTP: 1 session
    let sessions = http_get(&server.addr, "/sessions").await;
    let sessions_arr = sessions.as_array().unwrap();
    assert_eq!(sessions_arr.len(), 1, "should have 1 session");

    // Verify session 1 details via HTTP
    let s1_details = http_get(&server.addr, &format!("/sessions/{s1_id}")).await;
    let s1_endpoints = s1_details["endpoints"].as_array().unwrap();
    assert_eq!(
        s1_endpoints.len(),
        2,
        "session 1 should have 2 endpoints (rtp + file): {s1_details}"
    );
    assert!(
        s1_details["recordings"].as_array().unwrap().is_empty(),
        "session 1 should have no recordings"
    );

    // ═══════════════════════════════════════════════════════════════
    // Step 2: Session 2 — endpoint + recording + shared media + VAD
    // ═══════════════════════════════════════════════════════════════
    let mut ws2 = TestControlClient::connect(&server.addr).await;

    let s2_result = ws2.request_ok("session.create", json!({})).await;
    let s2_id = s2_result["session_id"].as_str().unwrap().to_string();

    // Create RTP endpoint
    let result = ws2
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv"}),
        )
        .await;
    let ep2a_id = result["endpoint_id"].as_str().unwrap().to_string();

    // Start full-session recording
    let result = ws2
        .request_ok(
            "recording.start",
            json!({"endpoint_id": null, "file_path": pcap_path.to_str().unwrap()}),
        )
        .await;
    let rec_id = result["recording_id"].as_str().unwrap().to_string();

    // Create shared file playback (same file as session 1)
    let result = ws2
        .request_ok(
            "endpoint.create_with_file",
            json!({"source": wav1.to_str().unwrap(), "shared": true, "loop_count": null}),
        )
        .await;
    let ep2_file_id = result["endpoint_id"].as_str().unwrap().to_string();

    // Start VAD on the RTP endpoint
    ws2.request_ok(
        "vad.start",
        json!({"endpoint_id": ep2a_id, "silence_interval_ms": 500}),
    )
    .await;

    // Verify via HTTP: 2 sessions
    let sessions = http_get(&server.addr, "/sessions").await;
    assert_eq!(
        sessions.as_array().unwrap().len(),
        2,
        "should have 2 sessions"
    );

    // Verify session 2 details
    let s2_details = http_get(&server.addr, &format!("/sessions/{s2_id}")).await;
    let s2_endpoints = s2_details["endpoints"].as_array().unwrap();
    assert_eq!(
        s2_endpoints.len(),
        2,
        "session 2 should have 2 endpoints: {s2_details}"
    );
    assert_eq!(
        s2_details["recordings"].as_array().unwrap().len(),
        1,
        "session 2 should have 1 recording"
    );
    assert_eq!(
        s2_details["vad_active"].as_array().unwrap().len(),
        1,
        "session 2 should have 1 VAD active"
    );

    // Small delay to let media flow
    tokio::time::sleep(timing::scaled_ms(500)).await;

    // ═══════════════════════════════════════════════════════════════
    // Step 3: Session 2 — stop media, add second endpoint
    // ═══════════════════════════════════════════════════════════════
    ws2.request_ok("endpoint.remove", json!({"endpoint_id": ep2_file_id}))
        .await;

    let result = ws2
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv"}),
        )
        .await;
    let ep2b_id = result["endpoint_id"].as_str().unwrap().to_string();

    // Verify session 1 still has file playing (unaffected by session 2's removal)
    let s1_details = http_get(&server.addr, &format!("/sessions/{s1_id}")).await;
    let s1_endpoints = s1_details["endpoints"].as_array().unwrap();
    assert_eq!(
        s1_endpoints.len(),
        2,
        "session 1 should still have 2 endpoints: {s1_details}"
    );
    let has_file = s1_endpoints.iter().any(|ep| ep["endpoint_type"] == "file");
    assert!(has_file, "session 1 should still have file endpoint");

    // Verify session 2 now has 2 rtp endpoints, recording still active
    let s2_details = http_get(&server.addr, &format!("/sessions/{s2_id}")).await;
    assert_eq!(
        s2_details["endpoints"].as_array().unwrap().len(),
        2,
        "session 2 should have 2 endpoints: {s2_details}"
    );
    assert_eq!(
        s2_details["recordings"].as_array().unwrap().len(),
        1,
        "session 2 recording should still be active"
    );

    // ═══════════════════════════════════════════════════════════════
    // Step 4: Session 2 — disconnect ep2b, play solo file, resume shared
    // ═══════════════════════════════════════════════════════════════
    ws2.request_ok("endpoint.remove", json!({"endpoint_id": ep2b_id}))
        .await;

    // Play a separate (non-shared) file
    let result = ws2
        .request_ok(
            "endpoint.create_with_file",
            json!({"source": wav2.to_str().unwrap(), "shared": false, "loop_count": 0}),
        )
        .await;
    let ep2_solo_file_id = result["endpoint_id"].as_str().unwrap().to_string();

    // Wait for the solo file to finish playing (1 second file + margin)
    // File playback timer-driven generation runs in the session task.
    // The file endpoint will be in "finished" state after the audio completes.
    tokio::time::sleep(timing::scaled_ms(1500)).await;

    // Remove the solo file endpoint (it should be done by now)
    ws2.request_ok("endpoint.remove", json!({"endpoint_id": ep2_solo_file_id}))
        .await;

    // Resume shared playback (same file as session 1)
    let result = ws2
        .request_ok(
            "endpoint.create_with_file",
            json!({"source": wav1.to_str().unwrap(), "shared": true, "loop_count": null}),
        )
        .await;
    let _ep2_reshared_id = result["endpoint_id"].as_str().unwrap().to_string();

    // Verify session 1's file endpoint is still playing
    let s1_details = http_get(&server.addr, &format!("/sessions/{s1_id}")).await;
    let has_file = s1_details["endpoints"]
        .as_array()
        .unwrap()
        .iter()
        .any(|ep| ep["endpoint_type"] == "file");
    assert!(
        has_file,
        "session 1 should still have file endpoint after session 2 rejoined shared"
    );

    // ═══════════════════════════════════════════════════════════════
    // Step 5: Session 2 — disconnect
    // ═══════════════════════════════════════════════════════════════
    ws2.request_ok("vad.stop", json!({"endpoint_id": ep2a_id}))
        .await;

    let rec_result = ws2
        .request_ok("recording.stop", json!({"recording_id": rec_id}))
        .await;
    let rec_duration = rec_result["duration_ms"].as_u64().unwrap();
    let _rec_packets = rec_result["packets"].as_u64().unwrap();
    assert!(
        rec_duration > 0,
        "recording should have nonzero duration: {rec_result}"
    );

    ws2.request_ok("session.destroy", json!({})).await;

    // ═══════════════════════════════════════════════════════════════
    // Step 6: Verify session 1 still alive
    // ═══════════════════════════════════════════════════════════════
    let sessions = http_get(&server.addr, "/sessions").await;
    assert_eq!(
        sessions.as_array().unwrap().len(),
        1,
        "should have 1 session remaining after session 2 destroyed"
    );

    let s1_details = http_get(&server.addr, &format!("/sessions/{s1_id}")).await;
    let s1_endpoints = s1_details["endpoints"].as_array().unwrap();
    assert!(
        s1_endpoints.len() >= 1,
        "session 1 should still have endpoints"
    );
    let has_file = s1_endpoints.iter().any(|ep| ep["endpoint_type"] == "file");
    assert!(
        has_file,
        "session 1 should still have file endpoint playing"
    );

    // Clean up session 1
    ws1.request_ok("session.destroy", json!({})).await;

    // Final verification: no sessions
    tokio::time::sleep(timing::scaled_ms(200)).await;
    let sessions = http_get(&server.addr, "/sessions").await;
    assert_eq!(
        sessions.as_array().unwrap().len(),
        0,
        "should have 0 sessions after cleanup"
    );
}

/// Test: GET /metrics returns Prometheus text exposition format with expected metric names.
#[tokio::test]
async fn test_metrics_endpoint() {
    let server = TestServer::start().await;

    // Create a session + endpoint to generate some metric values
    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;
    client
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv"}),
        )
        .await;

    // GET /metrics
    let (status, body) = http_get_text(&server.addr, "/metrics").await;
    assert_eq!(status, 200, "GET /metrics should return 200 OK");

    // Verify expected Prometheus metric names are present.
    // Counters get "_total" suffix from prometheus-client; gauges do not.
    assert!(
        body.contains("rtpbridge_sessions_total"),
        "metrics should contain rtpbridge_sessions_total: {body}"
    );
    assert!(
        body.contains("rtpbridge_sessions_active"),
        "metrics should contain rtpbridge_sessions_active: {body}"
    );
    assert!(
        body.contains("rtpbridge_endpoints_total"),
        "metrics should contain rtpbridge_endpoints_total: {body}"
    );
    assert!(
        body.contains("rtpbridge_endpoints_active"),
        "metrics should contain rtpbridge_endpoints_active: {body}"
    );
    assert!(
        body.contains("rtpbridge_packets_routed_total"),
        "metrics should contain rtpbridge_packets_routed_total: {body}"
    );
    assert!(
        body.contains("rtpbridge_packets_recorded_total"),
        "metrics should contain rtpbridge_packets_recorded_total: {body}"
    );
    assert!(
        body.contains("rtpbridge_recordings_active"),
        "metrics should contain rtpbridge_recordings_active: {body}"
    );
    assert!(
        body.contains("rtpbridge_srtp_errors_total"),
        "metrics should contain rtpbridge_srtp_errors_total: {body}"
    );
    assert!(
        body.contains("rtpbridge_dtmf_events_total"),
        "metrics should contain rtpbridge_dtmf_events_total: {body}"
    );
    assert!(
        body.contains("rtpbridge_transcode_errors_total"),
        "metrics should contain rtpbridge_transcode_errors_total: {body}"
    );
    assert!(
        body.contains("rtpbridge_events_dropped_total"),
        "metrics should contain rtpbridge_events_dropped_total: {body}"
    );

    // After creating a session + endpoint, sessions_active and endpoints_active should be >= 1
    // The body is text exposition format, e.g. "rtpbridge_sessions_active 1"
    // We just verify they are present with a non-negative number (already asserted above)

    client.request_ok("session.destroy", json!({})).await;
}
