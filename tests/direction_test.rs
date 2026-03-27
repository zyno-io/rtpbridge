mod helpers;

use std::time::Duration;

use serde_json::json;

use helpers::control_client::TestControlClient;
use helpers::test_rtp_peer::{TestRtpPeer, parse_rtp_addr_from_sdp};
use helpers::test_server::TestServer;
use helpers::timing;

/// Helper: create an RTP endpoint with a given direction via create_offer,
/// wire up a TestRtpPeer, and accept the answer.
/// Returns (endpoint_id, peer).
async fn create_endpoint_with_peer(
    client: &mut TestControlClient,
    direction: &str,
) -> (String, TestRtpPeer) {
    let result = client
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": direction}),
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

    (ep_id, peer)
}

/// Test: Direction enforcement — sendonly endpoints should not receive media,
/// recvonly endpoints should receive but not send, sendrecv does both.
///
/// Setup:
///   Endpoint A = sendonly  (sends media into the session, does not receive)
///   Endpoint B = recvonly  (receives media from the session, does not send)
///   Endpoint C = sendrecv  (sends and receives)
///
/// Routing expectations:
///   A (sendonly)  → routes to B (recvonly) and C (sendrecv)
///   C (sendrecv)  → routes to B (recvonly) [A is sendonly, so excluded from receiving]
///   B (recvonly)  → routes nowhere (recvonly never sends)
#[tokio::test]
async fn test_direction_enforcement() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;

    client.request_ok("session.create", json!({})).await;

    // Create three endpoints with different directions
    let (_ep_a_id, mut peer_a) = create_endpoint_with_peer(&mut client, "sendonly").await;
    let (_ep_b_id, mut peer_b) = create_endpoint_with_peer(&mut client, "recvonly").await;
    let (_ep_c_id, mut peer_c) = create_endpoint_with_peer(&mut client, "sendrecv").await;

    // Activate symmetric RTP guard on peers that will receive (or be checked)
    peer_a.activate().await;
    peer_b.activate().await;
    peer_c.activate().await;
    tokio::time::sleep(timing::scaled_ms(50)).await;

    // Start receive loops on all peers
    peer_a.start_recv();
    peer_b.start_recv();
    peer_c.start_recv();
    tokio::time::sleep(timing::scaled_ms(50)).await;

    // ── Test 1: Send from peer C (sendrecv) ──
    // C sends → should be routed to B (recvonly gets media)
    // C sends → should NOT be routed to A (sendonly does not receive)
    peer_c.send_tone_for(Duration::from_millis(200)).await;
    tokio::time::sleep(timing::scaled_ms(500)).await;

    let b_from_c = peer_b.received_count();
    let a_from_c = peer_a.received_count();

    assert!(
        b_from_c > 0,
        "peer B (recvonly) should receive media from peer C (sendrecv), got {b_from_c}"
    );
    assert_eq!(
        a_from_c, 0,
        "peer A (sendonly) should NOT receive media from peer C, got {a_from_c}"
    );

    // ── Test 2: Send from peer A (sendonly) ──
    // A sends → should be routed to C (sendrecv receives)
    // A sends → should be routed to B (recvonly receives)
    // Reset B's count conceptually by recording current value
    let b_before = peer_b.received_count();
    let c_before = peer_c.received_count();

    peer_a.send_tone_for(Duration::from_millis(200)).await;
    tokio::time::sleep(timing::scaled_ms(500)).await;

    let c_from_a = peer_c.received_count() - c_before;
    let b_from_a = peer_b.received_count() - b_before;

    assert!(
        c_from_a > 0,
        "peer C (sendrecv) should receive media from peer A (sendonly), got {c_from_a}"
    );
    assert!(
        b_from_a > 0,
        "peer B (recvonly) should receive media from peer A (sendonly), got {b_from_a}"
    );

    // Verify session info reflects all 3 endpoints
    let info = client.request_ok("session.info", json!({})).await;
    let endpoints = info["endpoints"].as_array().unwrap();
    assert_eq!(endpoints.len(), 3, "should have 3 endpoints");

    client.request_ok("session.destroy", json!({})).await;
}

/// Test: recvonly endpoints never produce outgoing media even when
/// audio is sent from the peer side.
#[tokio::test]
async fn test_recvonly_does_not_route() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;

    client.request_ok("session.create", json!({})).await;

    // Create a recvonly endpoint and a sendrecv endpoint
    let (_ep_recv_id, mut peer_recv) = create_endpoint_with_peer(&mut client, "recvonly").await;
    let (_ep_sr_id, mut peer_sr) = create_endpoint_with_peer(&mut client, "sendrecv").await;

    // Activate symmetric RTP guard on peer that will be checked
    peer_sr.activate().await;
    tokio::time::sleep(timing::scaled_ms(50)).await;

    peer_sr.start_recv();
    tokio::time::sleep(timing::scaled_ms(50)).await;

    // Send audio from recvonly peer — bridge should NOT route it
    peer_recv.send_tone_for(Duration::from_millis(200)).await;
    tokio::time::sleep(timing::scaled_ms(500)).await;

    let sr_received = peer_sr.received_count();
    assert_eq!(
        sr_received, 0,
        "sendrecv peer should NOT receive media from recvonly peer, got {sr_received}"
    );

    client.request_ok("session.destroy", json!({})).await;
}

/// Test: sendonly endpoint does not receive media from any source
#[tokio::test]
async fn test_sendonly_never_receives() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;

    client.request_ok("session.create", json!({})).await;

    let (_ep_so_id, mut peer_so) = create_endpoint_with_peer(&mut client, "sendonly").await;
    let (_ep_sr_id, mut peer_sr) = create_endpoint_with_peer(&mut client, "sendrecv").await;

    // Activate symmetric RTP guard on peer that will be checked
    peer_so.activate().await;
    tokio::time::sleep(timing::scaled_ms(50)).await;

    peer_so.start_recv();
    tokio::time::sleep(timing::scaled_ms(50)).await;

    // Send from sendrecv peer
    peer_sr.send_tone_for(Duration::from_millis(200)).await;
    tokio::time::sleep(timing::scaled_ms(500)).await;

    let so_received = peer_so.received_count();
    assert_eq!(
        so_received, 0,
        "sendonly peer should never receive media, got {so_received}"
    );

    client.request_ok("session.destroy", json!({})).await;
}

/// Test: Direction change mid-session by removing and re-creating an endpoint with a new direction.
///
/// NOTE: There is no `endpoint.modify` or `endpoint.modify_direction` handler in the control
/// protocol. The only way to change an endpoint's direction mid-session is to remove it and
/// create a new one with the desired direction. This test verifies that approach works correctly:
///
/// Setup:
///   Endpoint A = sendrecv, Endpoint B = sendrecv, Endpoint C = sendrecv
///   Phase 1: A sends, B and C both receive (all sendrecv)
///   Phase 2: Remove B, re-create B as recvonly
///   Phase 3: A sends, B receives. B sends — no one should get B's packets (recvonly is ignored).
#[tokio::test]
async fn test_direction_change_mid_session() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;

    client.request_ok("session.create", json!({})).await;

    // Phase 1: Create 3 sendrecv endpoints
    let (_ep_a_id, mut peer_a) = create_endpoint_with_peer(&mut client, "sendrecv").await;
    let (ep_b_id, mut peer_b) = create_endpoint_with_peer(&mut client, "sendrecv").await;
    let (_ep_c_id, mut peer_c) = create_endpoint_with_peer(&mut client, "sendrecv").await;

    // Activate symmetric RTP guard on peers that will receive (or be checked)
    peer_a.activate().await;
    peer_b.activate().await;
    peer_c.activate().await;
    tokio::time::sleep(timing::scaled_ms(50)).await;

    peer_b.start_recv();
    peer_c.start_recv();
    tokio::time::sleep(timing::scaled_ms(50)).await;

    // A sends — B and C should both receive
    peer_a.send_tone_for(Duration::from_millis(200)).await;
    tokio::time::sleep(timing::scaled_ms(500)).await;

    assert!(
        peer_b.received_count() > 0,
        "B (sendrecv) should receive from A in phase 1"
    );
    assert!(
        peer_c.received_count() > 0,
        "C (sendrecv) should receive from A in phase 1"
    );

    // Phase 2: Remove endpoint B and re-create it as recvonly
    client
        .request_ok("endpoint.remove", json!({"endpoint_id": ep_b_id}))
        .await;
    tokio::time::sleep(timing::scaled_ms(200)).await;

    // Create new B as recvonly
    let (_ep_b2_id, mut peer_b2) = create_endpoint_with_peer(&mut client, "recvonly").await;

    // Activate symmetric RTP guard on new peer
    peer_b2.activate().await;
    tokio::time::sleep(timing::scaled_ms(50)).await;

    peer_b2.start_recv();

    // Also start recv on peer_a to check if B2's outbound is blocked
    peer_a.start_recv();
    tokio::time::sleep(timing::scaled_ms(50)).await;

    // Phase 3: A sends — B2 (recvonly) should receive
    peer_a.send_tone_for(Duration::from_millis(200)).await;
    tokio::time::sleep(timing::scaled_ms(500)).await;

    assert!(
        peer_b2.received_count() > 0,
        "B2 (recvonly) should receive media from A"
    );

    // B2's peer sends — since B2 is recvonly, its outbound packets should NOT be routed
    let a_before = peer_a.received_count();
    let c_before = peer_c.received_count();
    peer_b2.send_tone_for(Duration::from_millis(200)).await;
    tokio::time::sleep(timing::scaled_ms(500)).await;

    let a_from_b2 = peer_a.received_count() - a_before;
    let c_from_b2 = peer_c.received_count() - c_before;
    assert_eq!(
        a_from_b2, 0,
        "A should NOT receive media from B2 (recvonly), got {a_from_b2}"
    );
    assert_eq!(
        c_from_b2, 0,
        "C should NOT receive media from B2 (recvonly), got {c_from_b2}"
    );

    // Verify session has 3 endpoints (A, B2, C)
    let info = client.request_ok("session.info", json!({})).await;
    assert_eq!(
        info["endpoints"].as_array().unwrap().len(),
        3,
        "should have 3 endpoints after direction change"
    );

    client.request_ok("session.destroy", json!({})).await;
}
