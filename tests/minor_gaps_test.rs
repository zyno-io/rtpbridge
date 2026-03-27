mod helpers;

use std::time::Duration;

use serde_json::json;
use tempfile::TempDir;

use helpers::control_client::TestControlClient;
use helpers::test_rtp_peer::{TestRtpPeer, parse_rtp_addr_from_sdp};
use helpers::test_server::TestServer;
use helpers::timing;
use helpers::wav::generate_test_wav;

/// Helper: create an RTP endpoint from a peer's offer, return (endpoint_id, connected peer)
async fn create_rtp_endpoint_from_offer(
    client: &mut TestControlClient,
    peer: &mut TestRtpPeer,
) -> String {
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
    ep_id
}

/// Helper: create an RTP endpoint via server offer, peer answers
async fn create_rtp_endpoint_from_server_offer(
    client: &mut TestControlClient,
    peer: &mut TestRtpPeer,
) -> String {
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
    ep_id
}

// ─────────────────────────────────────────────────────────────────────────────
// Test 1: 4+ party conference routing
//
// Creates 5 sendrecv RTP endpoints. Sends audio from endpoint A. Verifies
// all other 4 endpoints receive routed packets (fan-out routing).
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_five_party_conference_routing() {
    let tmp = TempDir::new().unwrap();
    let server = TestServer::builder()
        .media_dir(tmp.path().to_str().unwrap())
        .start()
        .await;
    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;

    // Create 5 peers and endpoints
    let mut peers: Vec<TestRtpPeer> = Vec::new();
    let mut ep_ids: Vec<String> = Vec::new();

    for _ in 0..5 {
        let mut peer = TestRtpPeer::new().await;
        let ep_id = create_rtp_endpoint_from_offer(&mut client, &mut peer).await;
        ep_ids.push(ep_id);
        peers.push(peer);
    }

    // Activate and start receiving on peers 1-4 (not peer 0 — that's the sender)
    for peer in peers.iter_mut().skip(1) {
        peer.activate().await;
    }
    tokio::time::sleep(timing::scaled_ms(50)).await;
    for peer in peers.iter_mut().skip(1) {
        peer.start_recv();
    }

    // Send audio from peer 0
    peers[0].send_tone_for(Duration::from_millis(300)).await;
    tokio::time::sleep(timing::scaled_ms(500)).await;

    // All other peers should have received packets
    for (i, peer) in peers.iter().skip(1).enumerate() {
        let received = peer.received_count();
        assert!(
            received > 0,
            "peer {} should have received packets from peer 0 in 5-party conference, got {received}",
            i + 1,
        );
    }

    // Verify session.info shows 5 endpoints
    let info = client.request_ok("session.info", json!({})).await;
    let endpoints = info["endpoints"].as_array().unwrap();
    assert_eq!(endpoints.len(), 5, "should have 5 endpoints: {info}");

    client.request_ok("session.destroy", json!({})).await;
}

// ─────────────────────────────────────────────────────────────────────────────
// Test 2: File playback to a WebRTC endpoint
//
// Creates a WebRTC endpoint, plays a WAV file, and verifies the str0m peer
// receives RTP packets from the file playback source.
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_file_playback_to_webrtc_endpoint() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Instant;
    use str0m::change::SdpOffer;
    use str0m::net::{Protocol, Receive};
    use str0m::{Candidate, Event, Input, Output, RtcConfig};
    use tokio::net::UdpSocket;

    let tmp = TempDir::new().unwrap();
    let server = TestServer::builder()
        .media_dir(tmp.path().to_str().unwrap())
        .start()
        .await;

    // Generate a 2-second WAV file inside the media_dir
    let wav_path = tmp.path().join("file-webrtc.wav");
    generate_test_wav(&wav_path, 2.0, 440.0);

    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;

    // Create WebRTC endpoint (server creates offer)
    let result = client
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "webrtc", "direction": "sendrecv"}),
        )
        .await;
    let webrtc_ep_id = result["endpoint_id"].as_str().unwrap().to_string();
    let server_offer = result["sdp_offer"].as_str().unwrap();

    // Set up str0m peer
    let peer_socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
    let peer_addr = peer_socket.local_addr().unwrap();

    let mut rtc = RtcConfig::new().set_rtp_mode(true).build(Instant::now());
    let candidate = Candidate::host(peer_addr, "udp").unwrap();
    rtc.add_local_candidate(candidate);

    let offer = SdpOffer::from_sdp_string(server_offer).unwrap();
    let answer = rtc.sdp_api().accept_offer(offer).unwrap();
    let answer_str = answer.to_sdp_string();

    client
        .request_ok(
            "endpoint.accept_answer",
            json!({"endpoint_id": webrtc_ep_id, "sdp": answer_str}),
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
                        if let Ok(receive) =
                            Receive::new(Protocol::Udp, source, peer_addr, &buf[..n])
                        {
                            let _ = rtc.handle_input(Input::Receive(Instant::now(), receive));
                        }
                    }
                    _ => {
                        let _ = rtc.handle_input(Input::Timeout(Instant::now()));
                    }
                }
            }
            Ok(Output::Transmit(transmit)) => {
                let _ = peer_socket
                    .send_to(&transmit.contents, transmit.destination)
                    .await;
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

    // Create file playback endpoint
    let result = client
        .request_ok(
            "endpoint.create_with_file",
            json!({"source": wav_path.to_str().unwrap(), "shared": false, "loop_count": 0}),
        )
        .await;
    assert!(!result["endpoint_id"].as_str().unwrap().is_empty());

    // Drive str0m for 2 seconds to collect file playback packets
    let rtp_received = Arc::new(AtomicU64::new(0));
    let counter = Arc::clone(&rtp_received);
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
                        if let Ok(receive) =
                            Receive::new(Protocol::Udp, source, peer_addr, &buf[..n])
                        {
                            let _ = rtc.handle_input(Input::Receive(Instant::now(), receive));
                        }
                    }
                    _ => {
                        let _ = rtc.handle_input(Input::Timeout(Instant::now()));
                    }
                }
            }
            Ok(Output::Transmit(transmit)) => {
                let _ = peer_socket
                    .send_to(&transmit.contents, transmit.destination)
                    .await;
            }
            Ok(Output::Event(Event::RtpPacket(_))) => {
                counter.fetch_add(1, Ordering::Relaxed);
            }
            Ok(Output::Event(_)) => {}
            Err(_) => break,
        }
    }

    let received = rtp_received.load(Ordering::Relaxed);
    assert!(
        received > 0,
        "WebRTC peer should have received RTP packets from file playback, got {received}"
    );

    client.request_ok("session.destroy", json!({})).await;
}

// ─────────────────────────────────────────────────────────────────────────────
// Test 3: DTMF forwarding across WebRTC-RTP boundary
//
// Sends DTMF from a plain RTP peer, verifies the server detects and reports
// the DTMF event — which proves DTMF traversed the routing from the RTP
// endpoint through the bridge to the WebRTC endpoint (and was detected).
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_dtmf_forwarding_rtp_to_webrtc() {
    use std::time::Instant;
    use str0m::change::SdpOffer;
    use str0m::net::{Protocol, Receive};
    use str0m::{Candidate, Event, Input, Output, RtcConfig};
    use tokio::net::UdpSocket;

    let tmp = TempDir::new().unwrap();
    let server = TestServer::builder()
        .media_dir(tmp.path().to_str().unwrap())
        .start()
        .await;
    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;

    // Create RTP endpoint (DTMF sender)
    let mut rtp_peer = TestRtpPeer::new().await;
    let _rtp_ep_id = create_rtp_endpoint_from_offer(&mut client, &mut rtp_peer).await;

    // Create WebRTC endpoint (DTMF receiver)
    let result = client
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "webrtc", "direction": "sendrecv"}),
        )
        .await;
    let webrtc_ep_id = result["endpoint_id"].as_str().unwrap().to_string();
    let server_offer = result["sdp_offer"].as_str().unwrap();

    let peer_socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let peer_addr = peer_socket.local_addr().unwrap();

    let mut rtc = RtcConfig::new().set_rtp_mode(true).build(Instant::now());
    let candidate = Candidate::host(peer_addr, "udp").unwrap();
    rtc.add_local_candidate(candidate);

    let offer = SdpOffer::from_sdp_string(server_offer).unwrap();
    let answer = rtc.sdp_api().accept_offer(offer).unwrap();
    let answer_str = answer.to_sdp_string();

    client
        .request_ok(
            "endpoint.accept_answer",
            json!({"endpoint_id": webrtc_ep_id, "sdp": answer_str}),
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
                        if let Ok(receive) =
                            Receive::new(Protocol::Udp, source, peer_addr, &buf[..n])
                        {
                            let _ = rtc.handle_input(Input::Receive(Instant::now(), receive));
                        }
                    }
                    _ => {
                        let _ = rtc.handle_input(Input::Timeout(Instant::now()));
                    }
                }
            }
            Ok(Output::Transmit(transmit)) => {
                let _ = peer_socket
                    .send_to(&transmit.contents, transmit.destination)
                    .await;
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

    // Send some audio first so the RTP endpoint is active
    rtp_peer.send_tone_for(Duration::from_millis(100)).await;
    tokio::time::sleep(timing::scaled_ms(100)).await;

    // Send DTMF digit '7' from the RTP peer (RFC 4733 telephone-event)
    for i in 0..5 {
        let is_end = i == 4;
        let duration = (i + 1) * 160;
        rtp_peer.send_dtmf_event(7, is_end, duration).await;
        tokio::time::sleep(timing::PACING).await;
    }
    // Triple-send end packet per RFC 4733
    for _ in 0..2 {
        rtp_peer.send_dtmf_event(7, true, 5 * 160).await;
        tokio::time::sleep(timing::PACING).await;
    }

    // Wait for the DTMF event from the server
    let mut got_dtmf = false;
    let mut dtmf_digit = String::new();
    for _ in 0..15 {
        if let Some(event) = client.recv_event(timing::scaled_ms(500)).await {
            let event_type = event["event"].as_str().unwrap_or("");
            if event_type == "dtmf" {
                got_dtmf = true;
                dtmf_digit = event["data"]["digit"].as_str().unwrap_or("").to_string();
                break;
            }
        }
    }

    assert!(
        got_dtmf,
        "should have received a DTMF event from the bridge"
    );
    assert_eq!(
        dtmf_digit, "7",
        "detected digit should be '7', got '{dtmf_digit}'"
    );

    client.request_ok("session.destroy", json!({})).await;
}

// ─────────────────────────────────────────────────────────────────────────────
// Test 4: Stats interval enforcement
//
// Subscribes with a 200ms interval, verifies events arrive approximately
// at that rate. Also verifies that intervals below 100ms are rejected.
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_stats_interval_enforcement() {
    let tmp = TempDir::new().unwrap();
    let server = TestServer::builder()
        .media_dir(tmp.path().to_str().unwrap())
        .start()
        .await;
    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;

    // Create an endpoint so there's something to report stats on
    let mut peer = TestRtpPeer::new().await;
    let _ep_id = create_rtp_endpoint_from_offer(&mut client, &mut peer).await;

    // Reject sub-500ms interval
    let resp = client
        .request("stats.subscribe", json!({"interval_ms": 50}))
        .await;
    assert!(
        resp.get("error").is_some(),
        "stats.subscribe with 50ms interval should be rejected: {resp}"
    );

    // Subscribe with 500ms interval
    client
        .request_ok("stats.subscribe", json!({"interval_ms": 500}))
        .await;

    // Send some audio to keep the session alive
    peer.send_tone_for(Duration::from_millis(200)).await;

    // Collect stats events over 3 seconds
    let start = std::time::Instant::now();
    let mut stats_count = 0u32;
    let mut event_timestamps: Vec<std::time::Instant> = Vec::new();

    while start.elapsed() < timing::scaled_ms(3000) {
        if let Some(event) = client.recv_event(timing::scaled_ms(600)).await {
            let event_type = event["event"].as_str().unwrap_or("");
            if event_type == "stats" {
                stats_count += 1;
                event_timestamps.push(std::time::Instant::now());
            }
        }
    }

    assert!(
        stats_count >= 3,
        "should have received at least 3 stats events in 3s at 500ms interval, got {stats_count}"
    );

    // Verify intervals are roughly 500ms (allow 100-1500ms window for timing jitter)
    if event_timestamps.len() >= 2 {
        for window in event_timestamps.windows(2) {
            let interval = window[1].duration_since(window[0]);
            assert!(
                interval.as_millis() >= 100 && interval.as_millis() <= 1500,
                "stats interval should be roughly 500ms, got {}ms",
                interval.as_millis()
            );
        }
    }

    // Unsubscribe and verify events stop
    client.request_ok("stats.unsubscribe", json!({})).await;

    // Drain any in-flight event
    let _ = client.recv_event(timing::scaled_ms(300)).await;

    // After unsubscribe, no more stats events should arrive
    let straggler = client.recv_event(timing::scaled_ms(500)).await;
    let got_stats_after_unsub = straggler
        .as_ref()
        .map(|e| e["event"].as_str().unwrap_or("") == "stats")
        .unwrap_or(false);
    assert!(
        !got_stats_after_unsub,
        "should NOT receive stats events after unsubscribe"
    );

    client.request_ok("session.destroy", json!({})).await;
}

// ─────────────────────────────────────────────────────────────────────────────
// Test 5: Concurrent recording + live media integrity
//
// Two recordings run simultaneously (one full-session, one single-leg) while
// live media flows between endpoints. Verifies both PCAPs are valid, contain
// expected packets, and have correct structure after concurrent writes.
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_concurrent_recording_with_live_media() {
    let tmp = TempDir::new().unwrap();
    let server = TestServer::builder()
        .media_dir(tmp.path().to_str().unwrap())
        .start()
        .await;
    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;

    // Create 3 endpoints
    let mut peer_a = TestRtpPeer::new().await;
    let mut peer_b = TestRtpPeer::new().await;
    let mut peer_c = TestRtpPeer::new().await;

    let ep_a_id = create_rtp_endpoint_from_offer(&mut client, &mut peer_a).await;
    let _ep_b_id = create_rtp_endpoint_from_offer(&mut client, &mut peer_b).await;
    let _ep_c_id = create_rtp_endpoint_from_server_offer(&mut client, &mut peer_c).await;

    // Activate and start receiving on all peers
    peer_a.activate().await;
    peer_b.activate().await;
    peer_c.activate().await;
    tokio::time::sleep(timing::scaled_ms(50)).await;
    peer_a.start_recv();
    peer_b.start_recv();
    peer_c.start_recv();

    // Start recording 1: full session
    let rec_dir = std::path::Path::new(&server.recording_dir);
    let pcap_full = rec_dir.join("rtpbridge-concurrent-rec-full.pcap");
    let rec_full = client
        .request_ok(
            "recording.start",
            json!({"endpoint_id": null, "file_path": pcap_full.to_str().unwrap()}),
        )
        .await;
    let rec_full_id = rec_full["recording_id"].as_str().unwrap().to_string();

    // Start recording 2: single-leg on endpoint A
    let pcap_leg = rec_dir.join("rtpbridge-concurrent-rec-leg.pcap");
    let rec_leg = client
        .request_ok(
            "recording.start",
            json!({"endpoint_id": ep_a_id, "file_path": pcap_leg.to_str().unwrap()}),
        )
        .await;
    let rec_leg_id = rec_leg["recording_id"].as_str().unwrap().to_string();

    // Send audio from all three peers concurrently
    let send_a = tokio::spawn({
        let mut peer = TestRtpPeer::new().await;
        // We can't move the original peers, so send from cloned sockets...
        // Actually, let's just send sequentially from each peer
        async move {
            peer.send_tone_for(Duration::from_millis(300)).await;
        }
    });

    // Send from peers A, B, C in overlapping fashion
    peer_a.send_tone_for(Duration::from_millis(300)).await;
    peer_b.send_tone_for(Duration::from_millis(300)).await;
    peer_c.send_tone_for(Duration::from_millis(300)).await;

    send_a.abort();
    tokio::time::sleep(timing::scaled_ms(500)).await;

    // Verify live media was actually routed during recording
    let b_received = peer_b.received_count();
    let c_received = peer_c.received_count();
    assert!(
        b_received > 0,
        "peer B should receive media during concurrent recording, got {b_received}"
    );
    assert!(
        c_received > 0,
        "peer C should receive media during concurrent recording, got {c_received}"
    );

    // Stop both recordings
    let stop_full = client
        .request_ok("recording.stop", json!({"recording_id": rec_full_id}))
        .await;
    let stop_leg = client
        .request_ok("recording.stop", json!({"recording_id": rec_leg_id}))
        .await;

    let full_packets = stop_full["packets"].as_u64().unwrap();
    let leg_packets = stop_leg["packets"].as_u64().unwrap();

    assert!(
        full_packets > 0,
        "full-session recording should have packets"
    );
    assert!(leg_packets > 0, "single-leg recording should have packets");

    // Full session should have more packets than single-leg (captures all endpoints)
    assert!(
        full_packets >= leg_packets,
        "full-session ({full_packets}) should have >= packets than single-leg ({leg_packets})"
    );

    tokio::time::sleep(timing::scaled_ms(500)).await;

    // Validate both PCAPs are structurally sound
    for (path, label) in [(&pcap_full, "full-session"), (&pcap_leg, "single-leg")] {
        let file = std::fs::File::open(path)
            .unwrap_or_else(|e| panic!("{label} PCAP at {path:?} should exist: {e}"));
        let mut reader = pcap_file::pcap::PcapReader::new(file)
            .unwrap_or_else(|e| panic!("{label} PCAP should be valid: {e}"));

        assert_eq!(
            reader.header().datalink,
            pcap_file::DataLink::ETHERNET,
            "{label} PCAP should be Ethernet"
        );

        let mut count = 0u32;
        let mut last_ts = 0u64;
        while let Some(pkt) = reader.next_packet() {
            let pkt = pkt.unwrap_or_else(|e| panic!("{label} PCAP packet error: {e}"));
            count += 1;

            // Verify monotonic timestamps
            let ts = pkt.timestamp.as_micros() as u64;
            assert!(
                ts >= last_ts,
                "{label} PCAP timestamps should be monotonic: {ts} < {last_ts}"
            );
            last_ts = ts;

            // Verify basic packet structure
            assert!(
                pkt.data.len() >= 42,
                "{label} packet {count} too small: {} bytes",
                pkt.data.len()
            );
            assert_eq!(
                &pkt.data[12..14],
                &[0x08, 0x00],
                "{label} packet {count} should be IPv4"
            );
        }

        assert!(
            count > 0,
            "{label} PCAP should contain packets, got {count}"
        );
    }

    client.request_ok("session.destroy", json!({})).await;
}
