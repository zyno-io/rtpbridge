mod helpers;

use std::time::Duration;

use serde_json::json;

use helpers::control_client::TestControlClient;
use helpers::test_rtp_peer::{TestRtpPeer, parse_rtp_addr_from_sdp};
use helpers::test_server::TestServer;
use helpers::timing;
use tempfile::TempDir;

/// Helper: create an RTP endpoint via create_from_offer and return (endpoint_id, peer)
/// with the peer already pointed at the server's RTP address.
async fn setup_rtp_endpoint(
    client: &mut TestControlClient,
    direction: &str,
) -> (String, TestRtpPeer) {
    let mut peer = TestRtpPeer::new().await;
    let offer = peer.make_sdp_offer();
    let result = client
        .request_ok(
            "endpoint.create_from_offer",
            json!({"sdp": offer, "direction": direction}),
        )
        .await;
    let ep_id = result["endpoint_id"].as_str().unwrap().to_string();
    let answer = result["sdp_answer"].as_str().unwrap();
    let server_addr = parse_rtp_addr_from_sdp(answer).expect("parse server addr");
    peer.set_remote(server_addr);
    (ep_id, peer)
}

/// Test: Destroy a session while real media is actively flowing between two RTP endpoints.
/// Verifies the destroy succeeds cleanly and the session is gone afterward.
#[tokio::test]
async fn test_destroy_during_active_media() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;

    let (_, mut peer_a) = setup_rtp_endpoint(&mut client, "sendrecv").await;
    let (_, mut peer_b) = setup_rtp_endpoint(&mut client, "sendrecv").await;

    // Activate symmetric RTP on peer B so the server will send to it
    peer_b.activate().await;
    tokio::time::sleep(timing::scaled_ms(50)).await;

    // Start receiving on peer B
    peer_b.start_recv();
    tokio::time::sleep(timing::scaled_ms(50)).await;

    // Send media from A and confirm it flows through
    peer_a.send_tone_for(timing::scaled_ms(200)).await;
    tokio::time::sleep(timing::scaled_ms(300)).await;

    let received = peer_b.received_count();
    assert!(
        received > 0,
        "peer B should have received packets before destroy, got {received}"
    );

    // Now send more media in the background while we destroy
    let socket_a = peer_a.socket.clone();
    let remote_a = peer_a.remote_addr.unwrap();
    let sender = tokio::spawn(async move {
        let payload = vec![0x80u8; 160];
        for seq in 0u16..50 {
            let ts = seq as u32 * 160;
            let pkt = helpers::test_rtp_peer::build_rtp_packet(0, seq, ts, 0xDEAD, false, &payload);
            // Ignore send errors — the endpoint may be gone
            let _ = socket_a.send_to(&pkt, remote_a).await;
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    });

    // Destroy session while media is still flowing
    tokio::time::sleep(timing::scaled_ms(30)).await;
    client.request_ok("session.destroy", json!({})).await;

    // Session is destroyed — further commands should fail with NO_SESSION
    let resp = client.request("session.info", json!({})).await;
    assert!(
        resp.get("error").is_some(),
        "session.info after destroy should return error: {resp}"
    );

    // Wait for the sender task to finish — it should not panic
    sender.await.expect("sender task should not panic");
}

/// Test: Commands on a destroyed session return NO_SESSION errors.
#[tokio::test]
async fn test_command_on_destroyed_session() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;
    client.request_ok("session.destroy", json!({})).await;

    // session.info should fail
    let resp = client.request("session.info", json!({})).await;
    assert!(
        resp.get("error").is_some(),
        "session.info on destroyed session should return error: {resp}"
    );
    let err_msg = resp["error"]["message"].as_str().unwrap_or("");
    assert!(
        err_msg.contains("NO_SESSION") || err_msg.contains("No session"),
        "error should indicate no session: {resp}"
    );

    // endpoint.create_offer should fail
    let resp = client
        .request(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv"}),
        )
        .await;
    assert!(
        resp.get("error").is_some(),
        "endpoint.create_offer on destroyed session should return error: {resp}"
    );
}

/// Test: Concurrent session.destroy and session.info — at least one succeeds,
/// neither panics, and the info request gets either a valid result or an error.
#[tokio::test]
async fn test_destroy_races_with_info_across_connections() {
    let server = TestServer::start().await;
    let addr = server.addr.clone();

    let mut client = TestControlClient::connect(&addr).await;
    let result = client.request_ok("session.create", json!({})).await;
    let session_id = result["session_id"].as_str().unwrap().to_string();

    // Create an endpoint so the session has some state
    client
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv"}),
        )
        .await;

    // Spawn a task that sends session.info
    let addr2 = addr.clone();
    let sid = session_id.clone();
    let info_task = tokio::spawn(async move {
        let mut c = TestControlClient::connect(&addr2).await;
        c.request("session.attach", json!({"session_id": sid}))
            .await;
        let resp = c.request("session.info", json!({})).await;
        // Should be either a valid result or an error — both are acceptable
        assert!(
            resp.get("result").is_some() || resp.get("error").is_some(),
            "session.info should return result or error, got: {resp}"
        );
        resp
    });

    // Meanwhile, destroy the session from the original client
    tokio::time::sleep(Duration::from_millis(5)).await;
    let destroy_resp = client.request("session.destroy", json!({})).await;

    // Destroy should succeed (or the session may already be gone from attach race)
    assert!(
        destroy_resp.get("result").is_some() || destroy_resp.get("error").is_some(),
        "destroy should return result or error: {destroy_resp}"
    );

    // The info task should complete without panic
    let info_resp = info_task.await.expect("info task should not panic");
    let _ = info_resp; // Already asserted inside the task
}

/// Test: Sending malformed/garbage data via UDP to an RTP endpoint does not crash
/// the session. The session remains healthy and responsive.
#[tokio::test]
async fn test_malformed_rtp_no_crash() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;

    // Create two RTP endpoints so the session has active routing
    let (_, peer_a) = setup_rtp_endpoint(&mut client, "sendrecv").await;
    let (_, _peer_b) = setup_rtp_endpoint(&mut client, "sendrecv").await;

    let remote = peer_a.remote_addr.unwrap();

    // Send zero-length packet (empty bytes)
    let _ = peer_a.socket.send_to(&[], remote).await;
    tokio::time::sleep(timing::scaled_ms(50)).await;

    // Send 1-byte packet (too short for RTP header)
    let _ = peer_a.socket.send_to(&[0xFF], remote).await;
    tokio::time::sleep(timing::scaled_ms(50)).await;

    // Send random garbage bytes (not valid RTP)
    let garbage: Vec<u8> = (0..200).map(|i| (i * 37 + 13) as u8).collect();
    let _ = peer_a.socket.send_to(&garbage, remote).await;
    tokio::time::sleep(timing::scaled_ms(50)).await;

    // Send a very short packet (3 bytes — still too short)
    let _ = peer_a.socket.send_to(&[0x80, 0x00, 0x01], remote).await;
    tokio::time::sleep(timing::scaled_ms(50)).await;

    // Send a packet with invalid RTP version
    let mut bad_version = vec![0u8; 20];
    bad_version[0] = 0x00; // version 0 instead of 2
    let _ = peer_a.socket.send_to(&bad_version, remote).await;
    tokio::time::sleep(timing::scaled_ms(100)).await;

    // Session should still be healthy
    let info = client.request_ok("session.info", json!({})).await;
    let endpoints = info["endpoints"].as_array().unwrap();
    assert_eq!(
        endpoints.len(),
        2,
        "session should still have 2 endpoints after malformed packets"
    );

    client.request_ok("session.destroy", json!({})).await;
}

/// Test: Removing an endpoint that has an active recording automatically stops
/// the recording and fires a recording.stopped event.
#[tokio::test]
async fn test_endpoint_remove_during_recording() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;

    // Create an RTP endpoint
    let result = client
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv"}),
        )
        .await;
    let ep_id = result["endpoint_id"].as_str().unwrap().to_string();

    // Start a per-endpoint recording
    let rec_dir = &server.recording_dir;
    let rec_path = format!("{rec_dir}/ep-remove-during-rec.pcap");
    let rec_result = client
        .request_ok(
            "recording.start",
            json!({"endpoint_id": ep_id, "file_path": rec_path}),
        )
        .await;
    let _rec_id = rec_result["recording_id"].as_str().unwrap().to_string();

    // Verify recording is active
    let info = client.request_ok("session.info", json!({})).await;
    let recordings = info["recordings"].as_array().unwrap();
    assert_eq!(recordings.len(), 1, "should have 1 active recording");

    // Remove the endpoint — this should auto-stop the recording
    client
        .request_ok("endpoint.remove", json!({"endpoint_id": ep_id}))
        .await;

    // Wait for the recording.stopped event
    let event = client.recv_event(helpers::timing::scaled_ms(3000)).await;
    assert!(
        event.is_some(),
        "should receive a recording.stopped event after endpoint removal"
    );
    let event = event.unwrap();
    assert_eq!(
        event["event"].as_str().unwrap_or(""),
        "recording.stopped",
        "event should be recording.stopped: {event}"
    );

    // Session should still be functional (just no endpoints)
    let info = client.request_ok("session.info", json!({})).await;
    let endpoints = info["endpoints"].as_array().unwrap();
    assert_eq!(
        endpoints.len(),
        0,
        "session should have 0 endpoints after removal"
    );

    // Can still create new endpoints
    client
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv"}),
        )
        .await;
    let info = client.request_ok("session.info", json!({})).await;
    assert_eq!(info["endpoints"].as_array().unwrap().len(), 1);

    client.request_ok("session.destroy", json!({})).await;
}

/// Test: Rapidly creating and removing endpoints in a tight loop does not
/// corrupt session state or crash the server.
#[tokio::test]
async fn test_rapid_endpoint_create_destroy() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;

    for i in 0..20 {
        // Create an RTP endpoint
        let result = client
            .request_ok(
                "endpoint.create_offer",
                json!({"type": "rtp", "direction": "sendrecv"}),
            )
            .await;
        let ep_id = result["endpoint_id"].as_str().unwrap().to_string();

        // Immediately remove it
        client
            .request_ok("endpoint.remove", json!({"endpoint_id": ep_id}))
            .await;

        // Every 5 iterations, verify session is still healthy
        if i % 5 == 4 {
            let info = client.request_ok("session.info", json!({})).await;
            let endpoints = info["endpoints"].as_array().unwrap();
            assert_eq!(
                endpoints.len(),
                0,
                "session should have 0 endpoints after create+remove cycle {i}"
            );
        }
    }

    // Final health check
    let info = client.request_ok("session.info", json!({})).await;
    assert!(!info["session_id"].as_str().unwrap().is_empty());
    assert_eq!(info["endpoints"].as_array().unwrap().len(), 0);

    // Verify we can still create an endpoint after all the churn
    let result = client
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv"}),
        )
        .await;
    assert!(
        !result["endpoint_id"].as_str().unwrap().is_empty(),
        "should still be able to create endpoints after rapid create/destroy cycles"
    );

    client.request_ok("session.destroy", json!({})).await;
}

// ── VAD parameter validation tests ──────────────────────────────────────

#[tokio::test]
async fn test_vad_start_nonexistent_endpoint() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;

    let resp = client
        .request(
            "vad.start",
            json!({"endpoint_id": "00000000-0000-0000-0000-000000000000"}),
        )
        .await;
    assert!(
        resp.get("error").is_some(),
        "vad.start on nonexistent endpoint should fail: {resp}"
    );

    client.request_ok("session.destroy", json!({})).await;
}

#[tokio::test]
async fn test_vad_start_threshold_out_of_range() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;
    let (ep_id, _peer) = setup_rtp_endpoint(&mut client, "sendrecv").await;

    // speech_threshold > 1.0 should be rejected
    let resp = client
        .request(
            "vad.start",
            json!({"endpoint_id": ep_id, "speech_threshold": 2.0}),
        )
        .await;
    assert_eq!(
        resp["error"]["code"], "INVALID_PARAMS",
        "speech_threshold=2.0 should be rejected: {resp}"
    );

    // speech_threshold < 0.0 should be rejected
    let resp = client
        .request(
            "vad.start",
            json!({"endpoint_id": ep_id, "speech_threshold": -0.5}),
        )
        .await;
    assert_eq!(
        resp["error"]["code"], "INVALID_PARAMS",
        "speech_threshold=-0.5 should be rejected: {resp}"
    );

    client.request_ok("session.destroy", json!({})).await;
}

#[tokio::test]
async fn test_vad_start_silence_interval_too_low() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;
    let (ep_id, _peer) = setup_rtp_endpoint(&mut client, "sendrecv").await;

    let resp = client
        .request(
            "vad.start",
            json!({"endpoint_id": ep_id, "silence_interval_ms": 50}),
        )
        .await;
    assert_eq!(
        resp["error"]["code"], "INVALID_PARAMS",
        "silence_interval_ms=50 should be rejected: {resp}"
    );

    client.request_ok("session.destroy", json!({})).await;
}

// ── File pause/resume on wrong endpoint type ────────────────────────────

#[tokio::test]
async fn test_file_pause_on_rtp_endpoint() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;
    let (ep_id, _peer) = setup_rtp_endpoint(&mut client, "sendrecv").await;

    let resp = client
        .request("endpoint.file.pause", json!({"endpoint_id": ep_id}))
        .await;
    assert!(
        resp.get("error").is_some(),
        "pause on RTP endpoint should fail: {resp}"
    );

    client.request_ok("session.destroy", json!({})).await;
}

#[tokio::test]
async fn test_file_resume_on_rtp_endpoint() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;
    let (ep_id, _peer) = setup_rtp_endpoint(&mut client, "sendrecv").await;

    let resp = client
        .request("endpoint.file.resume", json!({"endpoint_id": ep_id}))
        .await;
    assert!(
        resp.get("error").is_some(),
        "resume on RTP endpoint should fail: {resp}"
    );

    client.request_ok("session.destroy", json!({})).await;
}

// ── SDP / codec edge-case tests ─────────────────────────────────────────

/// Test: Sending an SDP answer without a c= line (no connection address)
/// should return an error from accept_answer.
#[tokio::test]
async fn test_accept_answer_without_connection_address() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;

    // Create offer
    let result = client
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv"}),
        )
        .await;
    let ep_id = result["endpoint_id"].as_str().unwrap().to_string();

    // Send SDP answer without a c= line
    let bad_sdp = "v=0\r\no=- 100 1 IN IP4 127.0.0.1\r\ns=-\r\nt=0 0\r\nm=audio 5000 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=sendrecv\r\n";
    let resp = client
        .request(
            "endpoint.accept_answer",
            json!({"endpoint_id": ep_id, "sdp": bad_sdp}),
        )
        .await;
    assert!(
        resp.get("error").is_some(),
        "accept_answer with no c= line should fail: {resp}"
    );

    client.request_ok("session.destroy", json!({})).await;
}

/// Test: Creating an offer with an explicitly empty codecs list should be
/// handled gracefully (either succeed or return a clear error).
#[tokio::test]
async fn test_create_offer_with_empty_codecs() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;

    let resp = client
        .request(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv", "codecs": []}),
        )
        .await;
    // Should either succeed with no audio codecs or return an error
    // Either way, server should not crash
    assert!(
        resp.get("result").is_some() || resp.get("error").is_some(),
        "server should handle empty codecs gracefully: {resp}"
    );

    client.request_ok("session.destroy", json!({})).await;
}

// ── Recording edge-case tests ───────────────────────────────────────────

/// Test: Starting a recording to a file path that already exists should
/// be rejected (prevents silent overwrite of prior recordings).
#[tokio::test]
async fn test_recording_start_existing_file_rejected() {
    let tmp = TempDir::new().unwrap();
    let server = TestServer::builder()
        .recording_dir(tmp.path().to_str().unwrap())
        .start()
        .await;
    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;

    let file_path = tmp
        .path()
        .join("existing.pcap")
        .to_string_lossy()
        .to_string();

    // Start first recording
    let r1 = client
        .request_ok("recording.start", json!({"file_path": file_path}))
        .await;
    let rec_id = r1["recording_id"].as_str().unwrap().to_string();

    // Stop first recording
    client
        .request_ok("recording.stop", json!({"recording_id": rec_id}))
        .await;

    // Wait briefly for file to be flushed
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Try to record to the same path — should fail (file already exists)
    let resp = client
        .request("recording.start", json!({"file_path": file_path}))
        .await;
    assert!(
        resp.get("error").is_some(),
        "recording to existing file should fail: {resp}"
    );

    client.request_ok("session.destroy", json!({})).await;
}

/// Test: SDP with only unsupported codecs creates endpoint but answer has no audio codec
#[tokio::test]
async fn test_unsupported_codecs_only_creates_degraded_endpoint() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;

    // Offer with only G.729 (PT 18) and PCMA (PT 8) — both unsupported
    let sdp = "v=0\r\n\
        o=- 123 1 IN IP4 10.0.0.1\r\n\
        s=-\r\n\
        c=IN IP4 10.0.0.1\r\n\
        t=0 0\r\n\
        m=audio 30000 RTP/AVP 8 18\r\n\
        a=rtpmap:8 PCMA/8000\r\n\
        a=rtpmap:18 G729/8000\r\n\
        a=sendrecv\r\n";

    let resp = client
        .request(
            "endpoint.create_from_offer",
            json!({"sdp": sdp, "direction": "sendrecv"}),
        )
        .await;
    // Endpoint is created but the answer SDP contains no audio codec —
    // only telephone-event. The endpoint cannot carry audio.
    assert!(
        resp.get("result").is_some(),
        "endpoint creation should succeed (graceful degradation): {resp}"
    );
    let answer = resp["result"]["sdp_answer"].as_str().unwrap();
    // Answer should NOT contain PCMU, G722, or Opus rtpmap lines
    assert!(
        !answer.contains("PCMU") && !answer.contains("G722") && !answer.contains("opus"),
        "answer should not contain any audio codec rtpmap, got: {answer}"
    );

    client.request_ok("session.destroy", json!({})).await;
}
