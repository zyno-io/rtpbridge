mod helpers;

use serde_json::json;

use helpers::control_client::TestControlClient;
use helpers::test_rtp_peer::{TestRtpPeer, build_rtp_packet, parse_rtp_addr_from_sdp};
use helpers::test_server::TestServer;
use helpers::timing;
use rtpbridge::media::srtp::SrtpContext;

/// Parse the base64 key from an `a=crypto:` SDP line.
/// Example line: `a=crypto:1 AES_CM_128_HMAC_SHA1_80 inline:BASE64KEY`
fn parse_crypto_key_from_sdp(sdp: &str) -> Option<String> {
    for line in sdp.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("a=crypto:") {
            // Format: "1 AES_CM_128_HMAC_SHA1_80 inline:KEY"
            if let Some(inline_pos) = rest.find("inline:") {
                let key_start = inline_pos + "inline:".len();
                let key = rest[key_start..].split_whitespace().next()?;
                // Strip any trailing '|' (lifetime params) if present
                let key = key.split('|').next()?;
                return Some(key.to_string());
            }
        }
    }
    None
}

/// Build an SRTP SDP answer with the peer's own crypto key and address
fn make_srtp_answer(peer: &TestRtpPeer, crypto_key: &str) -> String {
    format!(
        "v=0\r\n\
         o=- 300 1 IN IP4 {ip}\r\n\
         s=-\r\n\
         c=IN IP4 {ip}\r\n\
         t=0 0\r\n\
         m=audio {port} RTP/SAVP 0 101\r\n\
         a=rtpmap:0 PCMU/8000\r\n\
         a=rtpmap:101 telephone-event/8000\r\n\
         a=fmtp:101 0-16\r\n\
         a=crypto:1 AES_CM_128_HMAC_SHA1_80 inline:{key}\r\n\
         a=sendrecv\r\n",
        ip = peer.local_addr.ip(),
        port = peer.local_addr.port(),
        key = crypto_key,
    )
}

/// Test: Verify SRTP-encrypted media packets flow through the bridge.
///
/// The bridge creates SRTP endpoints with crypto keys. Peers use those keys
/// to encrypt outgoing RTP and decrypt incoming SRTP. This validates:
/// - SRTP crypto key exchange via SDP
/// - The bridge correctly decrypts inbound SRTP and re-encrypts for outbound
/// - Actual encrypted media traversal from peer A to peer B
#[tokio::test]
async fn test_srtp_encrypted_media_flow() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;

    client.request_ok("session.create", json!({})).await;

    // Create SRTP endpoint A (bridge creates offer with crypto)
    let result_a = client
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv", "srtp": true}),
        )
        .await;
    let ep_a_id = result_a["endpoint_id"].as_str().unwrap().to_string();
    let offer_a = result_a["sdp_offer"].as_str().unwrap();

    // Verify SRTP SDP structure
    assert!(offer_a.contains("RTP/SAVP"), "offer A should use RTP/SAVP");
    assert!(offer_a.contains("a=crypto:"), "offer A should have crypto");

    // Parse bridge's crypto key and address for endpoint A
    let bridge_key_a =
        parse_crypto_key_from_sdp(offer_a).expect("should parse crypto key from endpoint A offer");
    let server_addr_a = parse_rtp_addr_from_sdp(offer_a)
        .expect("should parse server address from endpoint A offer");

    // Create SRTP endpoint B
    let result_b = client
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv", "srtp": true}),
        )
        .await;
    let ep_b_id = result_b["endpoint_id"].as_str().unwrap().to_string();
    let offer_b = result_b["sdp_offer"].as_str().unwrap();

    let bridge_key_b =
        parse_crypto_key_from_sdp(offer_b).expect("should parse crypto key from endpoint B offer");
    let server_addr_b = parse_rtp_addr_from_sdp(offer_b)
        .expect("should parse server address from endpoint B offer");

    // The two endpoints should have different crypto keys
    assert_ne!(
        bridge_key_a, bridge_key_b,
        "each SRTP endpoint should generate a unique crypto key"
    );

    // Set up test peers
    let mut peer_a = TestRtpPeer::new().await;
    let mut peer_b = TestRtpPeer::new().await;

    // Accept answers with the bridge's own crypto key (peer echoes the key back)
    let answer_a = make_srtp_answer(&peer_a, &bridge_key_a);
    client
        .request_ok(
            "endpoint.accept_answer",
            json!({"endpoint_id": ep_a_id, "sdp": answer_a}),
        )
        .await;

    let answer_b = make_srtp_answer(&peer_b, &bridge_key_b);
    client
        .request_ok(
            "endpoint.accept_answer",
            json!({"endpoint_id": ep_b_id, "sdp": answer_b}),
        )
        .await;

    peer_a.set_remote(server_addr_a);
    peer_b.set_remote(server_addr_b);

    // Create SRTP contexts from the bridge's keys
    // The bridge uses the same key for both TX and RX when the peer echoes it back
    let mut ctx_a_tx = SrtpContext::from_sdes_key(&bridge_key_a)
        .expect("should create SRTP context for peer A TX");

    // Activate symmetric RTP with an SRTP-encrypted packet
    {
        let mut ctx_b_tx =
            SrtpContext::from_sdes_key(&bridge_key_b).expect("SRTP ctx for peer B activation");
        let rtp = build_rtp_packet(0, 0, 0, 0xAABBCCDD, false, &vec![0xFFu8; 160]);
        let srtp = ctx_b_tx.protect(&rtp).expect("SRTP protect");
        peer_b.socket.send_to(&srtp, server_addr_b).await.unwrap();
    }
    tokio::time::sleep(timing::scaled_ms(50)).await;
    peer_b.start_recv();

    // Send SRTP-encrypted packets from peer A to the bridge
    for seq in 0..10u16 {
        let payload: Vec<u8> = vec![0x80u8; 160]; // silence in PCMU
        let rtp = build_rtp_packet(0, seq, seq as u32 * 160, 0x12345678, seq == 0, &payload);
        let srtp = ctx_a_tx.protect(&rtp).expect("SRTP protect should succeed");
        peer_a
            .socket
            .send_to(&srtp, server_addr_a)
            .await
            .expect("send SRTP packet");
        tokio::time::sleep(timing::PACING).await;
    }

    // Wait for packets to traverse the bridge
    tokio::time::sleep(timing::scaled_ms(500)).await;

    // Peer B should have received forwarded packets
    let received_b = peer_b.received_count();
    assert!(
        received_b > 0,
        "peer B should have received SRTP-routed packets from the bridge, got {received_b}"
    );

    // Verify session info shows both endpoints
    let info = client.request_ok("session.info", json!({})).await;
    let endpoints = info["endpoints"].as_array().unwrap();
    assert_eq!(endpoints.len(), 2, "should have 2 SRTP endpoints");

    client.request_ok("session.destroy", json!({})).await;
}

/// Test: Verify that the crypto key extracted from the SDP is valid
/// and can be used to create an SrtpContext
#[tokio::test]
async fn test_srtp_key_validity() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;

    client.request_ok("session.create", json!({})).await;

    let result = client
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv", "srtp": true}),
        )
        .await;
    let offer = result["sdp_offer"].as_str().unwrap();

    // Parse the crypto key
    let key = parse_crypto_key_from_sdp(offer).expect("should parse crypto key");
    assert!(!key.is_empty(), "crypto key should not be empty");

    // The key should be valid base64 and produce a usable SrtpContext
    let ctx = SrtpContext::from_sdes_key(&key);
    assert!(
        ctx.is_ok(),
        "should create valid SrtpContext from bridge's key: {:?}",
        ctx.err()
    );

    // Verify the context can protect a packet (roundtrip test)
    let mut protect_ctx = ctx.unwrap();
    let rtp = build_rtp_packet(0, 1, 160, 0xAABBCCDD, false, &[0x80; 160]);
    let srtp = protect_ctx.protect(&rtp);
    assert!(srtp.is_ok(), "SRTP protect should succeed with bridge key");

    // SRTP packet should be longer (10-byte auth tag)
    let srtp = srtp.unwrap();
    assert_eq!(
        srtp.len(),
        rtp.len() + 10,
        "SRTP packet should include 10-byte auth tag"
    );

    client.request_ok("session.destroy", json!({})).await;
}

/// Test: SRTP rekey produces a new key and media continues flowing.
/// Verifies the dual-context grace period: old key still works briefly,
/// then new key takes over.
#[tokio::test]
async fn test_srtp_rekey_grace_period() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;

    client.request_ok("session.create", json!({})).await;

    // Create SRTP endpoint A
    let result_a = client
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv", "srtp": true}),
        )
        .await;
    let ep_a_id = result_a["endpoint_id"].as_str().unwrap().to_string();
    let offer_a = result_a["sdp_offer"].as_str().unwrap();
    let bridge_key_a = parse_crypto_key_from_sdp(offer_a).expect("parse key A");
    let server_addr_a = parse_rtp_addr_from_sdp(offer_a).expect("parse addr A");

    // Create SRTP endpoint B
    let result_b = client
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv", "srtp": true}),
        )
        .await;
    let ep_b_id = result_b["endpoint_id"].as_str().unwrap().to_string();
    let offer_b = result_b["sdp_offer"].as_str().unwrap();
    let bridge_key_b = parse_crypto_key_from_sdp(offer_b).expect("parse key B");
    let server_addr_b = parse_rtp_addr_from_sdp(offer_b).expect("parse addr B");

    // Wire up peers
    let mut peer_a = TestRtpPeer::new().await;
    let mut peer_b = TestRtpPeer::new().await;

    let answer_a = make_srtp_answer(&peer_a, &bridge_key_a);
    client
        .request_ok(
            "endpoint.accept_answer",
            json!({"endpoint_id": ep_a_id, "sdp": answer_a}),
        )
        .await;

    let answer_b = make_srtp_answer(&peer_b, &bridge_key_b);
    client
        .request_ok(
            "endpoint.accept_answer",
            json!({"endpoint_id": ep_b_id, "sdp": answer_b}),
        )
        .await;

    peer_a.set_remote(server_addr_a);
    peer_b.set_remote(server_addr_b);

    // Send SRTP from peer A with original key
    let mut ctx_a_tx = SrtpContext::from_sdes_key(&bridge_key_a).unwrap();
    // Activate symmetric RTP with an SRTP-encrypted packet
    {
        let mut ctx_b_tx =
            SrtpContext::from_sdes_key(&bridge_key_b).expect("SRTP ctx for peer B activation");
        let rtp = build_rtp_packet(0, 0, 0, 0xAABBCCDD, false, &vec![0xFFu8; 160]);
        let srtp = ctx_b_tx.protect(&rtp).expect("SRTP protect");
        peer_b.socket.send_to(&srtp, server_addr_b).await.unwrap();
    }
    tokio::time::sleep(timing::scaled_ms(50)).await;
    peer_b.start_recv();

    for seq in 0..5u16 {
        let rtp = build_rtp_packet(0, seq, seq as u32 * 160, 0x11111111, seq == 0, &[0x80; 160]);
        let srtp = ctx_a_tx.protect(&rtp).unwrap();
        peer_a.socket.send_to(&srtp, server_addr_a).await.unwrap();
        tokio::time::sleep(timing::PACING).await;
    }
    tokio::time::sleep(timing::scaled_ms(300)).await;

    let before_rekey = peer_b.received_count();
    assert!(before_rekey > 0, "should receive packets before rekey");

    // Rekey endpoint A — only TX is updated; RX update comes via accept_answer
    let rekey_result = client
        .request_ok("endpoint.srtp_rekey", json!({"endpoint_id": ep_a_id}))
        .await;
    let new_sdp = rekey_result["sdp"].as_str().unwrap();
    let new_key = parse_crypto_key_from_sdp(new_sdp).expect("parse new key");
    assert_ne!(
        new_key, bridge_key_a,
        "rekey should produce a different key"
    );

    // Old key should still work for RX (bridge's RX context unchanged)
    for seq in 5..10u16 {
        let rtp = build_rtp_packet(0, seq, seq as u32 * 160, 0x11111111, false, &[0x80; 160]);
        let srtp = ctx_a_tx.protect(&rtp).unwrap();
        peer_a.socket.send_to(&srtp, server_addr_a).await.unwrap();
        tokio::time::sleep(timing::PACING).await;
    }
    tokio::time::sleep(timing::scaled_ms(300)).await;

    let after_old_key = peer_b.received_count();
    assert!(
        after_old_key > before_rekey,
        "old key should still work after rekey: before={before_rekey}, after={after_old_key}"
    );

    // Simulate the peer's answer with the new key — this triggers the RX dual-context transition
    let answer_rekey = make_srtp_answer(&peer_a, &new_key);
    client
        .request_ok(
            "endpoint.accept_answer",
            json!({"endpoint_id": ep_a_id, "sdp": answer_rekey}),
        )
        .await;

    // Now switch to new key — should work via dual-context RX transition
    let mut ctx_a_new = SrtpContext::from_sdes_key(&new_key).unwrap();
    for seq in 10..15u16 {
        let rtp = build_rtp_packet(0, seq, seq as u32 * 160, 0x11111111, false, &[0x80; 160]);
        let srtp = ctx_a_new.protect(&rtp).unwrap();
        peer_a.socket.send_to(&srtp, server_addr_a).await.unwrap();
        tokio::time::sleep(timing::PACING).await;
    }
    tokio::time::sleep(timing::scaled_ms(300)).await;

    let after_new_key = peer_b.received_count();
    assert!(
        after_new_key > after_old_key,
        "new key should work after accept_answer: before={after_old_key}, after={after_new_key}"
    );

    client.request_ok("session.destroy", json!({})).await;
}

/// Test: RTCP packets are recorded in PCAP alongside SRTP packets.
/// The recorder captures plaintext RTCP (for analyzability) even when SRTCP is used on the wire.
/// This verifies that the RTCP recording path works for SRTP sessions.
#[tokio::test]
async fn test_rtcp_recorded_with_srtp_session() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;

    client.request_ok("session.create", json!({})).await;

    // Create two SRTP endpoints
    let result_a = client
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv", "srtp": true}),
        )
        .await;
    let ep_a_id = result_a["endpoint_id"].as_str().unwrap().to_string();
    let offer_a = result_a["sdp_offer"].as_str().unwrap();
    let bridge_key_a = parse_crypto_key_from_sdp(offer_a).unwrap();
    let server_addr_a = parse_rtp_addr_from_sdp(offer_a).unwrap();

    let result_b = client
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv", "srtp": true}),
        )
        .await;
    let ep_b_id = result_b["endpoint_id"].as_str().unwrap().to_string();
    let offer_b = result_b["sdp_offer"].as_str().unwrap();
    let bridge_key_b = parse_crypto_key_from_sdp(offer_b).unwrap();
    let server_addr_b = parse_rtp_addr_from_sdp(offer_b).unwrap();

    // Wire up peers
    let mut peer_a = TestRtpPeer::new().await;
    let mut peer_b = TestRtpPeer::new().await;

    let answer_a = make_srtp_answer(&peer_a, &bridge_key_a);
    client
        .request_ok(
            "endpoint.accept_answer",
            json!({"endpoint_id": ep_a_id, "sdp": answer_a}),
        )
        .await;

    let answer_b = make_srtp_answer(&peer_b, &bridge_key_b);
    client
        .request_ok(
            "endpoint.accept_answer",
            json!({"endpoint_id": ep_b_id, "sdp": answer_b}),
        )
        .await;

    peer_a.set_remote(server_addr_a);
    peer_b.set_remote(server_addr_b);

    // Start session-wide recording
    let pcap_path = std::path::Path::new(&server.recording_dir).join("srtcp-pcap-test.pcap");
    let rec = client
        .request_ok(
            "recording.start",
            json!({"endpoint_id": null, "file_path": pcap_path.to_str().unwrap()}),
        )
        .await;
    let rec_id = rec["recording_id"].as_str().unwrap().to_string();

    // Send SRTP from peer A for 7 seconds (RTCP is sent every ~5s)
    let mut ctx_a_tx = SrtpContext::from_sdes_key(&bridge_key_a).unwrap();
    // Activate symmetric RTP with an SRTP-encrypted packet
    {
        let mut ctx_b_tx =
            SrtpContext::from_sdes_key(&bridge_key_b).expect("SRTP ctx for peer B activation");
        let rtp = build_rtp_packet(0, 0, 0, 0xAABBCCDD, false, &vec![0xFFu8; 160]);
        let srtp = ctx_b_tx.protect(&rtp).expect("SRTP protect");
        peer_b.socket.send_to(&srtp, server_addr_b).await.unwrap();
    }
    tokio::time::sleep(timing::scaled_ms(50)).await;
    peer_b.start_recv();

    for seq in 0..350u16 {
        let rtp = build_rtp_packet(0, seq, seq as u32 * 160, 0x22222222, seq == 0, &[0x80; 160]);
        let srtp = ctx_a_tx.protect(&rtp).unwrap();
        peer_a.socket.send_to(&srtp, server_addr_a).await.unwrap();
        tokio::time::sleep(timing::PACING).await;
    }

    tokio::time::sleep(timing::scaled_ms(1000)).await;

    // Stop recording
    client
        .request_ok("recording.stop", json!({"recording_id": rec_id}))
        .await;
    tokio::time::sleep(timing::scaled_ms(500)).await;

    // Parse PCAP looking for SRTCP packets
    let file = std::fs::File::open(&pcap_path).expect("PCAP file should exist");
    let mut reader = pcap_file::pcap::PcapReader::new(file).expect("valid PCAP file");

    let payload_offset = 14 + 20 + 8; // Eth + IPv4 + UDP
    let mut rtp_count = 0u32;
    let mut rtcp_count = 0u32;

    while let Some(pkt) = reader.next_packet() {
        let pkt = pkt.expect("valid PCAP packet");
        let data = &pkt.data;

        if data.len() <= payload_offset + 2 {
            continue;
        }

        let udp_payload = &data[payload_offset..];
        let version = (udp_payload[0] >> 6) & 0x03;

        if version != 2 {
            continue;
        }

        let pt = udp_payload[1];

        // The recorder captures plaintext RTCP (PT 200=SR, 201=RR)
        // even when SRTCP is used on the wire.
        if pt == 200 || pt == 201 {
            rtcp_count += 1;
        } else {
            rtp_count += 1;
        }
    }

    assert!(
        rtp_count > 0,
        "should have RTP packets in PCAP from SRTP session"
    );
    assert!(
        rtcp_count > 0,
        "should have RTCP packets in PCAP from SRTP session \
         (got {rtcp_count} RTCP, {rtp_count} RTP)"
    );

    client.request_ok("session.destroy", json!({})).await;
}

/// Test: SRTP packets with corrupted auth tags are silently dropped.
/// Peer B should not receive any packets when peer A sends corrupted SRTP.
#[tokio::test]
async fn test_srtp_corrupted_packets_rejected() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;

    client.request_ok("session.create", json!({})).await;

    // Create two SRTP endpoints
    let result_a = client
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv", "srtp": true}),
        )
        .await;
    let ep_a_id = result_a["endpoint_id"].as_str().unwrap().to_string();
    let offer_a = result_a["sdp_offer"].as_str().unwrap();
    let bridge_key_a = parse_crypto_key_from_sdp(offer_a).unwrap();
    let server_addr_a = parse_rtp_addr_from_sdp(offer_a).unwrap();

    let result_b = client
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv", "srtp": true}),
        )
        .await;
    let ep_b_id = result_b["endpoint_id"].as_str().unwrap().to_string();
    let offer_b = result_b["sdp_offer"].as_str().unwrap();
    let bridge_key_b = parse_crypto_key_from_sdp(offer_b).unwrap();
    let server_addr_b = parse_rtp_addr_from_sdp(offer_b).unwrap();

    let mut peer_a = TestRtpPeer::new().await;
    let mut peer_b = TestRtpPeer::new().await;

    let answer_a = make_srtp_answer(&peer_a, &bridge_key_a);
    client
        .request_ok(
            "endpoint.accept_answer",
            json!({"endpoint_id": ep_a_id, "sdp": answer_a}),
        )
        .await;

    let answer_b = make_srtp_answer(&peer_b, &bridge_key_b);
    client
        .request_ok(
            "endpoint.accept_answer",
            json!({"endpoint_id": ep_b_id, "sdp": answer_b}),
        )
        .await;

    peer_a.set_remote(server_addr_a);
    peer_b.set_remote(server_addr_b);
    // Activate symmetric RTP with an SRTP-encrypted packet
    {
        let mut ctx_b_tx =
            SrtpContext::from_sdes_key(&bridge_key_b).expect("SRTP ctx for peer B activation");
        let rtp = build_rtp_packet(0, 0, 0, 0xAABBCCDD, false, &vec![0xFFu8; 160]);
        let srtp = ctx_b_tx.protect(&rtp).expect("SRTP protect");
        peer_b.socket.send_to(&srtp, server_addr_b).await.unwrap();
    }
    tokio::time::sleep(timing::scaled_ms(50)).await;
    peer_b.start_recv();

    // Send SRTP packets with CORRUPTED auth tags
    let mut ctx_a_tx = SrtpContext::from_sdes_key(&bridge_key_a).unwrap();
    for seq in 0..10u16 {
        let rtp = build_rtp_packet(0, seq, seq as u32 * 160, 0x33333333, seq == 0, &[0x80; 160]);
        let mut srtp = ctx_a_tx.protect(&rtp).unwrap();
        // Corrupt the last byte of the auth tag
        let last = srtp.len() - 1;
        srtp[last] ^= 0xFF;
        peer_a.socket.send_to(&srtp, server_addr_a).await.unwrap();
        tokio::time::sleep(timing::PACING).await;
    }

    tokio::time::sleep(timing::scaled_ms(500)).await;

    // Peer B should NOT have received any packets (all were corrupted)
    let received_corrupted = peer_b.received_count();
    assert_eq!(
        received_corrupted, 0,
        "corrupted SRTP packets should be dropped, but peer B received {received_corrupted}"
    );

    // Now send valid SRTP packets to confirm the path works
    let mut ctx_a_valid = SrtpContext::from_sdes_key(&bridge_key_a).unwrap();
    for seq in 10..20u16 {
        let rtp = build_rtp_packet(0, seq, seq as u32 * 160, 0x33333333, false, &[0x80; 160]);
        let srtp = ctx_a_valid.protect(&rtp).unwrap();
        peer_a.socket.send_to(&srtp, server_addr_a).await.unwrap();
        tokio::time::sleep(timing::PACING).await;
    }

    tokio::time::sleep(timing::scaled_ms(500)).await;

    let received_valid = peer_b.received_count();
    assert!(
        received_valid > 0,
        "valid SRTP packets should be forwarded after corrupted ones were dropped, got {received_valid}"
    );

    client.request_ok("session.destroy", json!({})).await;
}

/// Test: OSRTP bidirectional — full encrypted peer exchange.
///
/// Creates two OSRTP endpoints (RTP/AVP + a=crypto), sends encrypted media
/// in both directions using SrtpContext, and verifies both peers receive
/// decryptable audio through the bridge.
#[tokio::test]
async fn test_osrtp_bidirectional_encrypted_exchange() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;

    // Peer A sends an OSRTP offer: RTP/AVP profile with a=crypto line
    let peer_a = TestRtpPeer::new().await;
    let peer_a_key_bytes: Vec<u8> = (0..30).map(|_| rand::random::<u8>()).collect();
    let peer_a_key_b64 = base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        &peer_a_key_bytes,
    );
    let osrtp_offer_a = format!(
        "v=0\r\n\
         o=- 100 1 IN IP4 {ip}\r\n\
         s=-\r\n\
         c=IN IP4 {ip}\r\n\
         t=0 0\r\n\
         m=audio {port} RTP/AVP 0 101\r\n\
         a=rtpmap:0 PCMU/8000\r\n\
         a=rtpmap:101 telephone-event/8000\r\n\
         a=fmtp:101 0-16\r\n\
         a=crypto:1 AES_CM_128_HMAC_SHA1_80 inline:{key}\r\n\
         a=sendrecv\r\n",
        ip = peer_a.local_addr.ip(),
        port = peer_a.local_addr.port(),
        key = peer_a_key_b64,
    );

    let result = client
        .request_ok(
            "endpoint.create_from_offer",
            json!({"sdp": osrtp_offer_a, "direction": "sendrecv"}),
        )
        .await;
    let answer_a = result["sdp_answer"].as_str().unwrap();

    // Answer should echo back crypto (OSRTP)
    assert!(
        answer_a.contains("a=crypto:"),
        "OSRTP answer should contain crypto line: {answer_a}"
    );
    let bridge_key_a =
        parse_crypto_key_from_sdp(answer_a).expect("parse bridge crypto key from OSRTP answer");
    let server_addr_a = parse_rtp_addr_from_sdp(answer_a).expect("parse server addr A");

    // Create SRTP endpoint B (server creates offer with srtp=true)
    let result = client
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv", "srtp": true}),
        )
        .await;
    let ep_b_id = result["endpoint_id"].as_str().unwrap().to_string();
    let offer_b = result["sdp_offer"].as_str().unwrap();
    assert!(
        offer_b.contains("a=crypto:"),
        "SRTP offer should have crypto line"
    );
    let bridge_key_b =
        parse_crypto_key_from_sdp(offer_b).expect("parse bridge crypto key from offer B");
    let server_addr_b = parse_rtp_addr_from_sdp(offer_b).expect("parse server addr B");

    // Peer B answers echoing bridge's key
    let peer_b = TestRtpPeer::new().await;
    let answer_b = make_srtp_answer(&peer_b, &bridge_key_b);
    client
        .request_ok(
            "endpoint.accept_answer",
            json!({"endpoint_id": ep_b_id, "sdp": answer_b}),
        )
        .await;

    // SRTP contexts for sending
    let mut ctx_a_tx =
        SrtpContext::from_sdes_key(&peer_a_key_b64).expect("SRTP context for peer A TX");
    let mut ctx_b_tx =
        SrtpContext::from_sdes_key(&bridge_key_b).expect("SRTP context for peer B TX");

    let socket_a = Arc::clone(&peer_a.socket);
    let socket_b = Arc::clone(&peer_b.socket);

    let recv_count_a = Arc::new(AtomicU64::new(0));
    let recv_count_b = Arc::new(AtomicU64::new(0));

    // Peer B receive loop (decrypt with bridge_key_b)
    let counter_b = Arc::clone(&recv_count_b);
    let key_b_rx = bridge_key_b.clone();
    let recv_b = tokio::spawn({
        let socket_b = Arc::clone(&socket_b);
        async move {
            let mut buf = vec![0u8; 2048];
            let mut rx = SrtpContext::from_sdes_key(&key_b_rx).unwrap();
            loop {
                match tokio::time::timeout(timing::scaled_ms(5000), socket_b.recv_from(&mut buf))
                    .await
                {
                    Ok(Ok((n, _))) if n >= 12 => {
                        if rx.unprotect(&buf[..n]).is_ok() {
                            counter_b.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    _ => break,
                }
            }
        }
    });

    // Peer A receive loop (decrypt with bridge_key_a)
    let counter_a = Arc::clone(&recv_count_a);
    let key_a_rx = bridge_key_a.clone();
    let recv_a = tokio::spawn({
        let socket_a = Arc::clone(&socket_a);
        async move {
            let mut buf = vec![0u8; 2048];
            let mut rx = SrtpContext::from_sdes_key(&key_a_rx).unwrap();
            loop {
                match tokio::time::timeout(timing::scaled_ms(5000), socket_a.recv_from(&mut buf))
                    .await
                {
                    Ok(Ok((n, _))) if n >= 12 => {
                        if rx.unprotect(&buf[..n]).is_ok() {
                            counter_a.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    _ => break,
                }
            }
        }
    });

    tokio::time::sleep(timing::scaled_ms(100)).await;

    // Activate symmetric RTP on both endpoints with SRTP-encrypted packets
    // Use the same SSRCs and contexts as the main data flows, consuming seq=0
    {
        let rtp = build_rtp_packet(0, 0, 0, 0xAAAAAAAA, false, &vec![0xFFu8; 160]);
        let srtp = ctx_a_tx.protect(&rtp).expect("SRTP protect A activation");
        socket_a.send_to(&srtp, server_addr_a).await.unwrap();
    }
    {
        let rtp = build_rtp_packet(0, 0, 0, 0xBBBBBBBB, false, &vec![0xFFu8; 160]);
        let srtp = ctx_b_tx.protect(&rtp).expect("SRTP protect B activation");
        socket_b.send_to(&srtp, server_addr_b).await.unwrap();
    }
    tokio::time::sleep(timing::scaled_ms(100)).await;

    // A→B: Peer A sends SRTP (starting from seq=1, since seq=0 was used for activation)
    for seq in 1..21u16 {
        let rtp = build_rtp_packet(0, seq, seq as u32 * 160, 0xAAAAAAAA, seq == 1, &[0x80; 160]);
        let srtp = ctx_a_tx.protect(&rtp).expect("SRTP protect A");
        socket_a.send_to(&srtp, server_addr_a).await.unwrap();
        tokio::time::sleep(timing::PACING).await;
    }
    tokio::time::sleep(timing::scaled_ms(500)).await;

    // B→A: Peer B sends SRTP (starting from seq=1, since seq=0 was used for activation)
    for seq in 1..21u16 {
        let rtp = build_rtp_packet(0, seq, seq as u32 * 160, 0xBBBBBBBB, seq == 1, &[0x80; 160]);
        let srtp = ctx_b_tx.protect(&rtp).expect("SRTP protect B");
        socket_b.send_to(&srtp, server_addr_b).await.unwrap();
        tokio::time::sleep(timing::PACING).await;
    }
    tokio::time::sleep(timing::scaled_ms(1000)).await;

    recv_a.abort();
    recv_b.abort();

    let a_received = recv_count_a.load(Ordering::Relaxed);
    let b_received = recv_count_b.load(Ordering::Relaxed);

    assert!(
        b_received > 0,
        "peer B should receive decryptable SRTP packets from peer A, got {b_received}"
    );
    assert!(
        a_received > 0,
        "peer A should receive decryptable SRTP packets from peer B, got {a_received}"
    );

    client.request_ok("session.destroy", json!({})).await;
}

/// Test: Replayed SRTP packets are rejected; new packets after replay are accepted
#[tokio::test]
async fn test_srtp_replay_rejection_e2e() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;

    // Create two SRTP endpoints
    let result_a = client
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv", "srtp": true}),
        )
        .await;
    let ep_a_id = result_a["endpoint_id"].as_str().unwrap().to_string();
    let offer_a = result_a["sdp_offer"].as_str().unwrap();
    let bridge_key_a = parse_crypto_key_from_sdp(offer_a).unwrap();
    let server_addr_a = parse_rtp_addr_from_sdp(offer_a).unwrap();

    let result_b = client
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv", "srtp": true}),
        )
        .await;
    let ep_b_id = result_b["endpoint_id"].as_str().unwrap().to_string();
    let offer_b = result_b["sdp_offer"].as_str().unwrap();
    let bridge_key_b = parse_crypto_key_from_sdp(offer_b).unwrap();
    let server_addr_b = parse_rtp_addr_from_sdp(offer_b).unwrap();

    let mut peer_a = TestRtpPeer::new().await;
    let mut peer_b = TestRtpPeer::new().await;

    let answer_a = make_srtp_answer(&peer_a, &bridge_key_a);
    client
        .request_ok(
            "endpoint.accept_answer",
            json!({"endpoint_id": ep_a_id, "sdp": answer_a}),
        )
        .await;
    let answer_b = make_srtp_answer(&peer_b, &bridge_key_b);
    client
        .request_ok(
            "endpoint.accept_answer",
            json!({"endpoint_id": ep_b_id, "sdp": answer_b}),
        )
        .await;

    peer_a.set_remote(server_addr_a);
    peer_b.set_remote(server_addr_b);
    // Activate symmetric RTP with an SRTP-encrypted packet
    {
        let mut ctx_b_tx =
            SrtpContext::from_sdes_key(&bridge_key_b).expect("SRTP ctx for peer B activation");
        let rtp = build_rtp_packet(0, 0, 0, 0xAABBCCDD, false, &vec![0xFFu8; 160]);
        let srtp = ctx_b_tx.protect(&rtp).expect("SRTP protect");
        peer_b.socket.send_to(&srtp, server_addr_b).await.unwrap();
    }
    tokio::time::sleep(timing::scaled_ms(50)).await;
    peer_b.start_recv();

    // Send 10 SRTP packets and save encrypted bytes for replay
    let mut ctx_a = SrtpContext::from_sdes_key(&bridge_key_a).unwrap();
    let mut saved_packets: Vec<Vec<u8>> = Vec::new();

    for seq in 0..10u16 {
        let rtp = build_rtp_packet(0, seq, seq as u32 * 160, 0x44444444, seq == 0, &[0x80; 160]);
        let srtp = ctx_a.protect(&rtp).unwrap();
        saved_packets.push(srtp.clone());
        peer_a.socket.send_to(&srtp, server_addr_a).await.unwrap();
        tokio::time::sleep(timing::PACING).await;
    }
    tokio::time::sleep(timing::scaled_ms(500)).await;

    let after_initial = peer_b.received_count();
    assert!(
        after_initial > 0,
        "peer B should receive initial SRTP packets"
    );

    // Replay the exact same encrypted packets
    for saved in &saved_packets {
        peer_a.socket.send_to(saved, server_addr_a).await.unwrap();
        tokio::time::sleep(timing::PACING).await;
    }
    tokio::time::sleep(timing::scaled_ms(500)).await;

    let after_replay = peer_b.received_count();
    assert_eq!(
        after_replay, after_initial,
        "replayed packets should be rejected: before={after_initial}, after={after_replay}"
    );

    // Send new (non-replayed) packets → should be accepted
    for seq in 10..20u16 {
        let rtp = build_rtp_packet(0, seq, seq as u32 * 160, 0x44444444, false, &[0x80; 160]);
        let srtp = ctx_a.protect(&rtp).unwrap();
        peer_a.socket.send_to(&srtp, server_addr_a).await.unwrap();
        tokio::time::sleep(timing::PACING).await;
    }
    tokio::time::sleep(timing::scaled_ms(500)).await;

    let after_new = peer_b.received_count();
    assert!(
        after_new > after_replay,
        "new packets after replay should be accepted: before={after_replay}, after={after_new}"
    );

    client.request_ok("session.destroy", json!({})).await;
}

/// Test: SRTP packets encrypted with an incorrect key are discarded by the bridge.
///
/// Peer A first sends packets protected with a randomly-generated wrong key.
/// Peer B should receive 0 packets because the bridge cannot verify the auth tag.
/// Then peer A switches to the correct key and peer B starts receiving packets.
#[tokio::test]
async fn test_srtp_auth_failure_discards_packets() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;

    client.request_ok("session.create", json!({})).await;

    // Create SRTP endpoint A
    let result_a = client
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv", "srtp": true}),
        )
        .await;
    let ep_a_id = result_a["endpoint_id"].as_str().unwrap().to_string();
    let offer_a = result_a["sdp_offer"].as_str().unwrap();
    let bridge_key_a = parse_crypto_key_from_sdp(offer_a).expect("parse key A");
    let server_addr_a = parse_rtp_addr_from_sdp(offer_a).expect("parse addr A");

    // Create SRTP endpoint B
    let result_b = client
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv", "srtp": true}),
        )
        .await;
    let ep_b_id = result_b["endpoint_id"].as_str().unwrap().to_string();
    let offer_b = result_b["sdp_offer"].as_str().unwrap();
    let bridge_key_b = parse_crypto_key_from_sdp(offer_b).expect("parse key B");
    let server_addr_b = parse_rtp_addr_from_sdp(offer_b).expect("parse addr B");

    // Wire up peers
    let mut peer_a = TestRtpPeer::new().await;
    let mut peer_b = TestRtpPeer::new().await;

    let answer_a = make_srtp_answer(&peer_a, &bridge_key_a);
    client
        .request_ok(
            "endpoint.accept_answer",
            json!({"endpoint_id": ep_a_id, "sdp": answer_a}),
        )
        .await;

    let answer_b = make_srtp_answer(&peer_b, &bridge_key_b);
    client
        .request_ok(
            "endpoint.accept_answer",
            json!({"endpoint_id": ep_b_id, "sdp": answer_b}),
        )
        .await;

    peer_a.set_remote(server_addr_a);
    peer_b.set_remote(server_addr_b);
    // Activate symmetric RTP with an SRTP-encrypted packet
    {
        let mut ctx_b_tx =
            SrtpContext::from_sdes_key(&bridge_key_b).expect("SRTP ctx for peer B activation");
        let rtp = build_rtp_packet(0, 0, 0, 0xAABBCCDD, false, &vec![0xFFu8; 160]);
        let srtp = ctx_b_tx.protect(&rtp).expect("SRTP protect");
        peer_b.socket.send_to(&srtp, server_addr_b).await.unwrap();
    }
    tokio::time::sleep(timing::scaled_ms(50)).await;
    peer_b.start_recv();

    // Generate a random WRONG key (30 bytes, base64-encoded)
    let wrong_key_bytes: Vec<u8> = (0..30).map(|_| rand::random::<u8>()).collect();
    let wrong_key_b64 =
        base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &wrong_key_bytes);
    // Ensure the wrong key is actually different from the bridge key
    assert_ne!(
        wrong_key_b64, bridge_key_a,
        "random key should differ from bridge key"
    );

    // Send SRTP packets encrypted with the WRONG key
    let mut ctx_wrong = SrtpContext::from_sdes_key(&wrong_key_b64)
        .expect("should create SRTP context from wrong key");
    for seq in 0..10u16 {
        let rtp = build_rtp_packet(0, seq, seq as u32 * 160, 0x55555555, seq == 0, &[0x80; 160]);
        let srtp = ctx_wrong
            .protect(&rtp)
            .expect("SRTP protect with wrong key");
        peer_a.socket.send_to(&srtp, server_addr_a).await.unwrap();
        tokio::time::sleep(timing::PACING).await;
    }

    tokio::time::sleep(timing::scaled_ms(500)).await;

    // Peer B should have received 0 packets — wrong-key packets should be discarded
    let received_wrong = peer_b.received_count();
    assert_eq!(
        received_wrong, 0,
        "wrong-key SRTP packets should be discarded by bridge, but peer B received {received_wrong}"
    );

    // Now send packets with the CORRECT key
    let mut ctx_correct = SrtpContext::from_sdes_key(&bridge_key_a)
        .expect("should create SRTP context from correct key");
    for seq in 10..20u16 {
        let rtp = build_rtp_packet(0, seq, seq as u32 * 160, 0x55555555, false, &[0x80; 160]);
        let srtp = ctx_correct
            .protect(&rtp)
            .expect("SRTP protect with correct key");
        peer_a.socket.send_to(&srtp, server_addr_a).await.unwrap();
        tokio::time::sleep(timing::PACING).await;
    }

    tokio::time::sleep(timing::scaled_ms(500)).await;

    // Peer B should now have received packets
    let received_correct = peer_b.received_count();
    assert!(
        received_correct > 0,
        "correct-key SRTP packets should be forwarded to peer B, got {received_correct}"
    );

    client.request_ok("session.destroy", json!({})).await;
}

/// Test: SRTP-to-plain-RTP bridging.
///
/// Endpoint A is SRTP (encrypted). Endpoint B is plain RTP (unencrypted).
/// Peer A sends SRTP-encrypted packets to the bridge. The bridge decrypts them
/// and forwards plain RTP to peer B. Verifies that cross-mode bridging works.
#[tokio::test]
async fn test_srtp_to_plain_rtp_bridging() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;

    client.request_ok("session.create", json!({})).await;

    // Create SRTP endpoint A (bridge creates offer with crypto)
    let result_a = client
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv", "srtp": true}),
        )
        .await;
    let ep_a_id = result_a["endpoint_id"].as_str().unwrap().to_string();
    let offer_a = result_a["sdp_offer"].as_str().unwrap();
    assert!(
        offer_a.contains("RTP/SAVP"),
        "SRTP offer should use RTP/SAVP"
    );
    assert!(
        offer_a.contains("a=crypto:"),
        "SRTP offer should have crypto line"
    );
    let bridge_key_a = parse_crypto_key_from_sdp(offer_a).expect("parse crypto key A");
    let server_addr_a = parse_rtp_addr_from_sdp(offer_a).expect("parse server addr A");

    // Create plain RTP endpoint B (no srtp flag)
    let result_b = client
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv"}),
        )
        .await;
    let ep_b_id = result_b["endpoint_id"].as_str().unwrap().to_string();
    let offer_b = result_b["sdp_offer"].as_str().unwrap();
    // Plain RTP should use RTP/AVP, not RTP/SAVP
    assert!(
        offer_b.contains("RTP/AVP"),
        "plain RTP offer should use RTP/AVP: {offer_b}"
    );
    let server_addr_b = parse_rtp_addr_from_sdp(offer_b).expect("parse server addr B");

    // Set up peers
    let mut peer_a = TestRtpPeer::new().await;
    let mut peer_b = TestRtpPeer::new().await;

    // Accept answer for SRTP endpoint A (with crypto key)
    let answer_a = make_srtp_answer(&peer_a, &bridge_key_a);
    client
        .request_ok(
            "endpoint.accept_answer",
            json!({"endpoint_id": ep_a_id, "sdp": answer_a}),
        )
        .await;

    // Accept answer for plain RTP endpoint B (no crypto)
    let answer_b = peer_b.make_sdp_answer();
    client
        .request_ok(
            "endpoint.accept_answer",
            json!({"endpoint_id": ep_b_id, "sdp": answer_b}),
        )
        .await;

    peer_a.set_remote(server_addr_a);
    peer_b.set_remote(server_addr_b);

    // Activate symmetric RTP and start receiving on peer B (plain RTP)
    peer_b.activate().await;
    tokio::time::sleep(timing::scaled_ms(50)).await;
    peer_b.start_recv();

    // Peer A sends SRTP-encrypted packets to the bridge
    let mut ctx_a_tx = SrtpContext::from_sdes_key(&bridge_key_a)
        .expect("should create SRTP context for peer A TX");
    for seq in 0..10u16 {
        let payload: Vec<u8> = vec![0x80u8; 160]; // silence in PCMU
        let rtp = build_rtp_packet(0, seq, seq as u32 * 160, 0x66666666, seq == 0, &payload);
        let srtp = ctx_a_tx.protect(&rtp).expect("SRTP protect should succeed");
        peer_a.socket.send_to(&srtp, server_addr_a).await.unwrap();
        tokio::time::sleep(timing::PACING).await;
    }

    tokio::time::sleep(timing::scaled_ms(500)).await;

    // Peer B should have received plain RTP packets (bridge decrypts SRTP and sends plain RTP)
    let received_b = peer_b.received_count();
    assert!(
        received_b > 0,
        "peer B should receive plain RTP packets bridged from SRTP endpoint A, got {received_b}"
    );

    client.request_ok("session.destroy", json!({})).await;
}

/// Test: SRTP duplicate/replayed sequence numbers are rejected.
///
/// Peer A sends 5 sequential SRTP packets, then replays one with a reused
/// sequence number. The bridge's SRTP replay protection should discard
/// the duplicate, so peer B should receive exactly 5 packets, not 6.
#[tokio::test]
async fn test_srtp_duplicate_sequence_rejected() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;

    client.request_ok("session.create", json!({})).await;

    // Create SRTP endpoint A
    let result_a = client
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv", "srtp": true}),
        )
        .await;
    let ep_a_id = result_a["endpoint_id"].as_str().unwrap().to_string();
    let offer_a = result_a["sdp_offer"].as_str().unwrap();
    let bridge_key_a = parse_crypto_key_from_sdp(offer_a).expect("parse key A");
    let server_addr_a = parse_rtp_addr_from_sdp(offer_a).expect("parse addr A");

    // Create SRTP endpoint B
    let result_b = client
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv", "srtp": true}),
        )
        .await;
    let ep_b_id = result_b["endpoint_id"].as_str().unwrap().to_string();
    let offer_b = result_b["sdp_offer"].as_str().unwrap();
    let bridge_key_b = parse_crypto_key_from_sdp(offer_b).expect("parse key B");
    let server_addr_b = parse_rtp_addr_from_sdp(offer_b).expect("parse addr B");

    // Wire up peers
    let mut peer_a = TestRtpPeer::new().await;
    let mut peer_b = TestRtpPeer::new().await;

    let answer_a = make_srtp_answer(&peer_a, &bridge_key_a);
    client
        .request_ok(
            "endpoint.accept_answer",
            json!({"endpoint_id": ep_a_id, "sdp": answer_a}),
        )
        .await;

    let answer_b = make_srtp_answer(&peer_b, &bridge_key_b);
    client
        .request_ok(
            "endpoint.accept_answer",
            json!({"endpoint_id": ep_b_id, "sdp": answer_b}),
        )
        .await;

    peer_a.set_remote(server_addr_a);
    peer_b.set_remote(server_addr_b);
    // Activate symmetric RTP with an SRTP-encrypted packet
    {
        let mut ctx_b_tx =
            SrtpContext::from_sdes_key(&bridge_key_b).expect("SRTP ctx for peer B activation");
        let rtp = build_rtp_packet(0, 0, 0, 0xAABBCCDD, false, &vec![0xFFu8; 160]);
        let srtp = ctx_b_tx.protect(&rtp).expect("SRTP protect");
        peer_b.socket.send_to(&srtp, server_addr_b).await.unwrap();
    }
    tokio::time::sleep(timing::scaled_ms(50)).await;
    peer_b.start_recv();

    // Send 5 SRTP packets with sequential seq numbers and save them
    let mut ctx_a =
        SrtpContext::from_sdes_key(&bridge_key_a).expect("should create SRTP context for peer A");
    let mut saved_packets: Vec<Vec<u8>> = Vec::new();

    for seq in 0..5u16 {
        let rtp = build_rtp_packet(0, seq, seq as u32 * 160, 0x77777777, seq == 0, &[0x80; 160]);
        let srtp = ctx_a.protect(&rtp).expect("SRTP protect");
        saved_packets.push(srtp.clone());
        peer_a.socket.send_to(&srtp, server_addr_a).await.unwrap();
        tokio::time::sleep(timing::PACING).await;
    }

    tokio::time::sleep(timing::scaled_ms(500)).await;

    let after_initial = peer_b.received_count();
    assert!(
        after_initial > 0,
        "peer B should receive the initial 5 SRTP packets, got {after_initial}"
    );

    // Replay packet with seq=2 (an earlier sequence number already seen by the bridge).
    // This is a replay attack — the bridge should reject it via its replay window.
    let replayed = &saved_packets[2];
    peer_a
        .socket
        .send_to(replayed, server_addr_a)
        .await
        .unwrap();
    tokio::time::sleep(timing::scaled_ms(500)).await;

    let after_replay = peer_b.received_count();
    // The replayed packet should be rejected by SRTP replay protection.
    // Peer B's count should not increase beyond what it was after the initial 5 packets.
    assert_eq!(
        after_replay, after_initial,
        "replayed duplicate seq should be rejected: count before replay={after_initial}, after={after_replay}"
    );

    client.request_ok("session.destroy", json!({})).await;
}

// ── SRTP rekey error path tests ──────────────────────────────────────────

/// Test: SRTP rekey during continuous media flow.
///
/// Peer A sends a steady stream of SRTP packets while a rekey occurs mid-flow.
/// Verifies that packets continue flowing across the rekey boundary with no gap:
/// - Packets sent with the old key during grace period are accepted
/// - Packets sent with the new key after switchover are accepted
/// - Total received count is monotonically increasing throughout
#[tokio::test]
async fn test_srtp_rekey_during_active_media_flow() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;

    client.request_ok("session.create", json!({})).await;

    // Create two SRTP endpoints
    let result_a = client
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv", "srtp": true}),
        )
        .await;
    let ep_a_id = result_a["endpoint_id"].as_str().unwrap().to_string();
    let offer_a = result_a["sdp_offer"].as_str().unwrap();
    let bridge_key_a = parse_crypto_key_from_sdp(offer_a).expect("parse key A");
    let server_addr_a = parse_rtp_addr_from_sdp(offer_a).expect("parse addr A");

    let result_b = client
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv", "srtp": true}),
        )
        .await;
    let ep_b_id = result_b["endpoint_id"].as_str().unwrap().to_string();
    let offer_b = result_b["sdp_offer"].as_str().unwrap();
    let bridge_key_b = parse_crypto_key_from_sdp(offer_b).expect("parse key B");
    let server_addr_b = parse_rtp_addr_from_sdp(offer_b).expect("parse addr B");

    let mut peer_a = TestRtpPeer::new().await;
    let mut peer_b = TestRtpPeer::new().await;

    let answer_a = make_srtp_answer(&peer_a, &bridge_key_a);
    client
        .request_ok(
            "endpoint.accept_answer",
            json!({"endpoint_id": ep_a_id, "sdp": answer_a}),
        )
        .await;
    let answer_b = make_srtp_answer(&peer_b, &bridge_key_b);
    client
        .request_ok(
            "endpoint.accept_answer",
            json!({"endpoint_id": ep_b_id, "sdp": answer_b}),
        )
        .await;

    peer_a.set_remote(server_addr_a);
    peer_b.set_remote(server_addr_b);
    // Activate symmetric RTP with an SRTP-encrypted packet
    {
        let mut ctx_b_tx =
            SrtpContext::from_sdes_key(&bridge_key_b).expect("SRTP ctx for peer B activation");
        let rtp = build_rtp_packet(0, 0, 0, 0xAABBCCDD, false, &vec![0xFFu8; 160]);
        let srtp = ctx_b_tx.protect(&rtp).expect("SRTP protect");
        peer_b.socket.send_to(&srtp, server_addr_b).await.unwrap();
    }
    tokio::time::sleep(timing::scaled_ms(50)).await;
    peer_b.start_recv();

    let mut ctx_a = SrtpContext::from_sdes_key(&bridge_key_a).unwrap();
    let mut seq: u16 = 0;

    // Phase 1: send 20 packets with original key (steady flow)
    for _ in 0..20 {
        let rtp = build_rtp_packet(0, seq, seq as u32 * 160, 0xAAAA0001, seq == 0, &[0x80; 160]);
        let srtp = ctx_a.protect(&rtp).unwrap();
        peer_a.socket.send_to(&srtp, server_addr_a).await.unwrap();
        seq += 1;
        tokio::time::sleep(timing::PACING).await;
    }
    tokio::time::sleep(timing::scaled_ms(200)).await;
    let before_rekey = peer_b.received_count();
    assert!(before_rekey > 0, "should receive packets before rekey");

    // Phase 2: rekey mid-flow, then immediately send more with OLD key
    let rekey_result = client
        .request_ok("endpoint.srtp_rekey", json!({"endpoint_id": ep_a_id}))
        .await;
    let new_sdp = rekey_result["sdp"].as_str().unwrap();
    let new_key = parse_crypto_key_from_sdp(new_sdp).expect("parse new key");
    assert_ne!(new_key, bridge_key_a, "rekey should produce different key");

    // Continue sending with OLD key immediately after rekey (RX unchanged, still works)
    for _ in 0..10 {
        let rtp = build_rtp_packet(0, seq, seq as u32 * 160, 0xAAAA0001, false, &[0x80; 160]);
        let srtp = ctx_a.protect(&rtp).unwrap();
        peer_a.socket.send_to(&srtp, server_addr_a).await.unwrap();
        seq += 1;
        tokio::time::sleep(timing::PACING).await;
    }
    tokio::time::sleep(timing::scaled_ms(200)).await;
    let during_grace = peer_b.received_count();
    assert!(
        during_grace > before_rekey,
        "old key should still work after rekey: before={before_rekey}, during={during_grace}"
    );

    // Simulate peer's answer with new key — triggers RX dual-context transition
    let answer_rekey = make_srtp_answer(&peer_a, &new_key);
    client
        .request_ok(
            "endpoint.accept_answer",
            json!({"endpoint_id": ep_a_id, "sdp": answer_rekey}),
        )
        .await;

    // Phase 3: switch to new key seamlessly (no pause)
    let mut ctx_new = SrtpContext::from_sdes_key(&new_key).unwrap();
    for _ in 0..20 {
        let rtp = build_rtp_packet(0, seq, seq as u32 * 160, 0xAAAA0001, false, &[0x80; 160]);
        let srtp = ctx_new.protect(&rtp).unwrap();
        peer_a.socket.send_to(&srtp, server_addr_a).await.unwrap();
        seq += 1;
        tokio::time::sleep(timing::PACING).await;
    }
    tokio::time::sleep(timing::scaled_ms(300)).await;
    let after_new_key = peer_b.received_count();
    assert!(
        after_new_key > during_grace,
        "new key should work after accept_answer: during_grace={during_grace}, after={after_new_key}"
    );

    // Verify monotonic increase — no gap in delivery
    assert!(
        before_rekey < during_grace && during_grace < after_new_key,
        "packet counts should be monotonically increasing across rekey: {before_rekey} < {during_grace} < {after_new_key}"
    );

    client.request_ok("session.destroy", json!({})).await;
}

/// Test: calling endpoint.srtp_rekey on a plain RTP endpoint (no crypto) should fail.
#[tokio::test]
async fn test_srtp_rekey_on_non_srtp_endpoint() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;

    let peer = TestRtpPeer::new().await;
    let offer = peer.make_sdp_offer(); // plain RTP, no crypto
    let result = client
        .request_ok(
            "endpoint.create_from_offer",
            json!({"sdp": offer, "direction": "sendrecv"}),
        )
        .await;
    let ep_id = result["endpoint_id"].as_str().unwrap();

    let resp = client
        .request("endpoint.srtp_rekey", json!({"endpoint_id": ep_id}))
        .await;
    assert!(
        resp.get("error").is_some(),
        "rekey on non-SRTP endpoint should fail: {resp}"
    );

    client.request_ok("session.destroy", json!({})).await;
}

/// Test: calling endpoint.srtp_rekey on a nonexistent endpoint should fail.
#[tokio::test]
async fn test_srtp_rekey_on_nonexistent_endpoint() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;

    let resp = client
        .request(
            "endpoint.srtp_rekey",
            json!({"endpoint_id": "00000000-0000-0000-0000-000000000000"}),
        )
        .await;
    assert!(
        resp.get("error").is_some(),
        "rekey on nonexistent endpoint should fail: {resp}"
    );

    client.request_ok("session.destroy", json!({})).await;
}

/// Test: calling endpoint.srtp_rekey on a plain RTP endpoint (no SRTP) returns ENDPOINT_ERROR.
/// This specifically verifies the error code, complementing the existing non-SRTP test.
#[tokio::test]
async fn test_srtp_rekey_on_plain_rtp() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;

    // Create a plain RTP endpoint (no srtp flag)
    let peer = TestRtpPeer::new().await;
    let offer = peer.make_sdp_offer();
    let result = client
        .request_ok(
            "endpoint.create_from_offer",
            json!({"sdp": offer, "direction": "sendrecv"}),
        )
        .await;
    let ep_id = result["endpoint_id"].as_str().unwrap();

    // Verify the SDP does NOT contain crypto lines (plain RTP)
    let sdp = result["sdp_answer"].as_str().unwrap();
    assert!(
        !sdp.contains("a=crypto:"),
        "plain RTP endpoint should not have crypto lines"
    );

    // Try SRTP rekey — should fail with ENDPOINT_ERROR
    let resp = client
        .request("endpoint.srtp_rekey", json!({"endpoint_id": ep_id}))
        .await;
    assert!(
        resp.get("error").is_some(),
        "srtp_rekey on plain RTP should return error: {resp}"
    );
    assert_eq!(
        resp["error"]["code"].as_str().unwrap_or(""),
        "ENDPOINT_ERROR",
        "srtp_rekey on plain RTP should return ENDPOINT_ERROR: {resp}"
    );

    client.request_ok("session.destroy", json!({})).await;
}

/// Test: calling endpoint.srtp_rekey with a random UUID returns ENDPOINT_ERROR.
/// Verifies the error code for the nonexistent endpoint case.
#[tokio::test]
async fn test_srtp_rekey_on_nonexistent_endpoint_error_code() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;

    let resp = client
        .request(
            "endpoint.srtp_rekey",
            json!({"endpoint_id": "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"}),
        )
        .await;
    assert!(
        resp.get("error").is_some(),
        "srtp_rekey on nonexistent endpoint should return error: {resp}"
    );
    assert_eq!(
        resp["error"]["code"].as_str().unwrap_or(""),
        "ENDPOINT_ERROR",
        "srtp_rekey on nonexistent endpoint should return ENDPOINT_ERROR: {resp}"
    );

    client.request_ok("session.destroy", json!({})).await;
}
