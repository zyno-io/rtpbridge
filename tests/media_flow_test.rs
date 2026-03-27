mod helpers;

use serde_json::json;
use tempfile::TempDir;

use helpers::control_client::TestControlClient;
use helpers::test_rtp_peer::{TestRtpPeer, parse_rtp_addr_from_sdp};
use helpers::test_server::TestServer;
use helpers::timing;
use helpers::wav::generate_test_wav;

/// Helper: create an RTP endpoint via create_from_offer and return (endpoint_id, peer).
async fn setup_rtp_endpoint(
    client: &mut TestControlClient,
    direction: &str,
) -> (String, TestRtpPeer) {
    let mut peer = TestRtpPeer::new().await;
    let offer = peer.make_sdp_offer();
    let result = client
        .request_ok(
            "endpoint.create_from_offer",
            json!({"sdp": offer, "direction": direction}),
        )
        .await;
    let ep_id = result["endpoint_id"].as_str().unwrap().to_string();
    let answer = result["sdp_answer"].as_str().unwrap();
    let server_addr = parse_rtp_addr_from_sdp(answer).expect("parse server addr");
    peer.set_remote(server_addr);
    (ep_id, peer)
}

/// Test: Send RTP from peer A → bridge endpoint A → bridge routes → endpoint B → peer B receives.
/// This is the core "real media flow" test: actual audio bytes traverse the full pipeline.
#[tokio::test]
async fn test_rtp_to_rtp_media_flow() {
    let tmp = TempDir::new().unwrap();
    let tmp_str = tmp.path().to_str().unwrap();
    let server = TestServer::builder()
        .media_dir(tmp_str)
        .recording_dir(tmp_str)
        .start()
        .await;
    let mut client = TestControlClient::connect(&server.addr).await;

    client.request_ok("session.create", json!({})).await;

    // Create two test peers (simulating external RTP devices)
    let mut peer_a = TestRtpPeer::new().await;
    let mut peer_b = TestRtpPeer::new().await;

    // --- Endpoint A: accept peer A's offer ---
    let offer_a = peer_a.make_sdp_offer();
    let result = client
        .request_ok(
            "endpoint.create_from_offer",
            json!({"sdp": offer_a, "direction": "sendrecv"}),
        )
        .await;
    let _ep_a_id = result["endpoint_id"].as_str().unwrap().to_string();
    let answer_a = result["sdp_answer"].as_str().unwrap();
    // Parse server's answer to get the server-side RTP port for endpoint A
    let server_addr_a = parse_rtp_addr_from_sdp(answer_a).expect("parse server addr A");
    peer_a.set_remote(server_addr_a);

    // --- Endpoint B: server creates offer, peer B answers ---
    let result = client
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv"}),
        )
        .await;
    let ep_b_id = result["endpoint_id"].as_str().unwrap().to_string();
    let offer_b = result["sdp_offer"].as_str().unwrap();
    let server_addr_b = parse_rtp_addr_from_sdp(offer_b).expect("parse server addr B");
    // Give server peer B's address via accept_answer
    let answer_b = peer_b.make_sdp_answer();
    client
        .request_ok(
            "endpoint.accept_answer",
            json!({"endpoint_id": ep_b_id, "sdp": answer_b}),
        )
        .await;
    peer_b.set_remote(server_addr_b);

    // Activate symmetric RTP on peer B's server-side endpoint
    peer_b.activate().await;
    tokio::time::sleep(timing::scaled_ms(50)).await;

    // Start receiving on peer B
    peer_b.start_recv();

    // Small delay for recv task to be ready
    tokio::time::sleep(timing::scaled_ms(50)).await;

    // Send a burst of RTP packets from peer A → server
    peer_a.send_tone_for(timing::scaled_ms(200)).await;

    // Wait for packets to traverse the bridge
    tokio::time::sleep(timing::scaled_ms(500)).await;

    // Peer B should have received routed packets
    let received = peer_b.received_count();
    assert!(
        received > 0,
        "peer B should have received RTP packets from the bridge, got {received}"
    );

    // Validate received payload content — should be valid PCMU audio, not zeros/corruption
    let payload = peer_b.last_payload().await;
    assert!(
        payload.len() >= 160,
        "received RTP payload should be at least 160 bytes (one PCMU frame), got {}",
        payload.len()
    );
    // Mu-law silence is 0xFF — a routed tone should contain non-silence samples
    let non_silence = payload.iter().filter(|&&b| b != 0xFF).count();
    assert!(
        non_silence > 0,
        "received payload should contain non-silence audio data"
    );
    // Verify payload has byte variety (actual audio, not constant/corrupted)
    let unique_bytes: std::collections::HashSet<u8> = payload.iter().copied().collect();
    assert!(
        unique_bytes.len() > 1,
        "received payload should have varied byte values (audio tone, not constant fill)"
    );

    // Phase 2: Send a known distinguishable payload pattern and verify byte-for-byte integrity.
    // Both endpoints negotiate PCMU so no transcoding occurs — payload traverses unchanged.
    let known_pattern: Vec<u8> = (0..160)
        .map(|i| if i % 2 == 0 { 0x42 } else { 0xBD })
        .collect();
    for _ in 0..5 {
        peer_a.send_pcmu(&known_pattern).await;
        tokio::time::sleep(timing::PACING).await;
    }
    tokio::time::sleep(timing::scaled_ms(300)).await;

    let received_payload = peer_b.last_payload().await;
    assert_eq!(
        received_payload.len(),
        160,
        "received RTP payload should be exactly 160 bytes, got {}",
        received_payload.len()
    );
    assert_eq!(
        &received_payload[..],
        &known_pattern[..],
        "received payload should match the known pattern sent from peer A (no transcode path)"
    );

    // Verify session.info shows stats
    let info = client.request_ok("session.info", json!({})).await;
    let endpoints = info["endpoints"].as_array().unwrap();
    assert_eq!(endpoints.len(), 2, "should have 2 endpoints");

    client.request_ok("session.destroy", json!({})).await;
}

/// Test: File playback endpoint produces audio that reaches an RTP endpoint's connected peer.
#[tokio::test]
async fn test_file_playback_to_rtp_endpoint() {
    let tmp = TempDir::new().unwrap();
    let tmp_str = tmp.path().to_str().unwrap();
    let server = TestServer::builder()
        .media_dir(tmp_str)
        .recording_dir(tmp_str)
        .start()
        .await;
    let mut client = TestControlClient::connect(&server.addr).await;

    // Generate a test WAV file
    let wav_path = tmp.path().join("media-flow-test.wav");
    generate_test_wav(&wav_path, 1.0, 440.0);

    client.request_ok("session.create", json!({})).await;

    // Create an RTP endpoint with a connected peer
    let mut peer = TestRtpPeer::new().await;
    let result = client
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv"}),
        )
        .await;
    let ep_id = result["endpoint_id"].as_str().unwrap().to_string();
    let offer = result["sdp_offer"].as_str().unwrap();
    let server_addr = parse_rtp_addr_from_sdp(offer).expect("parse server addr");
    let answer = peer.make_sdp_answer();
    client
        .request_ok(
            "endpoint.accept_answer",
            json!({"endpoint_id": ep_id, "sdp": answer}),
        )
        .await;
    peer.set_remote(server_addr);

    // Activate symmetric RTP on the peer's server-side endpoint
    peer.activate().await;
    tokio::time::sleep(timing::scaled_ms(50)).await;

    peer.start_recv();

    // Create file playback endpoint (sends audio into the session)
    let result = client
        .request_ok(
            "endpoint.create_with_file",
            json!({"source": wav_path.to_str().unwrap(), "shared": false, "loop_count": 0}),
        )
        .await;
    assert!(!result["endpoint_id"].as_str().unwrap().is_empty());

    // Wait for file audio to be routed to the RTP endpoint
    tokio::time::sleep(timing::scaled_ms(1500)).await;

    let received = peer.received_count();
    assert!(
        received > 0,
        "peer should have received audio from file playback, got {received}"
    );

    // Validate received payload — file playback of a 440Hz tone should produce
    // non-trivial PCMU audio, not silence or zeros
    let payload = peer.last_payload().await;
    assert!(
        payload.len() >= 160,
        "file playback payload should be at least 160 bytes (one PCMU frame), got {}",
        payload.len()
    );
    let non_silence = payload.iter().filter(|&&b| b != 0xFF).count();
    assert!(
        non_silence > 0,
        "file playback payload should contain non-silence audio data"
    );
    let unique_bytes: std::collections::HashSet<u8> = payload.iter().copied().collect();
    assert!(
        unique_bytes.len() > 1,
        "file playback payload should have varied byte values (tone, not constant fill)"
    );

    client.request_ok("session.destroy", json!({})).await;
}

/// Test: SRTP endpoint creation with crypto exchange.
/// Verifies the full SRTP SDP negotiation including OSRTP (RTP/AVP + crypto).
#[tokio::test]
async fn test_srtp_endpoint_crypto_exchange() {
    let tmp = TempDir::new().unwrap();
    let tmp_str = tmp.path().to_str().unwrap();
    let server = TestServer::builder()
        .media_dir(tmp_str)
        .recording_dir(tmp_str)
        .start()
        .await;
    let mut client = TestControlClient::connect(&server.addr).await;

    client.request_ok("session.create", json!({})).await;

    // Create an SRTP endpoint (server creates offer with crypto)
    let result = client
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv", "srtp": true}),
        )
        .await;
    let ep_id = result["endpoint_id"].as_str().unwrap().to_string();
    let offer = result["sdp_offer"].as_str().unwrap();

    // Verify SRTP SDP has required fields
    assert!(
        offer.contains("RTP/SAVP"),
        "SRTP offer should use RTP/SAVP profile"
    );
    assert!(
        offer.contains("a=crypto:"),
        "SRTP offer should have crypto line"
    );

    // Extract crypto line
    let crypto_line = offer
        .lines()
        .find(|l| l.trim().starts_with("a=crypto:"))
        .expect("crypto line missing");
    assert!(
        crypto_line.contains("AES_CM_128_HMAC_SHA1_80"),
        "should use AES_CM_128_HMAC_SHA1_80 suite"
    );
    assert!(crypto_line.contains("inline:"), "should have inline key");

    // Accept answer with matching crypto (OSRTP-style: RTP/AVP + crypto)
    let peer = TestRtpPeer::new().await;
    let answer = format!(
        "v=0\r\n\
         o=- 300 1 IN IP4 {ip}\r\n\
         s=-\r\n\
         c=IN IP4 {ip}\r\n\
         t=0 0\r\n\
         m=audio {port} RTP/SAVP 0 101\r\n\
         a=rtpmap:0 PCMU/8000\r\n\
         a=rtpmap:101 telephone-event/8000\r\n\
         a=fmtp:101 0-16\r\n\
         {crypto}\r\n\
         a=sendrecv\r\n",
        ip = peer.local_addr.ip(),
        port = peer.local_addr.port(),
        crypto = crypto_line.trim(),
    );

    let resp = client
        .request(
            "endpoint.accept_answer",
            json!({"endpoint_id": ep_id, "sdp": answer}),
        )
        .await;
    assert!(
        resp.get("result").is_some(),
        "accept_answer with SRTP should succeed: {resp}"
    );

    // Also test OSRTP detection: create_from_offer with RTP/AVP + crypto
    let osrtp_offer = format!(
        "v=0\r\n\
         o=- 400 1 IN IP4 127.0.0.1\r\n\
         s=-\r\n\
         c=IN IP4 127.0.0.1\r\n\
         t=0 0\r\n\
         m=audio 55000 RTP/AVP 0 101\r\n\
         a=rtpmap:0 PCMU/8000\r\n\
         a=rtpmap:101 telephone-event/8000\r\n\
         a=fmtp:101 0-16\r\n\
         a=crypto:1 AES_CM_128_HMAC_SHA1_80 inline:YTEyMzQ1Njc4OTAxMjM0NTY3ODkwMTIzNDU2Nzg5\r\n\
         a=sendrecv\r\n"
    );
    let result = client
        .request_ok(
            "endpoint.create_from_offer",
            json!({"sdp": osrtp_offer, "direction": "sendrecv"}),
        )
        .await;
    let answer2 = result["sdp_answer"].as_str().unwrap();
    assert!(
        answer2.contains("a=crypto:"),
        "OSRTP answer should echo crypto: {answer2}"
    );

    client.request_ok("session.destroy", json!({})).await;
}

/// Test: Recording produces a valid PCAP file with correct packet structure.
/// Verifies PCAP file header, packet count, and Ethernet→IPv4→UDP→RTP structure.
#[tokio::test]
async fn test_recording_pcap_integrity() {
    let tmp = TempDir::new().unwrap();
    let tmp_str = tmp.path().to_str().unwrap();
    let server = TestServer::builder()
        .media_dir(tmp_str)
        .recording_dir(tmp_str)
        .start()
        .await;
    let mut client = TestControlClient::connect(&server.addr).await;

    client.request_ok("session.create", json!({})).await;

    // Set up two RTP endpoints
    let mut peer_a = TestRtpPeer::new().await;
    let mut peer_b = TestRtpPeer::new().await;

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

    let offer_b = peer_b.make_sdp_offer();
    let result = client
        .request_ok(
            "endpoint.create_from_offer",
            json!({"sdp": offer_b, "direction": "sendrecv"}),
        )
        .await;
    let answer_b = result["sdp_answer"].as_str().unwrap();
    let server_addr_b = parse_rtp_addr_from_sdp(answer_b).expect("parse addr B");
    peer_b.set_remote(server_addr_b);

    // Start recording (session-wide)
    let pcap_path = std::path::Path::new(&server.recording_dir).join("pcap-integrity-test.pcap");
    let rec_result = client
        .request_ok(
            "recording.start",
            json!({"endpoint_id": null, "file_path": pcap_path.to_str().unwrap()}),
        )
        .await;
    let rec_id = rec_result["recording_id"].as_str().unwrap().to_string();

    // Send 20 RTP packets from peer A
    for _ in 0..20 {
        let payload = vec![0x80u8; 160];
        peer_a.send_pcmu(&payload).await;
        tokio::time::sleep(timing::PACING).await;
    }

    tokio::time::sleep(timing::scaled_ms(500)).await;

    // Stop recording
    let stop_result = client
        .request_ok("recording.stop", json!({"recording_id": rec_id}))
        .await;
    assert!(
        stop_result["packets"].as_u64().unwrap() > 0,
        "should have recorded packets"
    );

    tokio::time::sleep(timing::scaled_ms(500)).await;

    // Parse the PCAP file
    let file = std::fs::File::open(&pcap_path).expect("PCAP file should exist");
    let mut reader = pcap_file::pcap::PcapReader::new(file).expect("valid PCAP file");

    let header = reader.header();
    assert_eq!(
        header.datalink,
        pcap_file::DataLink::ETHERNET,
        "link type should be Ethernet"
    );

    let mut packet_count = 0u32;
    while let Some(pkt) = reader.next_packet() {
        let pkt = pkt.expect("valid PCAP packet");
        let data = &pkt.data;

        assert!(data.len() >= 42, "packet too small for Eth+IP+UDP");
        assert_eq!(&data[12..14], &[0x08, 0x00], "EtherType should be IPv4");
        assert_eq!(data[14] & 0xF0, 0x40, "IPv4 version should be 4");
        assert_eq!(data[23], 17, "protocol should be UDP");

        let payload_offset = 14 + 20 + 8;
        if data.len() > payload_offset {
            assert_eq!(data[payload_offset] & 0xC0, 0x80, "RTP version should be 2");
        }

        packet_count += 1;
    }

    assert!(
        packet_count >= 20,
        "should have at least 20 recorded packets, got {packet_count}"
    );

    client.request_ok("session.destroy", json!({})).await;
}

/// Test: PCAP recording includes RTCP packets (sent by the bridge every ~5 seconds).
/// We send RTP for 7 seconds to ensure at least one RTCP SR is generated.
#[tokio::test]
async fn test_recording_pcap_contains_rtcp() {
    let tmp = TempDir::new().unwrap();
    let tmp_str = tmp.path().to_str().unwrap();
    let server = TestServer::builder()
        .media_dir(tmp_str)
        .recording_dir(tmp_str)
        .start()
        .await;
    let mut client = TestControlClient::connect(&server.addr).await;

    client.request_ok("session.create", json!({})).await;

    let mut peer_a = TestRtpPeer::new().await;
    let mut peer_b = TestRtpPeer::new().await;

    let offer_a = peer_a.make_sdp_offer();
    let result = client
        .request_ok(
            "endpoint.create_from_offer",
            json!({"sdp": offer_a, "direction": "sendrecv"}),
        )
        .await;
    peer_a.set_remote(parse_rtp_addr_from_sdp(result["sdp_answer"].as_str().unwrap()).unwrap());

    let offer_b = peer_b.make_sdp_offer();
    let result = client
        .request_ok(
            "endpoint.create_from_offer",
            json!({"sdp": offer_b, "direction": "sendrecv"}),
        )
        .await;
    peer_b.set_remote(parse_rtp_addr_from_sdp(result["sdp_answer"].as_str().unwrap()).unwrap());

    // Start session-wide recording
    let pcap_path = std::path::Path::new(&server.recording_dir).join("pcap-rtcp-test.pcap");
    let rec = client
        .request_ok(
            "recording.start",
            json!({"endpoint_id": null, "file_path": pcap_path.to_str().unwrap()}),
        )
        .await;
    let rec_id = rec["recording_id"].as_str().unwrap().to_string();

    // Send RTP for 7 seconds (RTCP interval is ~5s) at 50pps
    peer_a.send_tone_for(timing::scaled_ms(7000)).await;

    tokio::time::sleep(timing::scaled_ms(1000)).await;

    // Stop recording
    client
        .request_ok("recording.stop", json!({"recording_id": rec_id}))
        .await;

    tokio::time::sleep(timing::scaled_ms(500)).await;

    // Parse PCAP and look for RTCP packets
    let file = std::fs::File::open(&pcap_path).expect("PCAP file should exist");
    let mut reader = pcap_file::pcap::PcapReader::new(file).expect("valid PCAP file");

    let mut rtp_count = 0u32;
    let mut rtcp_count = 0u32;
    let payload_offset = 14 + 20 + 8; // Eth + IPv4 + UDP

    while let Some(pkt) = reader.next_packet() {
        let pkt = pkt.expect("valid PCAP packet");
        let data = &pkt.data;

        if data.len() <= payload_offset {
            continue;
        }

        let udp_payload = &data[payload_offset..];
        if udp_payload.len() < 2 {
            continue;
        }

        // RTCP: version=2 (top 2 bits = 10), PT in byte 1 is 200 (SR) or 201 (RR)
        let version = (udp_payload[0] >> 6) & 0x03;
        let pt = udp_payload[1];

        if version == 2 && (pt == 200 || pt == 201) {
            rtcp_count += 1;
        } else if version == 2 {
            rtp_count += 1;
        }
    }

    assert!(rtp_count > 0, "should have RTP packets in PCAP");
    assert!(
        rtcp_count > 0,
        "should have at least one RTCP packet in PCAP (got {rtcp_count} RTCP, {rtp_count} RTP)"
    );

    client.request_ok("session.destroy", json!({})).await;
}

/// Test: PCAP recording has monotonic timestamps and correct payload content
#[tokio::test]
async fn test_recording_pcap_timestamps_and_payload() {
    let tmp = TempDir::new().unwrap();
    let tmp_str = tmp.path().to_str().unwrap();
    let server = TestServer::builder()
        .media_dir(tmp_str)
        .recording_dir(tmp_str)
        .start()
        .await;
    let mut client = TestControlClient::connect(&server.addr).await;

    client.request_ok("session.create", json!({})).await;

    let mut peer_a = TestRtpPeer::new().await;
    let mut peer_b = TestRtpPeer::new().await;

    let offer_a = peer_a.make_sdp_offer();
    let result = client
        .request_ok(
            "endpoint.create_from_offer",
            json!({"sdp": offer_a, "direction": "sendrecv"}),
        )
        .await;
    peer_a.set_remote(parse_rtp_addr_from_sdp(result["sdp_answer"].as_str().unwrap()).unwrap());

    let offer_b = peer_b.make_sdp_offer();
    let result = client
        .request_ok(
            "endpoint.create_from_offer",
            json!({"sdp": offer_b, "direction": "sendrecv"}),
        )
        .await;
    peer_b.set_remote(parse_rtp_addr_from_sdp(result["sdp_answer"].as_str().unwrap()).unwrap());

    let pcap_path = std::path::Path::new(&server.recording_dir).join("pcap-ts-test.pcap");
    let rec = client
        .request_ok(
            "recording.start",
            json!({"endpoint_id": null, "file_path": pcap_path.to_str().unwrap()}),
        )
        .await;
    let rec_id = rec["recording_id"].as_str().unwrap().to_string();

    // Send 20 packets with a recognizable payload pattern (0xAA repeated)
    let pattern_payload = vec![0xAAu8; 160];
    for _ in 0..20 {
        peer_a.send_pcmu(&pattern_payload).await;
        tokio::time::sleep(timing::PACING).await;
    }

    tokio::time::sleep(timing::scaled_ms(500)).await;
    client
        .request_ok("recording.stop", json!({"recording_id": rec_id}))
        .await;
    tokio::time::sleep(timing::scaled_ms(500)).await;

    // Parse PCAP and verify timestamps and payloads
    let file = std::fs::File::open(&pcap_path).expect("PCAP file should exist");
    let mut reader = pcap_file::pcap::PcapReader::new(file).expect("valid PCAP file");

    let mut last_ts_us: u64 = 0;
    let mut found_pattern = false;
    let payload_offset = 14 + 20 + 8; // Eth + IPv4 + UDP
    let rtp_header_len = 12;

    while let Some(pkt) = reader.next_packet() {
        let pkt = pkt.expect("valid PCAP packet");

        // Verify monotonic timestamps
        let ts_us = pkt.timestamp.as_micros() as u64;
        assert!(
            ts_us >= last_ts_us,
            "PCAP timestamps should be monotonic: {ts_us} < {last_ts_us}"
        );
        last_ts_us = ts_us;

        // Check if this packet contains our pattern payload
        let data = &pkt.data;
        if data.len() > payload_offset + rtp_header_len + 10 {
            let rtp_payload = &data[payload_offset + rtp_header_len..];
            if rtp_payload.len() >= 160 && rtp_payload[..10].iter().all(|&b| b == 0xAA) {
                found_pattern = true;
            }
        }
    }

    assert!(
        found_pattern,
        "PCAP should contain packets with our 0xAA payload pattern"
    );

    client.request_ok("session.destroy", json!({})).await;
}

/// Test: 3 sendrecv endpoints — A sends, both B and C receive. B sends, both A and C receive.
#[tokio::test]
async fn test_star_routing_three_sendrecv_endpoints() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;

    let (_, mut peer_a) = setup_rtp_endpoint(&mut client, "sendrecv").await;
    let (_, mut peer_b) = setup_rtp_endpoint(&mut client, "sendrecv").await;
    let (_, mut peer_c) = setup_rtp_endpoint(&mut client, "sendrecv").await;

    // Activate symmetric RTP on peers that will receive
    peer_b.activate().await;
    peer_c.activate().await;
    tokio::time::sleep(timing::scaled_ms(50)).await;

    // Phase 1: A sends → B and C both receive
    peer_b.start_recv();
    peer_c.start_recv();
    tokio::time::sleep(timing::scaled_ms(50)).await;

    peer_a.send_tone_for(timing::scaled_ms(300)).await;
    tokio::time::sleep(timing::scaled_ms(500)).await;

    let b_received = peer_b.received_count();
    let c_received = peer_c.received_count();
    assert!(
        b_received > 0,
        "peer B should receive from A in 3-endpoint star, got {b_received}"
    );
    assert!(
        c_received > 0,
        "peer C should receive from A in 3-endpoint star, got {c_received}"
    );

    // Phase 2: B sends → A and C receive
    peer_a.start_recv();
    tokio::time::sleep(timing::scaled_ms(50)).await;
    let c_before = peer_c.received_count();

    peer_b.send_tone_for(timing::scaled_ms(300)).await;
    tokio::time::sleep(timing::scaled_ms(500)).await;

    let a_received = peer_a.received_count();
    let c_after = peer_c.received_count();
    assert!(
        a_received > 0,
        "peer A should receive from B, got {a_received}"
    );
    assert!(
        c_after > c_before,
        "peer C should receive from B: before={c_before}, after={c_after}"
    );

    // Verify 3 endpoints in session
    let info = client.request_ok("session.info", json!({})).await;
    assert_eq!(info["endpoints"].as_array().unwrap().len(), 3);

    client.request_ok("session.destroy", json!({})).await;
}

/// Test: File playback (sendonly) routes to 3 RTP endpoints simultaneously
#[tokio::test]
async fn test_star_routing_file_to_three_rtp_endpoints() {
    let tmp = TempDir::new().unwrap();
    let tmp_str = tmp.path().to_str().unwrap();
    let server = TestServer::builder()
        .media_dir(tmp_str)
        .recording_dir(tmp_str)
        .start()
        .await;
    let mut client = TestControlClient::connect(&server.addr).await;

    let wav_path = tmp.path().join("star-file-test.wav");
    generate_test_wav(&wav_path, 1.0, 440.0);

    client.request_ok("session.create", json!({})).await;

    let (_, mut peer_a) = setup_rtp_endpoint(&mut client, "sendrecv").await;
    let (_, mut peer_b) = setup_rtp_endpoint(&mut client, "sendrecv").await;
    let (_, mut peer_c) = setup_rtp_endpoint(&mut client, "sendrecv").await;

    // Activate symmetric RTP on all peers that will receive file playback
    peer_a.activate().await;
    peer_b.activate().await;
    peer_c.activate().await;
    tokio::time::sleep(timing::scaled_ms(50)).await;

    peer_a.start_recv();
    peer_b.start_recv();
    peer_c.start_recv();

    // File playback → all 3 RTP peers should receive
    client
        .request_ok(
            "endpoint.create_with_file",
            json!({"source": wav_path.to_str().unwrap(), "shared": false, "loop_count": 0}),
        )
        .await;

    tokio::time::sleep(timing::scaled_ms(1500)).await;

    assert!(
        peer_a.received_count() > 0,
        "peer A should receive from file"
    );
    assert!(
        peer_b.received_count() > 0,
        "peer B should receive from file"
    );
    assert!(
        peer_c.received_count() > 0,
        "peer C should receive from file"
    );

    client.request_ok("session.destroy", json!({})).await;
}

/// Test: Removing an endpoint during active media flow doesn't break remaining routing
#[tokio::test]
async fn test_endpoint_removal_during_active_routing() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;

    let (_, mut peer_a) = setup_rtp_endpoint(&mut client, "sendrecv").await;
    let (_, mut peer_b) = setup_rtp_endpoint(&mut client, "sendrecv").await;
    let (ep_c_id, mut peer_c) = setup_rtp_endpoint(&mut client, "sendrecv").await;

    // Activate symmetric RTP on peers that will receive
    peer_b.activate().await;
    peer_c.activate().await;
    tokio::time::sleep(timing::scaled_ms(50)).await;

    peer_b.start_recv();
    peer_c.start_recv();
    tokio::time::sleep(timing::scaled_ms(50)).await;

    // Send from A → B and C receive
    peer_a.send_tone_for(timing::scaled_ms(200)).await;
    tokio::time::sleep(timing::scaled_ms(300)).await;

    assert!(
        peer_b.received_count() > 0,
        "B should receive before removal"
    );
    assert!(
        peer_c.received_count() > 0,
        "C should receive before removal"
    );

    // Remove endpoint C while routing is active
    client
        .request_ok("endpoint.remove", json!({"endpoint_id": ep_c_id}))
        .await;
    tokio::time::sleep(timing::scaled_ms(200)).await;

    // Capture C's count after removal settles
    let c_after_remove = peer_c.received_count();

    // Verify C is gone from session
    let info = client.request_ok("session.info", json!({})).await;
    let endpoints = info["endpoints"].as_array().unwrap();
    assert_eq!(endpoints.len(), 2, "should have 2 endpoints after removal");
    let ids: Vec<&str> = endpoints
        .iter()
        .map(|e| e["endpoint_id"].as_str().unwrap())
        .collect();
    assert!(!ids.contains(&ep_c_id.as_str()));

    // Send more from A → B should still receive, C should NOT
    let b_before = peer_b.received_count();
    peer_a.send_tone_for(timing::scaled_ms(200)).await;
    tokio::time::sleep(timing::scaled_ms(300)).await;

    assert!(
        peer_b.received_count() > b_before,
        "B should still receive after C removal"
    );
    let c_final = peer_c.received_count();
    assert!(
        c_final - c_after_remove <= 2,
        "C should not receive new packets after removal, delta={}",
        c_final - c_after_remove
    );

    client.request_ok("session.destroy", json!({})).await;
}
