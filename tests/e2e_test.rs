mod helpers;

use serde_json::json;

use helpers::control_client::TestControlClient;
use helpers::test_server::TestServer;

#[tokio::test]
async fn test_e2e_session_lifecycle_with_endpoints() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;

    // Create session
    let result = client.request_ok("session.create", json!({})).await;
    assert!(!result["session_id"].as_str().unwrap().is_empty());

    // Create WebRTC endpoint offer
    let result = client
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "webrtc", "direction": "sendrecv"}),
        )
        .await;
    let ep1_id = result["endpoint_id"].as_str().unwrap().to_string();
    let sdp = result["sdp_offer"].as_str().unwrap();
    assert!(sdp.contains("v=0"), "should be valid SDP");

    // Create plain RTP endpoint offer
    let result = client
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv"}),
        )
        .await;
    let _ep2_id = result["endpoint_id"].as_str().unwrap().to_string();
    let sdp_rtp = result["sdp_offer"].as_str().unwrap();
    assert!(sdp_rtp.contains("RTP/AVP"), "should be plain RTP SDP");

    // Create SRTP endpoint (OSRTP-compatible)
    let result = client
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv", "srtp": true}),
        )
        .await;
    let sdp_srtp = result["sdp_offer"].as_str().unwrap();
    assert!(sdp_srtp.contains("RTP/SAVP"), "should be SRTP SDP");
    assert!(sdp_srtp.contains("a=crypto:"), "should have crypto line");

    // Create endpoint from a plain RTP SDP offer (auto-detect)
    let sip_offer = "v=0\r\n\
        o=- 100 1 IN IP4 192.168.1.50\r\n\
        s=-\r\n\
        c=IN IP4 192.168.1.50\r\n\
        t=0 0\r\n\
        m=audio 40000 RTP/AVP 0 9 101\r\n\
        a=rtpmap:0 PCMU/8000\r\n\
        a=rtpmap:9 G722/8000\r\n\
        a=rtpmap:101 telephone-event/8000\r\n\
        a=fmtp:101 0-16\r\n\
        a=sendrecv\r\n";

    let result = client
        .request_ok(
            "endpoint.create_from_offer",
            json!({"sdp": sip_offer, "direction": "sendrecv"}),
        )
        .await;
    assert!(!result["endpoint_id"].as_str().unwrap().is_empty());
    let answer = result["sdp_answer"].as_str().unwrap();
    assert!(answer.contains("RTP/AVP"), "answer should match plain RTP");
    assert!(answer.contains("PCMU"), "answer should include PCMU");

    // Create endpoint from an OSRTP offer (RTP/AVP with a=crypto)
    let osrtp_offer = "v=0\r\n\
        o=- 200 1 IN IP4 192.168.1.60\r\n\
        s=-\r\n\
        c=IN IP4 192.168.1.60\r\n\
        t=0 0\r\n\
        m=audio 50000 RTP/AVP 0 101\r\n\
        a=rtpmap:0 PCMU/8000\r\n\
        a=rtpmap:101 telephone-event/8000\r\n\
        a=fmtp:101 0-16\r\n\
        a=crypto:1 AES_CM_128_HMAC_SHA1_80 inline:YTEyMzQ1Njc4OTAxMjM0NTY3ODkwMTIzNDU2Nzg5\r\n\
        a=sendrecv\r\n";

    let result = client
        .request_ok(
            "endpoint.create_from_offer",
            json!({"sdp": osrtp_offer, "direction": "sendrecv"}),
        )
        .await;
    assert!(!result["endpoint_id"].as_str().unwrap().is_empty());
    let answer = result["sdp_answer"].as_str().unwrap();
    // OSRTP: the answer should echo back the crypto
    assert!(
        answer.contains("a=crypto:"),
        "OSRTP answer should echo crypto: {answer}"
    );

    // Remove the first endpoint
    let resp = client
        .request("endpoint.remove", json!({"endpoint_id": ep1_id}))
        .await;
    assert!(!resp["result"].is_null(), "expected result: {resp}");

    // Session info should still work
    let result = client.request_ok("session.info", json!({})).await;
    assert!(!result["session_id"].as_str().unwrap().is_empty());

    // Destroy session
    let resp = client.request("session.destroy", json!({})).await;
    assert!(!resp["result"].is_null(), "expected result: {resp}");
}

#[tokio::test]
async fn test_e2e_recording_and_vad() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;
    let rec_dir = std::path::Path::new(&server.recording_dir);
    let rec_path = rec_dir.join("recording.pcap");
    let leg_path = rec_dir.join("leg.pcap");

    // Create session
    client.request_ok("session.create", json!({})).await;

    // Create an RTP endpoint
    let result = client
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv"}),
        )
        .await;
    let ep_id = result["endpoint_id"].as_str().unwrap().to_string();

    // Start recording (full session)
    let result = client
        .request_ok(
            "recording.start",
            json!({"endpoint_id": null, "file_path": rec_path.to_str().unwrap()}),
        )
        .await;
    let rec_id = result["recording_id"].as_str().unwrap().to_string();

    // Start single-leg recording
    let result = client
        .request_ok(
            "recording.start",
            json!({"endpoint_id": ep_id, "file_path": leg_path.to_str().unwrap()}),
        )
        .await;
    let leg_rec_id = result["recording_id"].as_str().unwrap().to_string();

    // Start VAD on the endpoint
    let resp = client
        .request(
            "vad.start",
            json!({"endpoint_id": ep_id, "silence_interval_ms": 500}),
        )
        .await;
    assert!(
        !resp["result"].is_null(),
        "vad.start should succeed: {resp}"
    );

    // Subscribe to stats
    let resp = client
        .request("stats.subscribe", json!({"interval_ms": 1000}))
        .await;
    assert!(
        !resp["result"].is_null(),
        "stats.subscribe should succeed: {resp}"
    );

    // Stop VAD
    let resp = client
        .request("vad.stop", json!({"endpoint_id": ep_id}))
        .await;
    assert!(!resp["result"].is_null(), "expected result: {resp}");

    // Unsubscribe stats
    let resp = client.request("stats.unsubscribe", json!({})).await;
    assert!(!resp["result"].is_null(), "expected result: {resp}");

    // Stop recordings
    let result = client
        .request_ok("recording.stop", json!({"recording_id": rec_id}))
        .await;
    assert!(!result["file_path"].as_str().unwrap().is_empty());

    let result = client
        .request_ok("recording.stop", json!({"recording_id": leg_rec_id}))
        .await;
    assert!(!result["file_path"].as_str().unwrap().is_empty());

    // Clean up
    client.request("session.destroy", json!({})).await;
    // tmp dir auto-cleaned on drop
}

#[tokio::test]
async fn test_e2e_dtmf_inject() {
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

    // Inject DTMF — should succeed even without a connected remote
    // (packets are built and sent to the remote address, which may not exist in test)
    let resp = client
        .request(
            "endpoint.dtmf.inject",
            json!({"endpoint_id": ep_id, "digit": "5", "duration_ms": 100}),
        )
        .await;
    // DTMF inject succeeds (packets are queued) even without a connected remote
    if let Some(error) = resp.get("error") {
        let code = error["code"].as_str().unwrap_or("");
        assert!(
            code == "ENDPOINT_ERROR",
            "if DTMF inject fails, expected ENDPOINT_ERROR, got: {code}"
        );
    } else {
        assert!(
            resp.get("result").is_some(),
            "expected result or error: {resp}"
        );
    }

    client.request("session.destroy", json!({})).await;
}

#[tokio::test]
async fn test_recording_start_rejects_unknown_endpoint_id() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;
    let rec_dir = std::path::Path::new(&server.recording_dir);
    let ghost_path = rec_dir.join("ghost-recording.pcap");
    let full_path = rec_dir.join("full-session-rec.pcap");

    client.request_ok("session.create", json!({})).await;

    // Use a fake endpoint_id that does not exist in the session
    let fake_ep_id = uuid::Uuid::new_v4().to_string();
    let resp = client
        .request(
            "recording.start",
            json!({
                "endpoint_id": fake_ep_id,
                "file_path": ghost_path.to_str().unwrap()
            }),
        )
        .await;

    assert!(
        resp.get("error").is_some(),
        "recording.start with unknown endpoint_id should return error: {resp}"
    );
    let error_msg = resp["error"]["message"].as_str().unwrap_or("");
    assert!(
        error_msg.contains("not found") || error_msg.contains("Endpoint"),
        "error should mention endpoint not found: {resp}"
    );

    // Full-session recording (endpoint_id=null) should still succeed
    let result = client
        .request_ok(
            "recording.start",
            json!({
                "endpoint_id": null,
                "file_path": full_path.to_str().unwrap()
            }),
        )
        .await;
    assert!(
        !result["recording_id"].as_str().unwrap().is_empty(),
        "full-session recording should succeed"
    );

    client.request("session.destroy", json!({})).await;
    // tmp dir auto-cleaned on drop
}
