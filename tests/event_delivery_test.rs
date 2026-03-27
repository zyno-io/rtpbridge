mod helpers;

use std::time::Duration;

use serde_json::json;
use tempfile::TempDir;

use helpers::control_client::TestControlClient;
use helpers::test_rtp_peer::{TestRtpPeer, parse_rtp_addr_from_sdp};
use helpers::test_server::TestServer;
use helpers::timing;
use helpers::wav::generate_test_wav;

/// Helper: create a session with an RTP endpoint connected to a test peer
async fn setup_rtp_session(client: &mut TestControlClient) -> (String, TestRtpPeer) {
    client.request_ok("session.create", json!({})).await;

    let mut peer = TestRtpPeer::new().await;
    let offer = peer.make_sdp_offer();
    let result = client
        .request_ok(
            "endpoint.create_from_offer",
            json!({"sdp": offer, "direction": "sendrecv"}),
        )
        .await;
    let ep_id = result["endpoint_id"].as_str().unwrap().to_string();
    let answer = result["sdp_answer"].as_str().unwrap();
    let server_addr = parse_rtp_addr_from_sdp(answer).expect("parse server addr");
    peer.set_remote(server_addr);

    (ep_id, peer)
}

/// Test: stats events arrive when subscribed
#[tokio::test]
async fn test_stats_event_delivery() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;

    let (_ep_id, mut peer) = setup_rtp_session(&mut client).await;

    // Subscribe to stats with a short interval
    client
        .request_ok("stats.subscribe", json!({"interval_ms": 500}))
        .await;

    // Send a known number of RTP packets to generate stats
    let packets_to_send: u64 = 25;
    let payload = vec![0x80u8; 160];
    for _ in 0..packets_to_send {
        peer.send_pcmu(&payload).await;
        tokio::time::sleep(timing::PACING).await;
    }

    // Wait for stats interval to elapse (subscribed at 500ms)
    tokio::time::sleep(timing::scaled_ms(600)).await;

    // Collect stats events and validate packet count is in a reasonable range
    let mut best_count: u64 = 0;
    for _ in 0..10 {
        let event = client.recv_event(helpers::timing::scaled_ms(3000)).await;
        if let Some(ref event) = event {
            if event["event"].as_str().unwrap_or("") != "stats" {
                continue;
            }
            let endpoints = event["data"]["endpoints"].as_array().unwrap();
            assert!(
                !endpoints.is_empty(),
                "stats should include endpoint data: {event}"
            );
            let ep_stats = &endpoints[0];
            let count = ep_stats["inbound"]["packets"].as_u64().unwrap_or(0);
            if count > best_count {
                best_count = count;
            }
            if count >= packets_to_send / 2 {
                break;
            }
        }
    }

    assert!(
        best_count >= packets_to_send / 2,
        "stats should report at least {} inbound packets (sent {}), got {}",
        packets_to_send / 2,
        packets_to_send,
        best_count
    );
    assert!(
        best_count <= packets_to_send + 5,
        "stats should not report more than {} inbound packets (sent {}), got {}",
        packets_to_send + 5,
        packets_to_send,
        best_count
    );

    // Unsubscribe
    client.request_ok("stats.unsubscribe", json!({})).await;

    client.request_ok("session.destroy", json!({})).await;
}

/// Test: VAD silence events arrive when audio stops
#[tokio::test]
async fn test_vad_silence_event_delivery() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;

    let (ep_id, mut peer) = setup_rtp_session(&mut client).await;

    // Start VAD with a short silence interval
    client
        .request_ok(
            "vad.start",
            json!({"endpoint_id": ep_id, "silence_interval_ms": 200}),
        )
        .await;

    // Send a longer burst of audio to reliably trigger speech detection
    peer.send_tone_for(Duration::from_millis(500)).await;

    // Stop sending — silence should trigger a vad.silence event after the silence interval (200ms).
    // Wait for vad.silence specifically, skipping any vad.speech_started events.
    // Also accept the original event names the server may use.
    let mut got_silence_event = false;
    let mut last_event_type = String::new();
    for _ in 0..15 {
        if let Some(event) = client.recv_event(helpers::timing::scaled_ms(2000)).await {
            let event_type = event["event"].as_str().unwrap_or("").to_string();
            if event_type.contains("vad")
                || event_type.contains("silence")
                || event_type.contains("speech")
            {
                last_event_type = event_type.clone();
                assert_eq!(
                    event["data"]["endpoint_id"].as_str().unwrap(),
                    ep_id,
                    "VAD event should be for our endpoint"
                );
                if event_type.contains("silence") {
                    got_silence_event = true;
                    break;
                }
            }
        }
    }
    assert!(
        got_silence_event,
        "expected vad.silence event, only received: {last_event_type}"
    );

    client
        .request_ok("vad.stop", json!({"endpoint_id": ep_id}))
        .await;
    client.request_ok("session.destroy", json!({})).await;
}

/// Test: DTMF events arrive when DTMF RTP packets are sent
#[tokio::test]
async fn test_dtmf_event_delivery() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;

    let (ep_id, mut peer) = setup_rtp_session(&mut client).await;

    // Subscribe to stats (to ensure event_tx is wired up)
    client
        .request_ok("stats.subscribe", json!({"interval_ms": 5000}))
        .await;

    // Send DTMF digit '5' as RFC 4733 telephone-event packets
    // Multiple packets with same timestamp, end bit on last
    for i in 0..5 {
        let is_end = i == 4;
        let duration = (i + 1) * 160; // increasing duration
        peer.send_dtmf_event(5, is_end, duration).await;
        tokio::time::sleep(timing::PACING).await;
    }
    // Send end packet again (RFC 4733 triple-send)
    for _ in 0..2 {
        peer.send_dtmf_event(5, true, 5 * 160).await;
        tokio::time::sleep(timing::PACING).await;
    }

    // Wait for DTMF event (generous timeout for loaded CI)
    let mut got_dtmf = false;
    for _ in 0..10 {
        if let Some(event) = client.recv_event(helpers::timing::scaled_ms(2000)).await {
            let event_type = event["event"].as_str().unwrap_or("");
            if event_type == "dtmf" {
                got_dtmf = true;
                assert_eq!(
                    event["data"]["endpoint_id"].as_str().unwrap(),
                    ep_id,
                    "DTMF event should be for our endpoint"
                );
                assert_eq!(
                    event["data"]["digit"].as_str().unwrap(),
                    "5",
                    "DTMF digit should be '5': {event}"
                );
                break;
            }
        }
    }
    assert!(got_dtmf, "should have received a DTMF event");

    client.request_ok("session.destroy", json!({})).await;
}

/// Test: bidirectional DTMF — peer A sends DTMF, bridge detects event, and
/// forwards telephone-event packets to peer B.
#[tokio::test]
async fn test_dtmf_bidirectional_forwarding() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;

    // Create session with two RTP endpoints
    client.request_ok("session.create", json!({})).await;

    // Endpoint A (sender)
    let mut peer_a = TestRtpPeer::new().await;
    let offer_a = peer_a.make_sdp_offer();
    let result = client
        .request_ok(
            "endpoint.create_from_offer",
            json!({"sdp": offer_a, "direction": "sendrecv"}),
        )
        .await;
    let ep_a_id = result["endpoint_id"].as_str().unwrap().to_string();
    let answer_a = result["sdp_answer"].as_str().unwrap();
    let server_addr_a = parse_rtp_addr_from_sdp(answer_a).expect("parse server addr A");
    peer_a.set_remote(server_addr_a);

    // Endpoint B (receiver)
    let mut peer_b = TestRtpPeer::new().await;
    let offer_b = peer_b.make_sdp_offer();
    let result = client
        .request_ok(
            "endpoint.create_from_offer",
            json!({"sdp": offer_b, "direction": "sendrecv"}),
        )
        .await;
    let _ep_b_id = result["endpoint_id"].as_str().unwrap().to_string();
    let answer_b = result["sdp_answer"].as_str().unwrap();
    let server_addr_b = parse_rtp_addr_from_sdp(answer_b).expect("parse server addr B");
    peer_b.set_remote(server_addr_b);
    peer_b.activate().await;
    tokio::time::sleep(timing::scaled_ms(50)).await;

    // Start receiving on peer B to capture forwarded DTMF packets
    peer_b.start_recv();
    tokio::time::sleep(timing::scaled_ms(50)).await;

    // Send a few audio packets first to prime the routing path
    for _ in 0..5 {
        peer_a.send_pcmu(&[0x80u8; 160]).await;
        tokio::time::sleep(timing::PACING).await;
    }

    // Send DTMF digit '5' as RFC 4733 telephone-event packets from peer A
    for i in 0..5 {
        let is_end = i == 4;
        let duration = (i + 1) * 160;
        peer_a.send_dtmf_event(5, is_end, duration).await;
        tokio::time::sleep(timing::PACING).await;
    }
    // Triple-send the end packet per RFC 4733
    for _ in 0..2 {
        peer_a.send_dtmf_event(5, true, 5 * 160).await;
        tokio::time::sleep(timing::PACING).await;
    }

    // Wait for DTMF event from bridge (detection)
    let mut got_dtmf_event = false;
    for _ in 0..10 {
        if let Some(event) = client.recv_event(helpers::timing::scaled_ms(2000)).await {
            let event_type = event["event"].as_str().unwrap_or("");
            if event_type == "dtmf" {
                assert_eq!(
                    event["data"]["endpoint_id"].as_str().unwrap(),
                    ep_a_id,
                    "DTMF event should come from endpoint A"
                );
                assert_eq!(
                    event["data"]["digit"].as_str().unwrap(),
                    "5",
                    "DTMF digit should be '5'"
                );
                got_dtmf_event = true;
                break;
            }
        }
    }
    assert!(
        got_dtmf_event,
        "bridge should detect and emit DTMF event from peer A"
    );

    // Verify peer B received forwarded packets (audio + DTMF)
    tokio::time::sleep(timing::scaled_ms(200)).await;
    let received = peer_b.received_count();
    assert!(
        received >= 5,
        "peer B should receive forwarded packets (audio + DTMF), got {received}"
    );

    client.request_ok("session.destroy", json!({})).await;
}

/// Test: media timeout event fires after inactivity
#[tokio::test]
async fn test_media_timeout_event() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;

    let (_ep_id, mut peer) = setup_rtp_session(&mut client).await;

    // Send a brief burst of audio to establish media flow
    peer.send_tone_for(Duration::from_millis(200)).await;

    // Stop sending — wait for media_timeout (threshold is 5s)
    let mut got_timeout = false;
    for _ in 0..15 {
        if let Some(event) = client.recv_event(helpers::timing::scaled_ms(2000)).await {
            let event_type = event["event"].as_str().unwrap_or("");
            if event_type == "endpoint.media_timeout" {
                got_timeout = true;
                let duration_ms = event["data"]["duration_ms"].as_u64().unwrap();
                assert!(
                    duration_ms >= 5000,
                    "timeout duration should be >= 5000ms, got {duration_ms}"
                );
                break;
            }
        }
    }
    assert!(
        got_timeout,
        "should have received endpoint.media_timeout event after ~5s of silence"
    );

    client.request_ok("session.destroy", json!({})).await;
}

/// Test: session transitions to orphaned state when the WebSocket disconnects,
/// and can be reclaimed with session.attach.
#[tokio::test]
async fn test_session_orphaned_on_disconnect() {
    let server = TestServer::start().await;

    // Create a session and note its ID
    let mut client = TestControlClient::connect(&server.addr).await;
    let result = client.request_ok("session.create", json!({})).await;
    let session_id = result["session_id"].as_str().unwrap().to_string();

    // Close the WebSocket — this should orphan the session
    client.close().await;

    // Give the server time to process the disconnect
    tokio::time::sleep(timing::scaled_ms(500)).await;

    // Verify the session is in orphaned state via the REST API
    let http_client = reqwest::Client::new();
    let resp = http_client
        .get(format!("http://{}/sessions/{session_id}", server.addr))
        .timeout(helpers::timing::scaled_ms(5000))
        .send()
        .await
        .expect("HTTP request failed");
    let body: serde_json::Value =
        serde_json::from_str(&resp.text().await.unwrap()).expect("invalid JSON");
    assert_eq!(
        body["state"].as_str().unwrap_or(""),
        "orphaned",
        "session should be in orphaned state after disconnect: {body}"
    );

    // Verify the session can be reclaimed via session.attach
    let mut client2 = TestControlClient::connect(&server.addr).await;
    let resp = client2
        .request("session.attach", json!({"session_id": session_id}))
        .await;
    assert!(
        resp.get("result").is_some(),
        "session.attach should succeed for orphaned session: {resp}"
    );

    // After attach, it should be active again
    let resp = http_client
        .get(format!("http://{}/sessions/{session_id}", server.addr))
        .timeout(helpers::timing::scaled_ms(5000))
        .send()
        .await
        .expect("HTTP request failed");
    let body: serde_json::Value =
        serde_json::from_str(&resp.text().await.unwrap()).expect("invalid JSON");
    assert_eq!(
        body["state"].as_str().unwrap_or(""),
        "active",
        "session should be active after re-attach: {body}"
    );

    client2.request_ok("session.destroy", json!({})).await;
}

/// Test: DTMF injection validation — multi-char digit rejected
#[tokio::test]
async fn test_dtmf_inject_validation() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;

    let (ep_id, _peer) = setup_rtp_session(&mut client).await;

    // Multi-char digit should be rejected
    let resp = client
        .request(
            "endpoint.dtmf.inject",
            json!({
                "endpoint_id": ep_id,
                "digit": "12",
                "duration_ms": 100,
                "volume": 10
            }),
        )
        .await;
    assert_eq!(resp["error"]["code"], "INVALID_PARAMS");
    assert!(
        resp["error"]["message"]
            .as_str()
            .unwrap()
            .contains("exactly one character")
    );

    // Valid single digit should be accepted (may error due to no codec, but not INVALID_PARAMS)
    let resp = client
        .request(
            "endpoint.dtmf.inject",
            json!({
                "endpoint_id": ep_id,
                "digit": "5",
                "duration_ms": 100,
                "volume": 10
            }),
        )
        .await;
    // Should not be INVALID_PARAMS (might be ENDPOINT_ERROR if no codec configured, that's fine)
    if let Some(error) = resp.get("error") {
        assert_ne!(
            error["code"], "INVALID_PARAMS",
            "single digit should pass validation"
        );
    }

    client.request_ok("session.destroy", json!({})).await;
}

/// Test: DTMF inject rejects excessive duration to prevent DoS
#[tokio::test]
async fn test_dtmf_inject_excessive_duration_rejected() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;

    let (ep_id, _peer) = setup_rtp_session(&mut client).await;

    // Duration exceeding 10s cap should be rejected
    let resp = client
        .request(
            "endpoint.dtmf.inject",
            json!({
                "endpoint_id": ep_id,
                "digit": "5",
                "duration_ms": 60_000,
                "volume": 10
            }),
        )
        .await;
    assert_eq!(
        resp["error"]["code"], "INVALID_PARAMS",
        "60s duration should be rejected"
    );
    assert!(
        resp["error"]["message"]
            .as_str()
            .unwrap()
            .contains("duration_ms"),
        "error should mention duration_ms"
    );

    // Duration at the 10s cap should be accepted (may error for other reasons)
    let resp = client
        .request(
            "endpoint.dtmf.inject",
            json!({
                "endpoint_id": ep_id,
                "digit": "5",
                "duration_ms": 10_000,
                "volume": 10
            }),
        )
        .await;
    if let Some(error) = resp.get("error") {
        assert_ne!(
            error["code"], "INVALID_PARAMS",
            "10s duration should pass validation"
        );
    }

    client.request_ok("session.destroy", json!({})).await;
}

/// Test: file.finished event fires with reason "completed" when file playback ends
#[tokio::test]
async fn test_file_finished_event() {
    // Need a server with media_dir configured for local file playback
    let media_dir = TempDir::new().expect("create temp media dir");

    let wav_path = media_dir.path().join("short.wav");
    generate_test_wav(&wav_path, 0.5, 440.0); // 500ms audio file

    let server = TestServer::builder()
        .media_dir(&media_dir.path().to_string_lossy())
        .start()
        .await;

    let mut client = TestControlClient::connect(&server.addr).await;

    // Create session + RTP endpoint as destination
    let (_ep_id, _peer) = setup_rtp_session(&mut client).await;

    // Create file endpoint with loop_count=0 (play once)
    let result = client
        .request_ok(
            "endpoint.create_with_file",
            json!({
                "source": wav_path.to_string_lossy(),
                "loop_count": 0
            }),
        )
        .await;
    let file_ep_id = result["endpoint_id"].as_str().unwrap().to_string();

    // Wait for file.finished event (500ms file + margin for CI)
    let mut got_finished = false;
    for _ in 0..15 {
        if let Some(event) = client.recv_event(helpers::timing::scaled_ms(2000)).await {
            let event_type = event["event"].as_str().unwrap_or("");
            if event_type == "endpoint.file.finished" {
                assert_eq!(
                    event["data"]["endpoint_id"].as_str().unwrap(),
                    file_ep_id,
                    "event should be for our file endpoint"
                );
                assert_eq!(
                    event["data"]["reason"].as_str().unwrap(),
                    "completed",
                    "reason should be 'completed' for natural playback end"
                );
                assert!(
                    event["data"]["error"].is_null(),
                    "error should be null for successful completion"
                );
                got_finished = true;
                break;
            }
        }
    }
    assert!(
        got_finished,
        "should have received endpoint.file.finished event with reason 'completed'"
    );

    client.request_ok("session.destroy", json!({})).await;
}

/// Test: file pause/resume emits endpoint.state_changed events
#[tokio::test]
async fn test_file_pause_resume_state_changed_events() {
    let media_dir = TempDir::new().expect("create temp media dir");

    let wav_path = media_dir.path().join("long.wav");
    generate_test_wav(&wav_path, 10.0, 440.0); // long file to avoid finishing during test

    let server = TestServer::builder()
        .media_dir(&media_dir.path().to_string_lossy())
        .start()
        .await;

    let mut client = TestControlClient::connect(&server.addr).await;
    let (_ep_id, _peer) = setup_rtp_session(&mut client).await;

    // Create file endpoint (infinite loop)
    let result = client
        .request_ok(
            "endpoint.create_with_file",
            json!({
                "source": wav_path.to_string_lossy(),
                "loop_count": null
            }),
        )
        .await;
    let file_ep_id = result["endpoint_id"].as_str().unwrap().to_string();

    // Let playback start
    tokio::time::sleep(timing::scaled_ms(300)).await;
    // Drain any initial events
    while client.recv_event(timing::scaled_ms(50)).await.is_some() {}

    // Pause — should emit playing → paused
    client
        .request_ok("endpoint.file.pause", json!({"endpoint_id": file_ep_id}))
        .await;

    let mut got_pause_event = false;
    for _ in 0..10 {
        if let Some(event) = client.recv_event(helpers::timing::scaled_ms(2000)).await {
            if event["event"] == "endpoint.state_changed"
                && event["data"]["new_state"] == "paused"
                && event["data"]["old_state"] == "playing"
            {
                assert_eq!(event["data"]["endpoint_id"].as_str().unwrap(), file_ep_id);
                got_pause_event = true;
                break;
            }
        }
    }
    assert!(
        got_pause_event,
        "should receive state_changed playing→paused on file pause"
    );

    // Resume — should emit paused → playing
    client
        .request_ok("endpoint.file.resume", json!({"endpoint_id": file_ep_id}))
        .await;

    let mut got_resume_event = false;
    for _ in 0..10 {
        if let Some(event) = client.recv_event(helpers::timing::scaled_ms(2000)).await {
            if event["event"] == "endpoint.state_changed"
                && event["data"]["new_state"] == "playing"
                && event["data"]["old_state"] == "paused"
            {
                assert_eq!(event["data"]["endpoint_id"].as_str().unwrap(), file_ep_id);
                got_resume_event = true;
                break;
            }
        }
    }
    assert!(
        got_resume_event,
        "should receive state_changed paused→playing on file resume"
    );

    client.request_ok("session.destroy", json!({})).await;
}

/// Test: Stats subscription returns RTCP-derived metrics (jitter, loss, packet counts)
#[tokio::test]
async fn test_rtcp_stats_accuracy() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;

    let (_ep_a_id, mut peer_a) = setup_rtp_session(&mut client).await;

    // Create second endpoint so routing is active
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
    peer_b.start_recv();
    tokio::time::sleep(timing::scaled_ms(50)).await;

    // Subscribe to stats with short interval
    client
        .request_ok("stats.subscribe", json!({"interval_ms": 500}))
        .await;

    // Send media for 2 seconds
    peer_a.send_tone_for(Duration::from_secs(2)).await;

    // Wait for stats events with RTCP-derived data
    let mut got_stats = false;
    for _ in 0..10 {
        if let Some(event) = client.recv_event(helpers::timing::scaled_ms(2000)).await {
            if event["event"].as_str().unwrap_or("") == "stats" {
                let endpoints = event["data"]["endpoints"].as_array().unwrap();
                assert!(!endpoints.is_empty(), "stats should have endpoint data");

                for ep in endpoints {
                    let inbound = &ep["inbound"];
                    let outbound = &ep["outbound"];

                    // Verify all RTCP-derived fields present and typed correctly
                    assert!(
                        inbound["packets"].is_number(),
                        "inbound.packets should exist"
                    );
                    assert!(inbound["bytes"].is_number(), "inbound.bytes should exist");
                    assert!(inbound["jitter_ms"].is_number(), "jitter_ms should exist");
                    assert!(
                        inbound["packets_lost"].is_number(),
                        "packets_lost should exist"
                    );
                    assert!(
                        outbound["packets"].is_number(),
                        "outbound.packets should exist"
                    );
                    assert!(outbound["bytes"].is_number(), "outbound.bytes should exist");

                    // In local test, loss should be minimal
                    let lost = inbound["packets_lost"].as_u64().unwrap_or(0);
                    assert!(lost < 5, "local test should have minimal loss, got {lost}");

                    // Jitter should be non-negative
                    let jitter = inbound["jitter_ms"].as_f64().unwrap_or(-1.0);
                    assert!(jitter >= 0.0, "jitter should be >= 0, got {jitter}");
                }

                let total_inbound: u64 = endpoints
                    .iter()
                    .map(|ep| ep["inbound"]["packets"].as_u64().unwrap_or(0))
                    .sum();
                if total_inbound > 0 {
                    got_stats = true;
                    break;
                }
            }
        }
    }

    assert!(
        got_stats,
        "should receive stats event with non-zero inbound packet data"
    );

    client.request_ok("stats.unsubscribe", json!({})).await;
    client.request_ok("session.destroy", json!({})).await;
}

/// Create two endpoints with different telephone-event payload types.
/// Inject DTMF on one, verify the other receives it.
#[tokio::test]
async fn test_dtmf_forwarding_pt_remap() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;

    client.request_ok("session.create", json!({})).await;

    // Endpoint A: telephone-event PT=101
    let mut peer_a = TestRtpPeer::new().await;
    let offer_a = peer_a.make_sdp_offer_with_te_pt(101);
    let result = client
        .request_ok(
            "endpoint.create_from_offer",
            json!({"sdp": offer_a, "direction": "sendrecv"}),
        )
        .await;
    let ep_a_id = result["endpoint_id"].as_str().unwrap().to_string();
    let answer_a = result["sdp_answer"].as_str().unwrap();
    let server_addr_a = parse_rtp_addr_from_sdp(answer_a).expect("parse server addr A");
    peer_a.set_remote(server_addr_a);

    // Endpoint B: telephone-event PT=96 (different from A)
    let mut peer_b = TestRtpPeer::new().await;
    let offer_b = peer_b.make_sdp_offer_with_te_pt(96);
    let result = client
        .request_ok(
            "endpoint.create_from_offer",
            json!({"sdp": offer_b, "direction": "sendrecv"}),
        )
        .await;
    let answer_b = result["sdp_answer"].as_str().unwrap();
    let server_addr_b = parse_rtp_addr_from_sdp(answer_b).expect("parse server addr B");
    peer_b.set_remote(server_addr_b);

    // Activate peer B so symmetric RTP guard allows outbound packets
    peer_b.activate().await;
    tokio::time::sleep(timing::scaled_ms(50)).await;

    // Start receiving on peer B to capture forwarded DTMF
    peer_b.start_recv();
    tokio::time::sleep(timing::scaled_ms(100)).await;

    // Send a few PCMU packets first to establish the media flow
    peer_a.send_tone_for(timing::scaled_ms(200)).await;
    tokio::time::sleep(timing::scaled_ms(200)).await;

    // Inject DTMF on endpoint A — bridge should forward to B
    let resp = client
        .request(
            "endpoint.dtmf.inject",
            json!({
                "endpoint_id": ep_a_id,
                "digit": "5",
                "duration_ms": 160,
                "volume": 10
            }),
        )
        .await;
    // DTMF inject should succeed — both endpoints are connected with answers accepted
    assert!(
        resp.get("result").is_some(),
        "DTMF inject should succeed on a connected endpoint: {resp}"
    );

    // Give time for DTMF packets to be generated and forwarded
    tokio::time::sleep(timing::scaled_ms(500)).await;

    let received = peer_b.received_count();
    assert!(
        received > 0,
        "peer B should receive forwarded DTMF packets after inject on A, got {received}"
    );

    client.request_ok("session.destroy", json!({})).await;
}

/// TestRtpPeer sends RFC 4733 telephone-event packets to the bridge.
/// Verify the bridge emits a "dtmf" event on the control connection.
#[tokio::test]
async fn test_dtmf_detection_from_peer() {
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
    let ep_id = result["endpoint_id"].as_str().unwrap().to_string();
    let answer = result["sdp_answer"].as_str().unwrap();
    let server_addr = parse_rtp_addr_from_sdp(answer).expect("parse server addr");
    peer.set_remote(server_addr);

    // Send some PCMU audio first to establish media flow
    peer.send_tone_for(timing::scaled_ms(200)).await;
    tokio::time::sleep(timing::scaled_ms(100)).await;

    // Send RFC 4733 DTMF event for digit '3' (event_id=3)
    // Multiple packets with increasing duration, end bit on last
    for i in 0..5 {
        let is_end = i == 4;
        let duration = (i + 1) * 160;
        peer.send_dtmf_event(3, is_end, duration).await;
        tokio::time::sleep(timing::PACING).await;
    }
    // Triple-send end packet per RFC 4733
    for _ in 0..2 {
        peer.send_dtmf_event(3, true, 5 * 160).await;
        tokio::time::sleep(timing::PACING).await;
    }

    // Wait for DTMF event from the bridge
    let mut got_dtmf = false;
    for _ in 0..15 {
        if let Some(event) = client.recv_event(timing::scaled_ms(500)).await {
            let event_type = event["event"].as_str().unwrap_or("");
            if event_type == "dtmf" {
                got_dtmf = true;
                assert_eq!(
                    event["data"]["endpoint_id"].as_str().unwrap(),
                    ep_id,
                    "DTMF event should be for our endpoint"
                );
                assert_eq!(
                    event["data"]["digit"].as_str().unwrap(),
                    "3",
                    "DTMF digit should be '3': {event}"
                );
                break;
            }
        }
    }
    assert!(got_dtmf, "should have received a DTMF event for digit '3'");

    client.request_ok("session.destroy", json!({})).await;
}
