mod helpers;

use serde_json::json;

use helpers::control_client::TestControlClient;
use helpers::test_rtp_peer::{TestRtpPeer, parse_rtp_addr_from_sdp};
use helpers::test_server::TestServer;
use helpers::timing;

/// Test: Destroying one session while creating endpoints on a separate session doesn't panic
#[tokio::test]
async fn test_destroy_independent_sessions_concurrently() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;

    // Spawn a task that creates endpoints in a loop
    let addr = server.addr.clone();
    let creator = tokio::spawn(async move {
        let mut c = TestControlClient::connect(&addr).await;
        c.request_ok("session.create", json!({})).await;
        for _ in 0..5 {
            let resp = c
                .request(
                    "endpoint.create_offer",
                    json!({"type": "rtp", "direction": "sendrecv"}),
                )
                .await;
            // Either succeeds or returns an error — both are fine
            let _ = resp;
            tokio::time::sleep(helpers::timing::scaled_ms(10)).await;
        }
        c.request_ok("session.destroy", json!({})).await;
    });

    // Meanwhile, destroy the first session
    tokio::time::sleep(helpers::timing::scaled_ms(20)).await;
    client.request_ok("session.destroy", json!({})).await;

    // Verify the creator task completes without panic
    creator.await.expect("creator task should not panic");
}

/// Test: Rapid recording start/stop/start cycles work correctly
#[tokio::test]
async fn test_simultaneous_recording_start_stop() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;

    client
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv"}),
        )
        .await;

    let rec_dir = &server.recording_dir;

    for i in 0..5 {
        let path = format!("{rec_dir}/rapid-{i}.pcap");

        // Start recording
        let result = client
            .request_ok(
                "recording.start",
                json!({"endpoint_id": null, "file_path": path}),
            )
            .await;
        let rec_id = result["recording_id"].as_str().unwrap().to_string();

        // Immediately stop
        client
            .request_ok("recording.stop", json!({"recording_id": rec_id}))
            .await;
    }

    // Verify session is still healthy
    let info = client.request_ok("session.info", json!({})).await;
    assert_eq!(info["endpoints"].as_array().unwrap().len(), 1);

    client.request_ok("session.destroy", json!({})).await;
}

/// Test: Removing endpoint while DTMF injection is in progress doesn't crash
#[tokio::test]
async fn test_endpoint_removal_during_dtmf() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;

    // Create two RTP endpoints
    let ep1_result = client
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv"}),
        )
        .await;
    let ep1_id = ep1_result["endpoint_id"].as_str().unwrap().to_string();
    let ep1_sdp = ep1_result["sdp_offer"].as_str().unwrap();

    // Need a peer to answer so the endpoint has a codec negotiated
    let ep1_addr = parse_rtp_addr_from_sdp(ep1_sdp).unwrap();
    let mut peer1 = TestRtpPeer::new().await;
    peer1.set_remote(ep1_addr);
    let answer1 = peer1.make_sdp_answer();
    client
        .request_ok(
            "endpoint.accept_answer",
            json!({"endpoint_id": ep1_id, "sdp": answer1}),
        )
        .await;

    let ep2_result = client
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv"}),
        )
        .await;
    let _ep2_id = ep2_result["endpoint_id"].as_str().unwrap().to_string();

    // Inject DTMF on endpoint 1 (long duration to ensure it's still in progress)
    let dtmf_resp = client
        .request(
            "endpoint.dtmf.inject",
            json!({"endpoint_id": ep1_id, "digit": "5", "duration_ms": 500, "volume": 10}),
        )
        .await;

    // If DTMF inject succeeded, immediately remove the endpoint
    if dtmf_resp.get("result").is_some() {
        // Remove endpoint while DTMF may still be draining
        let remove_resp = client
            .request("endpoint.remove", json!({"endpoint_id": ep1_id}))
            .await;
        // Should succeed or return a clean error — no crash
        let _ = remove_resp;
    }

    // Session should still be healthy
    let info = client.request_ok("session.info", json!({})).await;
    assert!(info["endpoints"].as_array().is_some());

    client.request_ok("session.destroy", json!({})).await;
}

/// Test: Create, destroy, create on same WS connection — second session gets new ID
#[tokio::test]
async fn test_rapid_create_destroy_create() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;

    // First session
    let result1 = client.request_ok("session.create", json!({})).await;
    let id1 = result1["session_id"].as_str().unwrap().to_string();

    // Destroy it
    client.request_ok("session.destroy", json!({})).await;

    // Create another on the same connection
    let result2 = client.request_ok("session.create", json!({})).await;
    let id2 = result2["session_id"].as_str().unwrap().to_string();

    assert_ne!(id1, id2, "second session should have a different ID");

    // Second session should be fully functional
    client
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv"}),
        )
        .await;

    client.request_ok("session.destroy", json!({})).await;
}

/// Test: Second client can't attach a session that was already reattached
#[tokio::test]
async fn test_multiple_attach_attempts() {
    let server = TestServer::builder()
        .disconnect_timeout_secs(5)
        .start()
        .await;

    // Client 1 creates and orphans a session
    let mut client1 = TestControlClient::connect(&server.addr).await;
    let result = client1.request_ok("session.create", json!({})).await;
    let session_id = result["session_id"].as_str().unwrap().to_string();
    client1.close().await;

    tokio::time::sleep(timing::scaled_ms(500)).await;

    // Client 2 attaches — should succeed
    let mut client2 = TestControlClient::connect(&server.addr).await;
    let attach_result = client2
        .request("session.attach", json!({"session_id": session_id}))
        .await;
    assert!(
        attach_result.get("result").is_some(),
        "first attach should succeed: {attach_result}"
    );

    // Client 3 tries to attach the same session — should fail
    let mut client3 = TestControlClient::connect(&server.addr).await;
    let attach_result2 = client3
        .request("session.attach", json!({"session_id": session_id}))
        .await;
    assert!(
        attach_result2.get("error").is_some(),
        "second attach should fail: {attach_result2}"
    );

    client2.request_ok("session.destroy", json!({})).await;
}

/// Test: Destroying session stops active recordings and PCAP files persist on disk
#[tokio::test]
async fn test_destroy_session_stops_recordings() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;

    client
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv"}),
        )
        .await;

    let rec_dir = &server.recording_dir;
    let mut paths = Vec::new();

    // Start 3 recordings without stopping them
    for i in 0..3 {
        let path = format!("{rec_dir}/persist-{i}.pcap");
        client
            .request_ok(
                "recording.start",
                json!({"endpoint_id": null, "file_path": path}),
            )
            .await;
        paths.push(path);
    }

    // Destroy session without stopping recordings
    client.request_ok("session.destroy", json!({})).await;

    // Give the recording tasks time to flush
    tokio::time::sleep(timing::scaled_ms(500)).await;

    // PCAP files should exist on disk
    for path in &paths {
        assert!(
            std::path::Path::new(path).exists(),
            "PCAP file should be flushed and preserved: {path}"
        );
    }
}

/// Test: Invalid recording path doesn't affect an existing active recording
#[tokio::test]
async fn test_recording_bad_path_doesnt_affect_existing() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;

    client
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv"}),
        )
        .await;

    let rec_dir = &server.recording_dir;

    // Start a valid recording
    let good_path = format!("{rec_dir}/good.pcap");
    let good_result = client
        .request_ok(
            "recording.start",
            json!({"endpoint_id": null, "file_path": good_path}),
        )
        .await;
    let good_id = good_result["recording_id"].as_str().unwrap().to_string();

    // Try to start a recording with a bad path (outside recording_dir)
    let bad_resp = client
        .request(
            "recording.start",
            json!({"endpoint_id": null, "file_path": "/nonexistent/bad.pcap"}),
        )
        .await;
    assert!(bad_resp.get("error").is_some(), "bad path should fail");

    // Original recording should still be active
    let info = client.request_ok("session.info", json!({})).await;
    let recordings = info["recordings"].as_array().unwrap();
    assert_eq!(recordings.len(), 1, "good recording should still be active");

    // Stop the good recording — should succeed
    client
        .request_ok("recording.stop", json!({"recording_id": good_id}))
        .await;

    client.request_ok("session.destroy", json!({})).await;
}
