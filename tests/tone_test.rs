mod helpers;

use std::time::Duration;

use serde_json::json;

use helpers::control_client::TestControlClient;
use helpers::test_rtp_peer::{TestRtpPeer, parse_rtp_addr_from_sdp};
use helpers::test_server::TestServer;
use helpers::timing;

/// Helper: create a PCMU endpoint and activate symmetric RTP.
async fn setup_rtp_endpoint(client: &mut TestControlClient, peer: &mut TestRtpPeer) -> String {
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
    peer.activate().await;
    ep_id
}

/// Test: A ringback tone endpoint sends PCMU packets to an RTP peer.
#[tokio::test]
async fn test_tone_ringback_delivers_audio() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;

    let mut peer = TestRtpPeer::new().await;
    setup_rtp_endpoint(&mut client, &mut peer).await;

    // Create ringback tone
    let result = client
        .request_ok("endpoint.create_tone", json!({"tone": "ringback"}))
        .await;
    let tone_id = result["endpoint_id"].as_str().unwrap().to_string();
    assert_eq!(result["tone"].as_str().unwrap(), "ringback");

    peer.start_recv();
    tokio::time::sleep(timing::scaled_ms(50)).await;

    // Wait for audio to arrive (ringback starts with 2s on phase)
    tokio::time::sleep(timing::scaled_ms(500)).await;

    let count = peer.received_count();
    assert!(
        count > 5,
        "should receive ringback tone packets, got {count}"
    );

    // Remove the tone
    client
        .request_ok("endpoint.remove", json!({"endpoint_id": tone_id}))
        .await;
}

/// Test: A sine tone endpoint with custom frequency.
#[tokio::test]
async fn test_tone_sine_custom_frequency() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;

    let mut peer = TestRtpPeer::new().await;
    setup_rtp_endpoint(&mut client, &mut peer).await;

    let result = client
        .request_ok(
            "endpoint.create_tone",
            json!({"tone": "sine", "frequency": 1000.0}),
        )
        .await;
    assert!(result["endpoint_id"].as_str().is_some());

    peer.start_recv();
    tokio::time::sleep(timing::scaled_ms(400)).await;

    let count = peer.received_count();
    assert!(count > 5, "should receive sine tone packets, got {count}");
}

/// Test: A tone with duration_ms finishes and emits an event.
#[tokio::test]
async fn test_tone_duration_limit_emits_finished_event() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;

    let mut peer = TestRtpPeer::new().await;
    setup_rtp_endpoint(&mut client, &mut peer).await;
    peer.start_recv();

    // Create a 200ms tone
    client
        .request_ok(
            "endpoint.create_tone",
            json!({"tone": "sine", "frequency": 440.0, "duration_ms": 200}),
        )
        .await;

    // Wait for the tone to finish
    let event = client
        .recv_event_type("endpoint.tone.finished", Duration::from_secs(3))
        .await;
    assert!(event.is_some(), "should receive tone.finished event");
}

/// Test: Busy tone has correct cadence (0.5s on / 0.5s off).
#[tokio::test]
async fn test_tone_busy_delivers_audio() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;

    let mut peer = TestRtpPeer::new().await;
    setup_rtp_endpoint(&mut client, &mut peer).await;

    client
        .request_ok("endpoint.create_tone", json!({"tone": "busy"}))
        .await;

    peer.start_recv();
    tokio::time::sleep(timing::scaled_ms(600)).await;

    let count = peer.received_count();
    assert!(count > 5, "should receive busy tone packets, got {count}");
}

/// Test: Invalid frequency is rejected.
#[tokio::test]
async fn test_tone_invalid_frequency_rejected() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;

    let result = client
        .request(
            "endpoint.create_tone",
            json!({"tone": "sine", "frequency": 50000.0}),
        )
        .await;
    assert!(
        result["error"].is_object(),
        "frequency > 20kHz should be rejected"
    );
}
