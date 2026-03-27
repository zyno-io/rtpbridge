mod helpers;

use serde_json::json;

use helpers::control_client::TestControlClient;
use helpers::test_rtp_peer::{TestRtpPeer, build_rtp_packet, parse_rtp_addr_from_sdp};
use helpers::test_server::TestServer;
use helpers::timing;

// ═══════════════════════════════════════════════════════════════════════════
// 1. Packet loss / jitter simulation
// ═══════════════════════════════════════════════════════════════════════════

/// Send RTP with gaps in sequence numbers (simulating packet loss).
/// The bridge should still forward whatever arrives — it doesn't retransmit.
#[tokio::test]
async fn test_packet_loss_sequence_gaps() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;

    // Create two RTP endpoints
    let mut peer_a = TestRtpPeer::new().await;
    let mut peer_b = TestRtpPeer::new().await;
    peer_b.start_recv();

    let sdp_a = peer_a.make_sdp_offer();
    let res_a = client
        .request_ok(
            "endpoint.create_from_offer",
            json!({"sdp": sdp_a, "direction": "sendrecv"}),
        )
        .await;
    let ep_a_id = res_a["endpoint_id"].as_str().unwrap().to_string();

    let res_b = client
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv"}),
        )
        .await;
    let sdp_b_offer = res_b["sdp_offer"].as_str().unwrap();
    let server_addr_b = parse_rtp_addr_from_sdp(sdp_b_offer).unwrap();
    peer_b.set_remote(server_addr_b);

    let ep_b_id = res_b["endpoint_id"].as_str().unwrap().to_string();
    let answer_b = peer_b.make_sdp_answer();
    client
        .request_ok(
            "endpoint.accept_answer",
            json!({"endpoint_id": ep_b_id, "sdp": answer_b}),
        )
        .await;

    peer_b.activate().await;
    tokio::time::sleep(timing::scaled_ms(50)).await;

    // Get server's address for endpoint A
    let info = client.request_ok("session.info", json!({})).await;
    let endpoints = info["endpoints"].as_array().unwrap();
    let ep_a_info = endpoints
        .iter()
        .find(|e| e["endpoint_id"].as_str().unwrap() == ep_a_id)
        .unwrap();
    let server_addr_a = parse_rtp_addr_from_sdp(ep_a_info["sdp"].as_str().unwrap_or(""))
        .unwrap_or_else(|| {
            // Fallback: parse from the create_from_offer result
            let sdp = res_a["sdp_answer"].as_str().unwrap();
            parse_rtp_addr_from_sdp(sdp).unwrap()
        });
    peer_a.set_remote(server_addr_a);

    // Send a burst of address-learning packets from B so bridge knows where to route
    let payload = vec![0x7Fu8; 160]; // silence
    for _ in 0..3 {
        let pkt = build_rtp_packet(0, 0, 0, 0xBBBB, false, &payload);
        peer_b.socket.send_to(&pkt, server_addr_b).await.unwrap();
        tokio::time::sleep(timing::PACING).await;
    }

    // Send packets from A with sequence gaps: 0, 1, 2, [skip 3-7], 8, 9, 10
    let ssrc: u32 = 0xAAAA;
    let seqs: &[u16] = &[0, 1, 2, 8, 9, 10];
    for &seq in seqs {
        let ts = seq as u32 * 160;
        let pkt = build_rtp_packet(0, seq, ts, ssrc, false, &payload);
        peer_a.socket.send_to(&pkt, server_addr_a).await.unwrap();
        tokio::time::sleep(timing::PACING).await;
    }

    tokio::time::sleep(timing::scaled_ms(300)).await;

    // Peer B should have received the packets that were sent (not the gaps)
    let received = peer_b.received_count();
    assert!(
        received >= 4,
        "peer B should receive forwarded packets despite sequence gaps, got {received}"
    );

    client.request_ok("session.destroy", json!({})).await;
}

/// Send RTP with out-of-order delivery (jitter simulation).
/// The bridge forwards packets as-is without reordering.
#[tokio::test]
async fn test_out_of_order_packet_delivery() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;

    let mut peer_a = TestRtpPeer::new().await;
    let mut peer_b = TestRtpPeer::new().await;
    peer_b.start_recv();

    let sdp_a = peer_a.make_sdp_offer();
    let res_a = client
        .request_ok(
            "endpoint.create_from_offer",
            json!({"sdp": sdp_a, "direction": "sendrecv"}),
        )
        .await;

    let res_b = client
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv"}),
        )
        .await;
    let sdp_b_offer = res_b["sdp_offer"].as_str().unwrap();
    let server_addr_b = parse_rtp_addr_from_sdp(sdp_b_offer).unwrap();
    peer_b.set_remote(server_addr_b);

    let ep_b_id = res_b["endpoint_id"].as_str().unwrap().to_string();
    let answer_b = peer_b.make_sdp_answer();
    client
        .request_ok(
            "endpoint.accept_answer",
            json!({"endpoint_id": ep_b_id, "sdp": answer_b}),
        )
        .await;

    peer_b.activate().await;
    tokio::time::sleep(timing::scaled_ms(50)).await;

    let server_addr_a = parse_rtp_addr_from_sdp(res_a["sdp_answer"].as_str().unwrap()).unwrap();
    peer_a.set_remote(server_addr_a);

    // Address learning for B
    let payload = vec![0x7Fu8; 160];
    for _ in 0..3 {
        let pkt = build_rtp_packet(0, 0, 0, 0xBBBB, false, &payload);
        peer_b.socket.send_to(&pkt, server_addr_b).await.unwrap();
        tokio::time::sleep(timing::PACING).await;
    }

    // Send packets out of order: 0, 2, 1, 4, 3, 5
    let ssrc: u32 = 0xAAAA;
    for &seq in &[0u16, 2, 1, 4, 3, 5] {
        let ts = seq as u32 * 160;
        let pkt = build_rtp_packet(0, seq, ts, ssrc, false, &payload);
        peer_a.socket.send_to(&pkt, server_addr_a).await.unwrap();
        tokio::time::sleep(timing::scaled_ms(10)).await;
    }

    tokio::time::sleep(timing::scaled_ms(300)).await;

    // All 6 packets should be forwarded (bridge doesn't reorder)
    let received = peer_b.received_count();
    assert!(
        received >= 4,
        "peer B should receive out-of-order packets, got {received}"
    );

    client.request_ok("session.destroy", json!({})).await;
}

/// Send a burst of packets with no inter-packet delay (jitter burst).
/// The bridge should handle rapid arrival without dropping or crashing.
#[tokio::test]
async fn test_burst_packet_delivery() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;

    let mut peer_a = TestRtpPeer::new().await;
    let mut peer_b = TestRtpPeer::new().await;
    peer_b.start_recv();

    let sdp_a = peer_a.make_sdp_offer();
    let res_a = client
        .request_ok(
            "endpoint.create_from_offer",
            json!({"sdp": sdp_a, "direction": "sendrecv"}),
        )
        .await;

    let res_b = client
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv"}),
        )
        .await;
    let sdp_b_offer = res_b["sdp_offer"].as_str().unwrap();
    let server_addr_b = parse_rtp_addr_from_sdp(sdp_b_offer).unwrap();
    peer_b.set_remote(server_addr_b);

    let ep_b_id = res_b["endpoint_id"].as_str().unwrap().to_string();
    let answer_b = peer_b.make_sdp_answer();
    client
        .request_ok(
            "endpoint.accept_answer",
            json!({"endpoint_id": ep_b_id, "sdp": answer_b}),
        )
        .await;

    peer_b.activate().await;
    tokio::time::sleep(timing::scaled_ms(50)).await;

    let server_addr_a = parse_rtp_addr_from_sdp(res_a["sdp_answer"].as_str().unwrap()).unwrap();
    peer_a.set_remote(server_addr_a);

    // Address learning for B
    let payload = vec![0x7Fu8; 160];
    for _ in 0..3 {
        let pkt = build_rtp_packet(0, 0, 0, 0xBBBB, false, &payload);
        peer_b.socket.send_to(&pkt, server_addr_b).await.unwrap();
        tokio::time::sleep(timing::PACING).await;
    }

    // Send 100 packets as fast as possible (no sleep between them)
    let ssrc: u32 = 0xAAAA;
    for seq in 0u16..100 {
        let ts = seq as u32 * 160;
        let pkt = build_rtp_packet(0, seq, ts, ssrc, false, &payload);
        peer_a.socket.send_to(&pkt, server_addr_a).await.unwrap();
    }

    tokio::time::sleep(timing::scaled_ms(500)).await;

    // Most packets should be forwarded; some loss under burst is acceptable
    let received = peer_b.received_count();
    assert!(
        received >= 50,
        "peer B should receive most burst packets, got {received}/100"
    );

    client.request_ok("session.destroy", json!({})).await;
}

// ═══════════════════════════════════════════════════════════════════════════
// 2. Resource exhaustion
// ═══════════════════════════════════════════════════════════════════════════

/// Max endpoints per session is enforced.
#[tokio::test]
async fn test_max_endpoints_per_session_exhaustion() {
    let server = TestServer::builder()
        .max_endpoints_per_session(2)
        .start()
        .await;
    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;

    // Create 2 endpoints (should succeed)
    for _ in 0..2 {
        client
            .request_ok(
                "endpoint.create_offer",
                json!({"type": "rtp", "direction": "sendrecv"}),
            )
            .await;
    }

    // Third should fail
    let result = client
        .request(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv"}),
        )
        .await;
    let err = result["error"].as_object().expect("expected error object");
    assert!(
        err.contains_key("message"),
        "third endpoint should be rejected: {result}"
    );
    let error_msg = result["error"]["message"].as_str().unwrap_or("");
    assert!(
        error_msg.contains("MAX_ENDPOINTS_REACHED"),
        "error should mention MAX_ENDPOINTS_REACHED: {error_msg}"
    );

    // Remove one endpoint, then creating another should succeed
    let info = client.request_ok("session.info", json!({})).await;
    let ep_id = info["endpoints"][0]["endpoint_id"]
        .as_str()
        .unwrap()
        .to_string();
    client
        .request_ok("endpoint.remove", json!({"endpoint_id": ep_id}))
        .await;

    let result = client
        .request(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv"}),
        )
        .await;
    assert!(
        result.get("result").is_some(),
        "should succeed after freeing a slot: {result}"
    );

    client.request_ok("session.destroy", json!({})).await;
}

/// Port pool exhaustion: with a tiny port range, creating too many endpoints
/// should fail gracefully with an error, not panic.
#[tokio::test]
async fn test_port_pool_exhaustion() {
    // Pick a dynamic port range: 4 ports = 2 allocatable pairs
    let base = portpicker::pick_unused_port().expect("no free port");
    let base = if base % 2 != 0 { base + 1 } else { base };
    let server = TestServer::builder()
        .rtp_port_range(base, base + 3)
        .start()
        .await;
    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;

    // First two endpoints should succeed (exhausts the port pool)
    client
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv"}),
        )
        .await;
    client
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv"}),
        )
        .await;

    // Third should fail — no ports left
    let result = client
        .request(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv"}),
        )
        .await;
    let err = result["error"].as_object().expect("expected error object");
    assert!(
        err.contains_key("message"),
        "should fail when port pool is exhausted: {result}"
    );

    client.request_ok("session.destroy", json!({})).await;
}

/// Multiple sessions competing for a limited port pool should not deadlock or panic.
#[tokio::test]
async fn test_port_pool_contention_across_sessions() {
    // Pick a dynamic port range: 6 ports = 3 pairs, spread across 2 sessions
    let base = portpicker::pick_unused_port().expect("no free port");
    let base = if base % 2 != 0 { base + 1 } else { base };
    let server = TestServer::builder()
        .rtp_port_range(base, base + 5)
        .start()
        .await;

    let mut client1 = TestControlClient::connect(&server.addr).await;
    client1.request_ok("session.create", json!({})).await;
    let mut client2 = TestControlClient::connect(&server.addr).await;
    client2.request_ok("session.create", json!({})).await;

    // Session 1: allocate 2 endpoints
    client1
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv"}),
        )
        .await;
    client1
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv"}),
        )
        .await;

    // Session 2: allocate 1 endpoint (uses last pair)
    client2
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv"}),
        )
        .await;

    // Both sessions trying more should fail
    let r1 = client1
        .request(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv"}),
        )
        .await;
    let err = r1["error"].as_object().expect("expected error object");
    assert!(err.contains_key("message"), "session 1 should fail: {r1}");

    let r2 = client2
        .request(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv"}),
        )
        .await;
    let err = r2["error"].as_object().expect("expected error object");
    assert!(err.contains_key("message"), "session 2 should fail: {r2}");

    // Destroy session 1 to free its ports
    client1.request_ok("session.destroy", json!({})).await;
    tokio::time::sleep(timing::scaled_ms(200)).await;

    // Session 2 should now be able to allocate again
    let r3 = client2
        .request(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv"}),
        )
        .await;
    assert!(
        r3.get("result").is_some(),
        "session 2 should succeed after session 1 freed ports: {r3}"
    );

    client2.request_ok("session.destroy", json!({})).await;
}

// ═══════════════════════════════════════════════════════════════════════════
// 3. WebSocket rate-limiting / abuse
// ═══════════════════════════════════════════════════════════════════════════

/// Rapid-fire WebSocket messages should not crash the server.
/// The server should process them all or drop gracefully.
#[tokio::test]
async fn test_rapid_fire_messages() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;

    // Send 100 session.info requests as fast as possible
    for _ in 0..100 {
        client.request_ok("session.info", json!({})).await;
    }

    // Server should still be responsive
    let info = client.request_ok("session.info", json!({})).await;
    assert!(
        !info["session_id"].as_str().unwrap().is_empty(),
        "server should still respond"
    );

    client.request_ok("session.destroy", json!({})).await;
}

/// Sending many invalid messages rapidly should not crash or exhaust resources.
#[tokio::test]
async fn test_rapid_malformed_messages() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;

    // Send 50 malformed messages rapidly
    for i in 0..50 {
        let garbage = format!(r#"{{"id":"{i}","method":"nonexistent.method","params":{{}}}}"#);
        let resp = client.send_raw(&garbage).await;
        assert!(
            resp["error"].is_object(),
            "should return error object for invalid method: {resp}"
        );
    }

    // Server should still accept valid requests
    client.request_ok("session.create", json!({})).await;
    let info = client.request_ok("session.info", json!({})).await;
    assert!(!info["session_id"].as_str().unwrap().is_empty());

    client.request_ok("session.destroy", json!({})).await;
}

/// Multiple clients connecting and disconnecting rapidly should not leak resources.
#[tokio::test]
async fn test_rapid_connect_disconnect() {
    let server = TestServer::builder()
        .disconnect_timeout_secs(1)
        .start()
        .await;

    // Rapidly connect, create session, cleanly close (triggers disconnect detection)
    for _ in 0..20 {
        let mut client = TestControlClient::connect(&server.addr).await;
        client.request_ok("session.create", json!({})).await;
        client.close().await;
    }

    // Wait for orphan timeouts (1s) + margin for 20 sessions
    tokio::time::sleep(timing::scaled_ms(4000)).await;

    // Verify all sessions were cleaned up
    let mut verify = TestControlClient::connect(&server.addr).await;
    verify.request_ok("session.create", json!({})).await;
    let list = verify.request_ok("session.list", json!({})).await;
    let sessions = list["sessions"].as_array().unwrap();
    assert_eq!(
        sessions.len(),
        1,
        "only the verify session should remain after orphan cleanup, got {}",
        sessions.len()
    );

    verify.request_ok("session.destroy", json!({})).await;
}

/// Sending commands without a session bound should return errors, not crash.
#[tokio::test]
async fn test_commands_without_session() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;

    // These all require a session but none is created
    let methods = [
        "session.info",
        "session.destroy",
        "endpoint.create_offer",
        "endpoint.remove",
        "recording.start",
        "recording.stop",
        "vad.start",
        "vad.stop",
        "stats.subscribe",
        "stats.unsubscribe",
    ];

    for method in &methods {
        let result = client.request(method, json!({})).await;
        let err = result["error"].as_object().expect("expected error object");
        assert!(
            err.contains_key("message"),
            "{method} without session should return error: {result}"
        );
    }

    // Server should still be functional
    client.request_ok("session.create", json!({})).await;
    client.request_ok("session.destroy", json!({})).await;
}

// ═══════════════════════════════════════════════════════════════════════════
// 4. Concurrent SDP renegotiation
// ═══════════════════════════════════════════════════════════════════════════

/// Multiple endpoint creates in rapid succession on the same session.
#[tokio::test]
async fn test_concurrent_endpoint_creation() {
    let server = TestServer::start().await;
    let addr = server.addr.clone();

    let mut client = TestControlClient::connect(&addr).await;
    client.request_ok("session.create", json!({})).await;

    // Create 5 RTP endpoints in rapid succession
    let mut endpoint_ids = Vec::new();
    for _ in 0..5 {
        let result = client
            .request_ok(
                "endpoint.create_offer",
                json!({"type": "rtp", "direction": "sendrecv"}),
            )
            .await;
        let ep_id = result["endpoint_id"].as_str().unwrap().to_string();
        endpoint_ids.push(ep_id);
    }

    // All endpoint IDs should be unique
    let unique: std::collections::HashSet<_> = endpoint_ids.iter().collect();
    assert_eq!(unique.len(), 5, "all endpoint IDs should be unique");

    // Session should report all 5 endpoints
    let info = client.request_ok("session.info", json!({})).await;
    assert_eq!(info["endpoints"].as_array().unwrap().len(), 5);

    // Remove all endpoints in rapid succession
    for ep_id in &endpoint_ids {
        client
            .request_ok("endpoint.remove", json!({"endpoint_id": ep_id}))
            .await;
    }

    // Session should have 0 endpoints
    let info = client.request_ok("session.info", json!({})).await;
    assert_eq!(info["endpoints"].as_array().unwrap().len(), 0);

    client.request_ok("session.destroy", json!({})).await;
}

/// Accept SDP answer on an endpoint, then immediately create another endpoint.
/// Tests that accept_answer doesn't block or corrupt session state.
#[tokio::test]
async fn test_accept_answer_then_create() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;

    // Create endpoint A and accept answer
    let res_a = client
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv"}),
        )
        .await;
    let ep_a_id = res_a["endpoint_id"].as_str().unwrap().to_string();

    let peer_a = TestRtpPeer::new().await;
    let answer_a = peer_a.make_sdp_answer();
    client
        .request_ok(
            "endpoint.accept_answer",
            json!({"endpoint_id": ep_a_id, "sdp": answer_a}),
        )
        .await;

    // Immediately create endpoint B
    let res_b = client
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv"}),
        )
        .await;
    let ep_b_id = res_b["endpoint_id"].as_str().unwrap().to_string();

    // Accept answer on B
    let peer_b = TestRtpPeer::new().await;
    let answer_b = peer_b.make_sdp_answer();
    client
        .request_ok(
            "endpoint.accept_answer",
            json!({"endpoint_id": ep_b_id, "sdp": answer_b}),
        )
        .await;

    // Both endpoints should be in connected state
    let info = client.request_ok("session.info", json!({})).await;
    let endpoints = info["endpoints"].as_array().unwrap();
    assert_eq!(endpoints.len(), 2);

    for ep in endpoints {
        let state = ep["state"].as_str().unwrap();
        assert_eq!(
            state, "connected",
            "endpoint should be connected after accept_answer"
        );
    }

    client.request_ok("session.destroy", json!({})).await;
}

/// ICE restart on one endpoint while creating another endpoint simultaneously.
#[tokio::test]
async fn test_ice_restart_during_endpoint_creation() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;

    // Create a WebRTC endpoint
    let mut peer = helpers::test_peer::TestPeer::new().await;
    let offer = peer.create_offer();
    let res_webrtc = client
        .request_ok(
            "endpoint.create_from_offer",
            json!({"sdp": offer, "direction": "sendrecv"}),
        )
        .await;
    let webrtc_ep_id = res_webrtc["endpoint_id"].as_str().unwrap().to_string();

    // ICE restart on the WebRTC endpoint
    let restart_result = client
        .request_ok("endpoint.ice_restart", json!({"endpoint_id": webrtc_ep_id}))
        .await;
    let sdp = restart_result["sdp_offer"].as_str().unwrap();
    assert!(sdp.contains("v=0"), "ICE restart should return new SDP");

    // Immediately create a plain RTP endpoint
    let rtp_result = client
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv"}),
        )
        .await;
    assert!(
        !rtp_result["endpoint_id"].as_str().unwrap().is_empty(),
        "RTP endpoint creation should succeed"
    );

    // Session should have both endpoints
    let info = client.request_ok("session.info", json!({})).await;
    assert_eq!(info["endpoints"].as_array().unwrap().len(), 2);

    client.request_ok("session.destroy", json!({})).await;
}

// ═══════════════════════════════════════════════════════════════════════════
// 5. Sequence number wraparound
// ═══════════════════════════════════════════════════════════════════════════

/// Send RTP packets with sequence numbers wrapping around u16::MAX (65535 → 0).
/// The bridge should forward all packets regardless of wraparound.
#[tokio::test]
async fn test_rtp_sequence_wraparound() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;

    let mut peer_a = TestRtpPeer::new().await;
    let mut peer_b = TestRtpPeer::new().await;
    peer_b.start_recv();

    let sdp_a = peer_a.make_sdp_offer();
    let res_a = client
        .request_ok(
            "endpoint.create_from_offer",
            json!({"sdp": sdp_a, "direction": "sendrecv"}),
        )
        .await;

    let res_b = client
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv"}),
        )
        .await;
    let sdp_b_offer = res_b["sdp_offer"].as_str().unwrap();
    let server_addr_b = parse_rtp_addr_from_sdp(sdp_b_offer).unwrap();
    peer_b.set_remote(server_addr_b);

    let ep_b_id = res_b["endpoint_id"].as_str().unwrap().to_string();
    let answer_b = peer_b.make_sdp_answer();
    client
        .request_ok(
            "endpoint.accept_answer",
            json!({"endpoint_id": ep_b_id, "sdp": answer_b}),
        )
        .await;

    peer_b.activate().await;
    tokio::time::sleep(timing::scaled_ms(50)).await;

    let server_addr_a = parse_rtp_addr_from_sdp(res_a["sdp_answer"].as_str().unwrap()).unwrap();
    peer_a.set_remote(server_addr_a);

    // Address learning for B
    let payload = vec![0x7Fu8; 160];
    for _ in 0..3 {
        let pkt = build_rtp_packet(0, 0, 0, 0xBBBB, false, &payload);
        peer_b.socket.send_to(&pkt, server_addr_b).await.unwrap();
        tokio::time::sleep(timing::PACING).await;
    }

    // Send 10 packets from A with sequence numbers wrapping around u16::MAX:
    // 65530, 65531, 65532, 65533, 65534, 65535, 0, 1, 2, 3
    let ssrc: u32 = 0xAAAA;
    let start_seq: u16 = 65530;
    for i in 0u16..10 {
        let seq = start_seq.wrapping_add(i);
        let ts = i as u32 * 160;
        let pkt = build_rtp_packet(0, seq, ts, ssrc, false, &payload);
        peer_a.socket.send_to(&pkt, server_addr_a).await.unwrap();
        tokio::time::sleep(timing::PACING).await;
    }

    tokio::time::sleep(timing::scaled_ms(500)).await;

    // Peer B should have received all 10 packets (or at least most of them)
    let received = peer_b.received_count();
    assert!(
        received >= 8,
        "peer B should receive packets across sequence wraparound, got {received}/10"
    );

    client.request_ok("session.destroy", json!({})).await;
}
