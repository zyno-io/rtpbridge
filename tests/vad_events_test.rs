mod helpers;

use std::time::Duration;

use serde_json::json;

use helpers::control_client::TestControlClient;
use helpers::test_rtp_peer::{TestRtpPeer, parse_rtp_addr_from_sdp};
use helpers::test_server::TestServer;
use helpers::timing;

/// Test: VAD emits a vad.speech_started event when a 440Hz tone is sent through the bridge.
///
/// Setup: two RTP endpoints in a session, VAD started on the sender endpoint (A).
/// Peer A sends a 440Hz tone for ~500ms. The bridge should detect speech on endpoint A
/// and emit a vad.speech_started event with the correct endpoint_id.
#[tokio::test]
async fn test_vad_speech_started_event() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;

    client.request_ok("session.create", json!({})).await;

    // Endpoint A (sender): create from peer's SDP offer
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

    // Endpoint B (receiver): create from peer's SDP offer
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

    // Start VAD on endpoint A (the sender) with default params
    client
        .request_ok("vad.start", json!({"endpoint_id": ep_a_id}))
        .await;

    // Send a 440Hz tone from peer A for ~500ms
    peer_a.send_tone_for(Duration::from_millis(500)).await;

    // Wait for a vad.speech_started event
    let mut got_speech_started = false;
    for _ in 0..15 {
        if let Some(event) = client.recv_event(timing::scaled_ms(2000)).await {
            let event_type = event["event"].as_str().unwrap_or("");
            if event_type == "vad.speech_started" {
                assert_eq!(
                    event["data"]["endpoint_id"].as_str().unwrap(),
                    ep_a_id,
                    "vad.speech_started event should contain the correct endpoint_id"
                );
                got_speech_started = true;
                break;
            }
        }
    }
    assert!(
        got_speech_started,
        "should have received a vad.speech_started event after sending 440Hz tone"
    );

    // Clean up
    client
        .request_ok("vad.stop", json!({"endpoint_id": ep_a_id}))
        .await;
    client.request_ok("session.destroy", json!({})).await;
}

/// Test: VAD emits a vad.silence event after speech stops.
///
/// Setup: two RTP endpoints in a session, VAD started on the sender endpoint (A)
/// with a short silence_interval_ms. Peer A sends tone then stops.
/// After the silence interval elapses, a vad.silence event should arrive with
/// the correct endpoint_id and a silence_duration_ms field.
#[tokio::test]
async fn test_vad_silence_event() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;

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

    // Start VAD with a short silence interval (200ms) so the silence event fires quickly
    client
        .request_ok(
            "vad.start",
            json!({"endpoint_id": ep_a_id, "silence_interval_ms": 200}),
        )
        .await;

    // Send a burst of audio to trigger speech detection first
    peer_a.send_tone_for(Duration::from_millis(500)).await;

    // Now stop sending. The silence interval should trigger a vad.silence event.
    // Wait for the vad.silence event, skipping any vad.speech_started events.
    let mut got_silence_event = false;
    for _ in 0..15 {
        if let Some(event) = client.recv_event(timing::scaled_ms(2000)).await {
            let event_type = event["event"].as_str().unwrap_or("");
            if event_type == "vad.silence" {
                assert_eq!(
                    event["data"]["endpoint_id"].as_str().unwrap(),
                    ep_a_id,
                    "vad.silence event should contain the correct endpoint_id"
                );
                assert!(
                    event["data"]["silence_duration_ms"].is_number(),
                    "vad.silence event should contain silence_duration_ms: {event}"
                );
                let silence_ms = event["data"]["silence_duration_ms"].as_u64().unwrap();
                assert!(
                    silence_ms >= 200,
                    "silence_duration_ms should be >= 200 (the configured interval), got {silence_ms}"
                );
                got_silence_event = true;
                break;
            }
        }
    }
    assert!(
        got_silence_event,
        "should have received a vad.silence event after audio stops"
    );

    // Clean up
    client
        .request_ok("vad.stop", json!({"endpoint_id": ep_a_id}))
        .await;
    client.request_ok("session.destroy", json!({})).await;
}
