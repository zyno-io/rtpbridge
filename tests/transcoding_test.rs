mod helpers;

use std::time::Duration;

use serde_json::json;

use helpers::control_client::TestControlClient;
use helpers::test_rtp_peer::{TestRtpPeer, parse_rtp_addr_from_sdp};
use helpers::timing;

use helpers::test_server::TestServer;

/// Test: Two PCMU endpoints can exchange audio (same codec passthrough).
/// Verifies that when both endpoints use the same codec (PCMU), RTP packets
/// are routed from sender to receiver without transcoding.
#[tokio::test]
async fn test_pcmu_passthrough() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;

    client.request_ok("session.create", json!({})).await;

    // Create two test peers
    let mut peer_a = TestRtpPeer::new().await;
    let mut peer_b = TestRtpPeer::new().await;

    // Endpoint A: accept peer A's PCMU offer
    let offer_a = peer_a.make_sdp_offer();
    let result = client
        .request_ok(
            "endpoint.create_from_offer",
            json!({"sdp": offer_a, "direction": "sendrecv"}),
        )
        .await;
    let _ep_a_id = result["endpoint_id"].as_str().unwrap().to_string();
    let answer_a = result["sdp_answer"].as_str().unwrap();
    let server_addr_a = parse_rtp_addr_from_sdp(answer_a).expect("parse server addr A");
    peer_a.set_remote(server_addr_a);

    // Verify the answer contains PCMU
    assert!(
        answer_a.contains("PCMU"),
        "answer should include PCMU codec"
    );

    // Endpoint B: server creates offer, peer B provides answer
    let result = client
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv"}),
        )
        .await;
    let ep_b_id = result["endpoint_id"].as_str().unwrap().to_string();
    let offer_b = result["sdp_offer"].as_str().unwrap();
    let server_addr_b = parse_rtp_addr_from_sdp(offer_b).expect("parse server addr B");

    // Verify offer includes PCMU
    assert!(offer_b.contains("PCMU"), "offer should include PCMU codec");

    let answer_b = peer_b.make_sdp_answer();
    client
        .request_ok(
            "endpoint.accept_answer",
            json!({"endpoint_id": ep_b_id, "sdp": answer_b}),
        )
        .await;
    peer_b.set_remote(server_addr_b);

    // Activate both peers so symmetric RTP guard allows outbound packets
    peer_a.activate().await;
    peer_b.activate().await;
    tokio::time::sleep(timing::scaled_ms(50)).await;

    // Start receiving on peer B
    peer_b.start_recv();
    tokio::time::sleep(timing::scaled_ms(50)).await;

    // Send PCMU audio from peer A
    peer_a.send_tone_for(Duration::from_millis(200)).await;

    // Wait for packets to traverse
    tokio::time::sleep(timing::scaled_ms(500)).await;

    // Peer B should have received the routed packets
    let received = peer_b.received_count();
    assert!(
        received > 0,
        "peer B should receive PCMU passthrough packets from peer A, got {received}"
    );

    // Verify stats show packet flow
    let info = client.request_ok("session.info", json!({})).await;
    let endpoints = info["endpoints"].as_array().unwrap();
    assert_eq!(endpoints.len(), 2, "should have 2 endpoints");

    client.request_ok("session.destroy", json!({})).await;
}

/// Test: Bidirectional PCMU media flow — both peers send and receive
#[tokio::test]
async fn test_pcmu_bidirectional() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;

    client.request_ok("session.create", json!({})).await;

    let mut peer_a = TestRtpPeer::new().await;
    let mut peer_b = TestRtpPeer::new().await;

    // Set up endpoint A
    let offer_a = peer_a.make_sdp_offer();
    let result = client
        .request_ok(
            "endpoint.create_from_offer",
            json!({"sdp": offer_a, "direction": "sendrecv"}),
        )
        .await;
    let answer_a = result["sdp_answer"].as_str().unwrap();
    let server_addr_a = parse_rtp_addr_from_sdp(answer_a).expect("parse server addr A");
    peer_a.set_remote(server_addr_a);

    // Set up endpoint B
    let result = client
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv"}),
        )
        .await;
    let ep_b_id = result["endpoint_id"].as_str().unwrap().to_string();
    let offer_b = result["sdp_offer"].as_str().unwrap();
    let server_addr_b = parse_rtp_addr_from_sdp(offer_b).expect("parse server addr B");
    let answer_b = peer_b.make_sdp_answer();
    client
        .request_ok(
            "endpoint.accept_answer",
            json!({"endpoint_id": ep_b_id, "sdp": answer_b}),
        )
        .await;
    peer_b.set_remote(server_addr_b);

    // Activate both peers so symmetric RTP guard allows outbound packets
    peer_a.activate().await;
    peer_b.activate().await;
    tokio::time::sleep(timing::scaled_ms(50)).await;

    // Start receiving on both peers
    peer_a.start_recv();
    peer_b.start_recv();
    tokio::time::sleep(timing::scaled_ms(50)).await;

    // Send from A to B
    peer_a.send_tone_for(Duration::from_millis(200)).await;
    tokio::time::sleep(timing::scaled_ms(500)).await;

    let b_received = peer_b.received_count();
    assert!(
        b_received > 0,
        "peer B should receive packets from A, got {b_received}"
    );

    // Send from B to A
    let a_before = peer_a.received_count();
    peer_b.send_tone_for(Duration::from_millis(200)).await;
    tokio::time::sleep(timing::scaled_ms(500)).await;

    let a_received = peer_a.received_count() - a_before;
    assert!(
        a_received > 0,
        "peer A should receive packets from B, got {a_received}"
    );

    client.request_ok("session.destroy", json!({})).await;
}

/// Test: Multiple endpoints in a session — 3-party conference with PCMU.
/// All sendrecv endpoints should route media to all others.
#[tokio::test]
async fn test_three_party_pcmu_conference() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;

    client.request_ok("session.create", json!({})).await;

    let mut peer_a = TestRtpPeer::new().await;
    let mut peer_b = TestRtpPeer::new().await;
    let mut peer_c = TestRtpPeer::new().await;

    // Set up endpoint A
    let offer_a = peer_a.make_sdp_offer();
    let result = client
        .request_ok(
            "endpoint.create_from_offer",
            json!({"sdp": offer_a, "direction": "sendrecv"}),
        )
        .await;
    let answer_a = result["sdp_answer"].as_str().unwrap();
    let server_addr_a = parse_rtp_addr_from_sdp(answer_a).expect("parse server addr A");
    peer_a.set_remote(server_addr_a);

    // Set up endpoint B
    let result = client
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv"}),
        )
        .await;
    let ep_b_id = result["endpoint_id"].as_str().unwrap().to_string();
    let offer_b = result["sdp_offer"].as_str().unwrap();
    let server_addr_b = parse_rtp_addr_from_sdp(offer_b).expect("parse server addr B");
    let answer_b = peer_b.make_sdp_answer();
    client
        .request_ok(
            "endpoint.accept_answer",
            json!({"endpoint_id": ep_b_id, "sdp": answer_b}),
        )
        .await;
    peer_b.set_remote(server_addr_b);

    // Set up endpoint C
    let result = client
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv"}),
        )
        .await;
    let ep_c_id = result["endpoint_id"].as_str().unwrap().to_string();
    let offer_c = result["sdp_offer"].as_str().unwrap();
    let server_addr_c = parse_rtp_addr_from_sdp(offer_c).expect("parse server addr C");
    let answer_c = peer_c.make_sdp_answer();
    client
        .request_ok(
            "endpoint.accept_answer",
            json!({"endpoint_id": ep_c_id, "sdp": answer_c}),
        )
        .await;
    peer_c.set_remote(server_addr_c);

    // Activate all peers so symmetric RTP guard allows outbound packets
    peer_a.activate().await;
    peer_b.activate().await;
    peer_c.activate().await;
    tokio::time::sleep(timing::scaled_ms(50)).await;

    // Start receiving on B and C
    peer_b.start_recv();
    peer_c.start_recv();
    tokio::time::sleep(timing::scaled_ms(50)).await;

    // Send from A → both B and C should receive
    peer_a.send_tone_for(Duration::from_millis(200)).await;
    tokio::time::sleep(timing::scaled_ms(500)).await;

    let b_received = peer_b.received_count();
    let c_received = peer_c.received_count();

    assert!(
        b_received > 0,
        "peer B should receive from A in 3-party, got {b_received}"
    );
    assert!(
        c_received > 0,
        "peer C should receive from A in 3-party, got {c_received}"
    );

    client.request_ok("session.destroy", json!({})).await;
}

/// Test: Cross-codec routing — PCMU endpoint A sends to G.722 endpoint B.
/// The bridge should transcode PCMU→G.722 automatically.
#[tokio::test]
async fn test_cross_codec_pcmu_to_g722() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;

    client.request_ok("session.create", json!({})).await;

    // Endpoint A: PCMU (peer provides offer with PCMU)
    let mut peer_a = TestRtpPeer::new().await;
    let offer_a = peer_a.make_sdp_offer();
    let result = client
        .request_ok(
            "endpoint.create_from_offer",
            json!({"sdp": offer_a, "direction": "sendrecv"}),
        )
        .await;
    let answer_a = result["sdp_answer"].as_str().unwrap();
    let server_addr_a = parse_rtp_addr_from_sdp(answer_a).expect("parse addr A");
    peer_a.set_remote(server_addr_a);

    // Endpoint B: G.722 (server creates offer, peer answers with G.722 only)
    let mut peer_b = TestRtpPeer::new().await;
    let result = client
        .request_ok(
            "endpoint.create_offer",
            json!({
                "type": "rtp",
                "direction": "sendrecv",
                "codecs": ["G722"]
            }),
        )
        .await;
    let ep_b_id = result["endpoint_id"].as_str().unwrap().to_string();
    let offer_b = result["sdp_offer"].as_str().unwrap();
    assert!(
        offer_b.contains("G722"),
        "offer B should include G.722: {offer_b}"
    );

    let server_addr_b = parse_rtp_addr_from_sdp(offer_b).expect("parse addr B");
    let answer_b = format!(
        "v=0\r\n\
         o=- 200 1 IN IP4 {ip}\r\n\
         s=-\r\n\
         c=IN IP4 {ip}\r\n\
         t=0 0\r\n\
         m=audio {port} RTP/AVP 9 101\r\n\
         a=rtpmap:9 G722/8000\r\n\
         a=rtpmap:101 telephone-event/8000\r\n\
         a=fmtp:101 0-16\r\n\
         a=sendrecv\r\n",
        ip = peer_b.local_addr.ip(),
        port = peer_b.local_addr.port(),
    );
    client
        .request_ok(
            "endpoint.accept_answer",
            json!({"endpoint_id": ep_b_id, "sdp": answer_b}),
        )
        .await;
    peer_b.set_remote(server_addr_b);

    // Activate both peers so symmetric RTP guard allows outbound packets
    peer_a.activate().await;
    peer_b.activate().await;
    tokio::time::sleep(timing::scaled_ms(50)).await;

    peer_b.start_recv();
    tokio::time::sleep(timing::scaled_ms(50)).await;

    // Send PCMU audio from peer A
    peer_a.send_tone_for(Duration::from_millis(300)).await;
    tokio::time::sleep(timing::scaled_ms(500)).await;

    let received = peer_b.received_count();
    assert!(
        received > 0,
        "peer B (G.722) should receive transcoded packets from peer A (PCMU), got {received}"
    );

    // Verify the received packet has G.722 payload type (PT=9)
    let raw = peer_b.last_received_raw().await;
    if raw.len() >= 12 {
        let pt = raw[1] & 0x7F;
        assert_eq!(pt, 9, "expected G.722 PT=9, got {pt}");
    }

    client.request_ok("session.destroy", json!({})).await;
}

/// Test: Symmetric RTP — bridge learns remote address from first inbound packet.
/// When the peer's actual address differs from the SDP (simulating NAT),
/// the bridge should route return traffic to the actual source address.
#[tokio::test]
async fn test_symmetric_rtp_address_learning() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;

    client.request_ok("session.create", json!({})).await;

    // Peer A: normal setup
    let mut peer_a = TestRtpPeer::new().await;
    let offer_a = peer_a.make_sdp_offer();
    let result = client
        .request_ok(
            "endpoint.create_from_offer",
            json!({"sdp": offer_a, "direction": "sendrecv"}),
        )
        .await;
    let answer_a = result["sdp_answer"].as_str().unwrap();
    let server_addr_a = parse_rtp_addr_from_sdp(answer_a).expect("parse addr A");
    peer_a.set_remote(server_addr_a);

    // Peer B: SDP declares one port, but real socket is on a different port (simulating NAT)
    let decoy_peer = TestRtpPeer::new().await; // This port goes in SDP
    let real_peer_b = TestRtpPeer::new().await; // This is where packets actually come from

    let fake_sdp_b = format!(
        "v=0\r\n\
         o=- 100 1 IN IP4 127.0.0.1\r\n\
         s=-\r\n\
         c=IN IP4 127.0.0.1\r\n\
         t=0 0\r\n\
         m=audio {port} RTP/AVP 0 101\r\n\
         a=rtpmap:0 PCMU/8000\r\n\
         a=rtpmap:101 telephone-event/8000\r\n\
         a=fmtp:101 0-16\r\n\
         a=sendrecv\r\n",
        port = decoy_peer.local_addr.port(),
    );
    let result = client
        .request_ok(
            "endpoint.create_from_offer",
            json!({"sdp": fake_sdp_b, "direction": "sendrecv"}),
        )
        .await;
    let answer_b = result["sdp_answer"].as_str().unwrap();
    let server_addr_b = parse_rtp_addr_from_sdp(answer_b).expect("parse addr B");

    let mut real_peer_b = real_peer_b;
    real_peer_b.set_remote(server_addr_b);

    // Activate both peers so symmetric RTP guard allows outbound packets
    peer_a.activate().await;
    real_peer_b.activate().await;
    tokio::time::sleep(timing::scaled_ms(50)).await;

    // real_peer_b starts receiving — this is where we expect routed packets to arrive
    real_peer_b.start_recv();
    tokio::time::sleep(timing::scaled_ms(50)).await;

    // real_peer_b sends RTP to trigger symmetric address learning
    real_peer_b.send_tone_for(Duration::from_millis(200)).await;
    tokio::time::sleep(timing::scaled_ms(200)).await;

    // Now peer A sends — bridge should route to real_peer_b's actual address
    peer_a.send_tone_for(Duration::from_millis(300)).await;
    tokio::time::sleep(timing::scaled_ms(500)).await;

    let received = real_peer_b.received_count();
    assert!(
        received > 0,
        "bridge should learn real address via symmetric RTP and route to it, got {received}"
    );

    client.request_ok("session.destroy", json!({})).await;
}

/// Test: Cross-codec Opus ↔ PCMU transcoding across endpoint types.
///
/// WebRTC endpoint (Opus-native) receives audio from a Plain RTP endpoint (PCMU).
/// The bridge must transcode PCMU→Opus in real time. This validates the live
/// transcoding path between heterogeneous endpoint types.
#[tokio::test]
async fn test_cross_codec_opus_pcmu_across_endpoint_types() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Instant;
    use str0m::change::SdpOffer;
    use str0m::net::{Protocol, Receive};
    use str0m::{Candidate, Event, Input, Output, RtcConfig};
    use tokio::net::UdpSocket;

    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;

    // Plain RTP endpoint (PCMU)
    let mut rtp_peer = TestRtpPeer::new().await;
    let offer_rtp = rtp_peer.make_sdp_offer();
    let result = client
        .request_ok(
            "endpoint.create_from_offer",
            json!({"sdp": offer_rtp, "direction": "sendrecv"}),
        )
        .await;
    let rtp_ep_id = result["endpoint_id"].as_str().unwrap().to_string();
    let answer_rtp = result["sdp_answer"].as_str().unwrap();
    rtp_peer.set_remote(parse_rtp_addr_from_sdp(answer_rtp).expect("parse server addr"));

    // Activate RTP peer so symmetric RTP guard allows outbound packets
    rtp_peer.activate().await;
    tokio::time::sleep(timing::scaled_ms(50)).await;

    rtp_peer.start_recv();

    // WebRTC endpoint (Opus negotiated by default)
    let result = client
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "webrtc", "direction": "sendrecv"}),
        )
        .await;
    let webrtc_ep_id = result["endpoint_id"].as_str().unwrap().to_string();
    let server_offer = result["sdp_offer"].as_str().unwrap();

    let peer_socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
    let peer_addr = peer_socket.local_addr().unwrap();

    let mut rtc = RtcConfig::new().set_rtp_mode(true).build(Instant::now());
    let candidate = Candidate::host(peer_addr, "udp").unwrap();
    rtc.add_local_candidate(candidate);

    let offer = SdpOffer::from_sdp_string(server_offer).unwrap();
    let answer = rtc.sdp_api().accept_offer(offer).unwrap();
    client
        .request_ok(
            "endpoint.accept_answer",
            json!({"endpoint_id": webrtc_ep_id, "sdp": answer.to_sdp_string()}),
        )
        .await;

    // Drive ICE to connected
    let deadline = Instant::now() + timing::scaled_ms(3000);
    let mut buf = vec![0u8; 2048];
    let mut connected = false;
    while Instant::now() < deadline {
        match rtc.poll_output() {
            Ok(Output::Timeout(when)) => {
                let wait = when
                    .checked_duration_since(Instant::now())
                    .unwrap_or(Duration::ZERO)
                    .min(Duration::from_millis(10));
                match tokio::time::timeout(wait, peer_socket.recv_from(&mut buf)).await {
                    Ok(Ok((n, source))) => {
                        if let Ok(r) = Receive::new(Protocol::Udp, source, peer_addr, &buf[..n]) {
                            let _ = rtc.handle_input(Input::Receive(Instant::now(), r));
                        }
                    }
                    _ => {
                        let _ = rtc.handle_input(Input::Timeout(Instant::now()));
                    }
                }
            }
            Ok(Output::Transmit(t)) => {
                let _ = peer_socket.send_to(&t.contents, t.destination).await;
            }
            Ok(Output::Event(Event::Connected)) => {
                connected = true;
                break;
            }
            Ok(Output::Event(_)) => {}
            Err(_) => break,
        }
    }
    assert!(connected, "ICE should reach Connected state");

    // Verify both endpoints exist with different types
    let info = client.request_ok("session.info", json!({})).await;
    let endpoints = info["endpoints"].as_array().unwrap();
    assert!(
        endpoints
            .iter()
            .any(|ep| ep["endpoint_id"].as_str().unwrap() == rtp_ep_id)
    );
    assert!(
        endpoints
            .iter()
            .any(|ep| ep["endpoint_id"].as_str().unwrap() == webrtc_ep_id)
    );

    // Send PCMU from RTP peer → bridge transcodes to Opus → WebRTC peer receives
    rtp_peer.send_tone_for(Duration::from_millis(500)).await;

    // Drive str0m to receive transcoded packets
    let webrtc_received = Arc::new(AtomicU64::new(0));
    let counter = Arc::clone(&webrtc_received);
    let deadline = Instant::now() + timing::scaled_ms(2000);
    while Instant::now() < deadline {
        match rtc.poll_output() {
            Ok(Output::Timeout(when)) => {
                let wait = when
                    .checked_duration_since(Instant::now())
                    .unwrap_or(Duration::ZERO)
                    .min(Duration::from_millis(10));
                match tokio::time::timeout(wait, peer_socket.recv_from(&mut buf)).await {
                    Ok(Ok((n, source))) => {
                        if let Ok(r) = Receive::new(Protocol::Udp, source, peer_addr, &buf[..n]) {
                            let _ = rtc.handle_input(Input::Receive(Instant::now(), r));
                        }
                    }
                    _ => {
                        let _ = rtc.handle_input(Input::Timeout(Instant::now()));
                    }
                }
            }
            Ok(Output::Transmit(t)) => {
                let _ = peer_socket.send_to(&t.contents, t.destination).await;
            }
            Ok(Output::Event(Event::RtpPacket(_))) => {
                counter.fetch_add(1, Ordering::Relaxed);
            }
            Ok(Output::Event(_)) => {}
            Err(_) => break,
        }
    }

    let received = webrtc_received.load(Ordering::Relaxed);
    assert!(
        received > 0,
        "WebRTC (Opus) peer should receive transcoded audio from RTP (PCMU) peer, got {received}"
    );

    client.request_ok("session.destroy", json!({})).await;
}

/// Test: Two G.722 endpoints can exchange audio (same codec passthrough).
#[tokio::test]
async fn test_g722_passthrough() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;

    client.request_ok("session.create", json!({})).await;

    // Endpoint A: peer provides G.722 offer
    let mut peer_a = TestRtpPeer::new().await;
    let offer_a = peer_a.make_g722_sdp_offer();
    let result = client
        .request_ok(
            "endpoint.create_from_offer",
            json!({"sdp": offer_a, "direction": "sendrecv"}),
        )
        .await;
    let answer_a = result["sdp_answer"].as_str().unwrap();
    assert!(
        answer_a.contains("G722"),
        "answer should include G722 codec"
    );
    let server_addr_a = parse_rtp_addr_from_sdp(answer_a).expect("parse server addr A");
    peer_a.set_remote(server_addr_a);

    // Endpoint B: server creates G.722 offer, peer answers
    let mut peer_b = TestRtpPeer::new().await;
    let result = client
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv", "codecs": ["G722"]}),
        )
        .await;
    let ep_b_id = result["endpoint_id"].as_str().unwrap().to_string();
    let offer_b = result["sdp_offer"].as_str().unwrap();
    let server_addr_b = parse_rtp_addr_from_sdp(offer_b).expect("parse server addr B");
    let answer_b = peer_b.make_g722_sdp_answer();
    client
        .request_ok(
            "endpoint.accept_answer",
            json!({"endpoint_id": ep_b_id, "sdp": answer_b}),
        )
        .await;
    peer_b.set_remote(server_addr_b);

    // Activate both peers so symmetric RTP guard allows outbound packets
    peer_a.activate().await;
    peer_b.activate().await;
    tokio::time::sleep(timing::scaled_ms(50)).await;

    // Start receiving on peer B
    peer_b.start_recv();
    tokio::time::sleep(timing::scaled_ms(50)).await;

    // Send G.722 audio from peer A
    peer_a.send_g722_tone_for(Duration::from_millis(300)).await;
    tokio::time::sleep(timing::scaled_ms(500)).await;

    let received = peer_b.received_count();
    assert!(
        received > 0,
        "peer B should receive G.722 passthrough packets from peer A, got {received}"
    );

    // Verify PT is 9 (G.722)
    let raw = peer_b.last_received_raw().await;
    if raw.len() >= 12 {
        let pt = raw[1] & 0x7F;
        assert_eq!(pt, 9, "expected G.722 PT=9, got {pt}");
    }

    client.request_ok("session.destroy", json!({})).await;
}

/// Test: Endpoint created via create_offer is NOT routable until accept_answer.
/// Reproduces the "No remote RTP address" bug where packets are routed to an
/// endpoint that hasn't completed SDP negotiation yet.
#[tokio::test]
async fn test_create_offer_not_routable_until_answer() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;

    client.request_ok("session.create", json!({})).await;

    // Endpoint A: immediately Connected (create_from_offer)
    let mut peer_a = TestRtpPeer::new().await;
    let offer_a = peer_a.make_sdp_offer();
    let result = client
        .request_ok(
            "endpoint.create_from_offer",
            json!({"sdp": offer_a, "direction": "sendrecv"}),
        )
        .await;
    let answer_a = result["sdp_answer"].as_str().unwrap();
    let server_addr_a = parse_rtp_addr_from_sdp(answer_a).expect("parse server addr A");
    peer_a.set_remote(server_addr_a);

    // Activate peer A so symmetric RTP guard allows outbound packets
    peer_a.activate().await;
    tokio::time::sleep(timing::scaled_ms(50)).await;

    // Endpoint B: create_offer puts it in Connecting state (no remote address)
    let mut peer_b = TestRtpPeer::new().await;
    let result = client
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv"}),
        )
        .await;
    let ep_b_id = result["endpoint_id"].as_str().unwrap().to_string();
    let offer_b = result["sdp_offer"].as_str().unwrap();
    let server_addr_b = parse_rtp_addr_from_sdp(offer_b).expect("parse server addr B");

    // B is in Connecting state — send audio from A; it should NOT produce
    // "No remote RTP address" errors (B should be excluded from routing)
    peer_a.send_tone_for(Duration::from_millis(100)).await;
    tokio::time::sleep(timing::scaled_ms(100)).await;

    // Verify session.info shows B is Connecting (not Connected)
    let info = client.request_ok("session.info", json!({})).await;
    let endpoints = info["endpoints"].as_array().unwrap();
    let ep_b_info = endpoints
        .iter()
        .find(|e| e["endpoint_id"].as_str().unwrap() == ep_b_id)
        .expect("endpoint B should exist");
    assert_eq!(
        ep_b_info["state"].as_str().unwrap(),
        "connecting",
        "endpoint B should still be in Connecting state"
    );

    // Now complete the SDP exchange — B becomes Connected and routable
    let answer_b = peer_b.make_sdp_answer();
    client
        .request_ok(
            "endpoint.accept_answer",
            json!({"endpoint_id": ep_b_id, "sdp": answer_b}),
        )
        .await;
    peer_b.set_remote(server_addr_b);

    // Activate peer B so symmetric RTP guard allows outbound packets
    peer_b.activate().await;
    tokio::time::sleep(timing::scaled_ms(50)).await;

    // Now audio from A should reach B
    peer_b.start_recv();
    tokio::time::sleep(timing::scaled_ms(50)).await;

    peer_a.send_tone_for(Duration::from_millis(200)).await;
    tokio::time::sleep(timing::scaled_ms(500)).await;

    let received = peer_b.received_count();
    assert!(
        received > 0,
        "peer B should receive audio after accept_answer, got {received}"
    );

    client.request_ok("session.destroy", json!({})).await;
}

/// Create two RTP endpoints: one PCMU-only, one Opus-only.
/// Send tone from PCMU endpoint, verify the Opus endpoint receives transcoded packets.
#[tokio::test]
async fn test_transcoding_pcmu_to_opus() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;

    client.request_ok("session.create", json!({})).await;

    // Endpoint A: PCMU-only (peer provides offer with PCMU)
    let mut peer_a = TestRtpPeer::new().await;
    let offer_a = peer_a.make_sdp_offer(); // offers PCMU + telephone-event
    let result = client
        .request_ok(
            "endpoint.create_from_offer",
            json!({"sdp": offer_a, "direction": "sendrecv"}),
        )
        .await;
    let answer_a = result["sdp_answer"].as_str().unwrap();
    let server_addr_a = parse_rtp_addr_from_sdp(answer_a).expect("parse server addr A");
    peer_a.set_remote(server_addr_a);

    // Endpoint B: Opus-only (server creates offer with Opus codec)
    let mut peer_b = TestRtpPeer::new().await;
    let result = client
        .request_ok(
            "endpoint.create_offer",
            json!({
                "type": "rtp",
                "direction": "sendrecv",
                "codecs": ["opus"]
            }),
        )
        .await;
    let ep_b_id = result["endpoint_id"].as_str().unwrap().to_string();
    let offer_b = result["sdp_offer"].as_str().unwrap();
    assert!(
        offer_b.to_lowercase().contains("opus"),
        "offer B should include Opus: {offer_b}"
    );

    let server_addr_b = parse_rtp_addr_from_sdp(offer_b).expect("parse addr B");

    // Build an answer from peer B accepting Opus
    let answer_b = format!(
        "v=0\r\n\
         o=- 200 1 IN IP4 {ip}\r\n\
         s=-\r\n\
         c=IN IP4 {ip}\r\n\
         t=0 0\r\n\
         m=audio {port} RTP/AVP 111 101\r\n\
         a=rtpmap:111 opus/48000/2\r\n\
         a=rtpmap:101 telephone-event/8000\r\n\
         a=fmtp:101 0-16\r\n\
         a=sendrecv\r\n",
        ip = peer_b.local_addr.ip(),
        port = peer_b.local_addr.port(),
    );
    client
        .request_ok(
            "endpoint.accept_answer",
            json!({"endpoint_id": ep_b_id, "sdp": answer_b}),
        )
        .await;
    peer_b.set_remote(server_addr_b);

    // Activate peer B so symmetric RTP guard allows outbound packets
    peer_b.activate().await;
    tokio::time::sleep(timing::scaled_ms(50)).await;

    // Start receiving on peer B
    peer_b.start_recv();
    tokio::time::sleep(timing::scaled_ms(100)).await;

    // Send PCMU audio from peer A
    peer_a.send_tone_for(timing::scaled_ms(500)).await;

    // Wait for transcoded packets to arrive
    tokio::time::sleep(timing::scaled_ms(1000)).await;

    let received = peer_b.received_count();
    assert!(
        received > 0,
        "peer B (Opus) should receive transcoded packets from peer A (PCMU), got {received}"
    );

    client.request_ok("session.destroy", json!({})).await;
}
