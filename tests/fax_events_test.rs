mod helpers;

use std::time::Duration;

use serde_json::json;

use helpers::control_client::TestControlClient;
use helpers::test_rtp_peer::{TestRtpPeer, parse_rtp_addr_from_sdp};
use helpers::test_server::TestServer;
use helpers::timing;

/// Set up a session with two plain-RTP endpoints. Returns the `server` and the
/// receiver `peer_b` so the caller keeps them in scope (both abort their tasks
/// on drop) for the life of the test, along with the client, sender peer, and
/// its endpoint id.
async fn setup_pair() -> (
    TestServer,
    TestControlClient,
    TestRtpPeer,
    TestRtpPeer,
    String,
) {
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

    // Endpoint B (receiver) — gives the sender a routing destination
    let mut peer_b = TestRtpPeer::new().await;
    let offer_b = peer_b.make_sdp_offer();
    let result = client
        .request_ok(
            "endpoint.create_from_offer",
            json!({"sdp": offer_b, "direction": "sendrecv"}),
        )
        .await;
    let answer_b = result["sdp_answer"].as_str().unwrap();
    let server_addr_b = parse_rtp_addr_from_sdp(answer_b).expect("parse server addr B");
    peer_b.set_remote(server_addr_b);

    (server, client, peer_a, peer_b, ep_a_id)
}

/// Wait for a specific fax event on the given endpoint.
async fn expect_fax_event(client: &mut TestControlClient, want: &str, ep_id: &str) -> bool {
    for _ in 0..15 {
        if let Some(event) = client.recv_event(timing::scaled_ms(2000)).await
            && event["event"].as_str().unwrap_or("") == want
        {
            assert_eq!(
                event["data"]["endpoint_id"].as_str().unwrap(),
                ep_id,
                "{want} event should carry the correct endpoint_id"
            );
            return true;
        }
    }
    false
}

/// CED: a sustained 2100Hz tone on the monitored endpoint produces a
/// `fax.ced_detected` event.
#[tokio::test]
async fn test_fax_ced_detected_event() {
    let (_server, mut client, mut peer_a, _peer_b, ep_a_id) = setup_pair().await;

    client
        .request_ok("fax_detect.start", json!({"endpoint_id": ep_a_id}))
        .await;

    // CED is a continuous ~2.6s answer tone; 600ms is plenty past the ~160ms
    // confirmation window.
    peer_a
        .send_tone_freq_for(2100.0, Duration::from_millis(600))
        .await;

    assert!(
        expect_fax_event(&mut client, "fax.ced_detected", &ep_a_id).await,
        "should have received a fax.ced_detected event from a 2100Hz tone"
    );

    client
        .request_ok("fax_detect.stop", json!({"endpoint_id": ep_a_id}))
        .await;
    client.request_ok("session.destroy", json!({})).await;
}

/// CNG: a sustained 1100Hz tone produces a `fax.cng_detected` event.
#[tokio::test]
async fn test_fax_cng_detected_event() {
    let (_server, mut client, mut peer_a, _peer_b, ep_a_id) = setup_pair().await;

    client
        .request_ok("fax_detect.start", json!({"endpoint_id": ep_a_id}))
        .await;

    peer_a
        .send_tone_freq_for(1100.0, Duration::from_millis(600))
        .await;

    assert!(
        expect_fax_event(&mut client, "fax.cng_detected", &ep_a_id).await,
        "should have received a fax.cng_detected event from a 1100Hz tone"
    );

    client
        .request_ok("fax_detect.stop", json!({"endpoint_id": ep_a_id}))
        .await;
    client.request_ok("session.destroy", json!({})).await;
}

/// A non-fax tone (440Hz speech-band) must NOT produce any fax event.
#[tokio::test]
async fn test_non_fax_tone_no_event() {
    let (_server, mut client, mut peer_a, _peer_b, ep_a_id) = setup_pair().await;

    client
        .request_ok("fax_detect.start", json!({"endpoint_id": ep_a_id}))
        .await;

    peer_a.send_tone_for(Duration::from_millis(600)).await; // 440Hz

    // Drain events for a short window; assert no fax.* event arrives.
    let mut saw_fax = false;
    for _ in 0..8 {
        if let Some(event) = client.recv_event(timing::scaled_ms(500)).await
            && event["event"].as_str().unwrap_or("").starts_with("fax.")
        {
            saw_fax = true;
            break;
        }
    }
    assert!(!saw_fax, "440Hz tone should not produce a fax event");

    client
        .request_ok("fax_detect.stop", json!({"endpoint_id": ep_a_id}))
        .await;
    client.request_ok("session.destroy", json!({})).await;
}

/// `fax_detect.stop` on an endpoint that was never started should return an error.
#[tokio::test]
async fn test_fax_stop_without_start_errors() {
    let (_server, mut client, _peer_a, _peer_b, ep_a_id) = setup_pair().await;

    let resp = client
        .request("fax_detect.stop", json!({"endpoint_id": ep_a_id}))
        .await;
    assert!(
        resp.get("error").is_some(),
        "fax_detect.stop without an active detector should error: {resp}"
    );

    client.request_ok("session.destroy", json!({})).await;
}

/// `fax_detect_active` should appear in session.info and list the monitored endpoint.
#[tokio::test]
async fn test_fax_detect_active_in_session_info() {
    let (_server, mut client, _peer_a, _peer_b, ep_a_id) = setup_pair().await;

    client
        .request_ok("fax_detect.start", json!({"endpoint_id": ep_a_id}))
        .await;

    let info = client.request_ok("session.info", json!({})).await;
    let active = info["fax_detect_active"]
        .as_array()
        .expect("session.info should contain fax_detect_active array");
    assert!(
        active.iter().any(|v| v.as_str() == Some(ep_a_id.as_str())),
        "fax_detect_active should list the monitored endpoint: {info}"
    );

    client
        .request_ok("fax_detect.stop", json!({"endpoint_id": ep_a_id}))
        .await;
    client.request_ok("session.destroy", json!({})).await;
}
