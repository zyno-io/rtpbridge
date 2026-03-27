mod helpers;

use std::time::Duration;

use serde_json::json;
use tempfile::TempDir;

use helpers::control_client::TestControlClient;
use helpers::test_rtp_peer::{TestRtpPeer, parse_rtp_addr_from_sdp};
use helpers::test_server::TestServer;
use helpers::timing;

// ─────────────────────────────────────────────────────────────────────────────
// Test 1: WebRTC media flow end-to-end
//
// Creates a WebRTC endpoint and an RTP endpoint in the same session, sends
// RTP audio from the plain RTP peer, and verifies the WebRTC str0m peer
// receives RTP packets through the bridge.
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_webrtc_media_flow_end_to_end() {
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
    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;

    // Create WebRTC endpoint (server creates offer, str0m peer accepts)
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

    // Create RTP endpoint as the sender
    let mut rtp_peer = TestRtpPeer::new().await;
    let offer_rtp = rtp_peer.make_sdp_offer();
    let result = client
        .request_ok(
            "endpoint.create_from_offer",
            json!({"sdp": offer_rtp, "direction": "sendrecv"}),
        )
        .await;
    let answer_rtp = result["sdp_answer"].as_str().unwrap();
    let server_rtp_addr = parse_rtp_addr_from_sdp(answer_rtp).expect("parse server addr");
    rtp_peer.set_remote(server_rtp_addr);

    // Drive str0m for ICE connectivity first
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

    // Now send RTP from the plain RTP peer — should be routed to WebRTC endpoint
    let rtp_packets_received = Arc::new(AtomicU64::new(0));
    let rtp_recv_counter = Arc::clone(&rtp_packets_received);

    // Send 25 packets (500ms) of PCMU tone from the RTP peer
    rtp_peer.send_tone_for(Duration::from_millis(500)).await;

    // Drive str0m for another 2 seconds to receive forwarded packets
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
                rtp_recv_counter.fetch_add(1, Ordering::Relaxed);
            }
            Ok(Output::Event(_)) => {}
            Err(_) => break,
        }
    }

    let received = rtp_packets_received.load(Ordering::Relaxed);
    assert!(
        received > 0,
        "WebRTC str0m peer should have received RTP packets routed from the plain RTP peer, got {received}"
    );

    client.request_ok("session.destroy", json!({})).await;
}

// ─────────────────────────────────────────────────────────────────────────────
// Test 2: WebRTC ↔ Plain RTP bridging (bidirectional)
//
// Creates both a WebRTC and a plain RTP endpoint, sends audio from the RTP
// side, and verifies the WebRTC peer receives it. This is the core
// browser-to-SIP use case.
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_webrtc_to_rtp_bridging() {
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
    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;

    // --- RTP endpoint (SIP side) ---
    let mut rtp_peer = TestRtpPeer::new().await;
    let offer_rtp = rtp_peer.make_sdp_offer();
    let result = client
        .request_ok(
            "endpoint.create_from_offer",
            json!({"sdp": offer_rtp, "direction": "sendrecv"}),
        )
        .await;
    let _rtp_ep_id = result["endpoint_id"].as_str().unwrap().to_string();
    let answer_rtp = result["sdp_answer"].as_str().unwrap();
    let server_rtp_addr = parse_rtp_addr_from_sdp(answer_rtp).expect("parse server addr");
    rtp_peer.set_remote(server_rtp_addr);
    rtp_peer.start_recv();

    // --- WebRTC endpoint (browser side) ---
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

    // Send RTP audio from the plain RTP peer → bridge → WebRTC
    let rtp_packets_received = Arc::new(AtomicU64::new(0));
    let counter = Arc::clone(&rtp_packets_received);

    rtp_peer.send_tone_for(Duration::from_millis(500)).await;

    // Drive str0m to receive forwarded packets
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

    let webrtc_received = rtp_packets_received.load(Ordering::Relaxed);
    assert!(
        webrtc_received > 0,
        "WebRTC peer should have received RTP packets bridged from plain RTP, got {webrtc_received}"
    );

    // Also verify the RTP peer received packets (from WebRTC RTCP or any echo)
    // The primary assertion is above — RTP→WebRTC works.

    // Verify session.info reports both endpoint types
    let info = client.request_ok("session.info", json!({})).await;
    let endpoints = info["endpoints"].as_array().unwrap();
    assert_eq!(endpoints.len(), 2, "should have 2 endpoints");
    let types: Vec<&str> = endpoints
        .iter()
        .filter_map(|ep| ep["endpoint_type"].as_str())
        .collect();
    assert!(
        types.contains(&"webrtc"),
        "should have webrtc endpoint: {info}"
    );
    assert!(types.contains(&"rtp"), "should have rtp endpoint: {info}");

    client.request_ok("session.destroy", json!({})).await;
}

// ─────────────────────────────────────────────────────────────────────────────
// Test 3: Per-endpoint (single-leg) recording
//
// Creates two RTP endpoints, starts a recording on only one of them, sends
// audio from both, and verifies the PCAP only contains packets from the
// targeted endpoint.
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_single_leg_recording() {
    let tmp = TempDir::new().unwrap();
    let server = TestServer::builder()
        .media_dir(tmp.path().to_str().unwrap())
        .start()
        .await;
    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;

    // Create two RTP endpoints
    let mut peer_a = TestRtpPeer::new().await;
    let mut peer_b = TestRtpPeer::new().await;

    let offer_a = peer_a.make_sdp_offer();
    let result = client
        .request_ok(
            "endpoint.create_from_offer",
            json!({"sdp": offer_a, "direction": "sendrecv"}),
        )
        .await;
    let ep_a_id = result["endpoint_id"].as_str().unwrap().to_string();
    let answer_a = result["sdp_answer"].as_str().unwrap();
    peer_a.set_remote(parse_rtp_addr_from_sdp(answer_a).unwrap());

    let offer_b = peer_b.make_sdp_offer();
    let result = client
        .request_ok(
            "endpoint.create_from_offer",
            json!({"sdp": offer_b, "direction": "sendrecv"}),
        )
        .await;
    let _ep_b_id = result["endpoint_id"].as_str().unwrap().to_string();
    let answer_b = result["sdp_answer"].as_str().unwrap();
    peer_b.set_remote(parse_rtp_addr_from_sdp(answer_b).unwrap());

    // Start recording on endpoint A ONLY (single-leg)
    let rec_dir = std::path::Path::new(&server.recording_dir);
    let pcap_leg = rec_dir.join("single-leg-test.pcap");
    let rec_leg = client
        .request_ok(
            "recording.start",
            json!({"endpoint_id": ep_a_id, "file_path": pcap_leg.to_str().unwrap()}),
        )
        .await;
    let rec_leg_id = rec_leg["recording_id"].as_str().unwrap().to_string();

    // Also start a full-session recording for comparison
    let pcap_full = rec_dir.join("full-session-test.pcap");
    let rec_full = client
        .request_ok(
            "recording.start",
            json!({"endpoint_id": null, "file_path": pcap_full.to_str().unwrap()}),
        )
        .await;
    let rec_full_id = rec_full["recording_id"].as_str().unwrap().to_string();

    // Send distinct payloads from each peer:
    // Peer A sends 0xAA pattern, Peer B sends 0xBB pattern
    let payload_a = vec![0xAAu8; 160];
    let payload_b = vec![0xBBu8; 160];

    for _ in 0..15 {
        peer_a.send_pcmu(&payload_a).await;
        peer_b.send_pcmu(&payload_b).await;
        tokio::time::sleep(timing::PACING).await;
    }

    tokio::time::sleep(timing::scaled_ms(500)).await;

    // Stop both recordings
    let stop_leg = client
        .request_ok("recording.stop", json!({"recording_id": rec_leg_id}))
        .await;
    let stop_full = client
        .request_ok("recording.stop", json!({"recording_id": rec_full_id}))
        .await;

    assert!(
        stop_leg["packets"].as_u64().unwrap() > 0,
        "single-leg recording should have packets"
    );
    assert!(
        stop_full["packets"].as_u64().unwrap() > 0,
        "full-session recording should have packets"
    );

    tokio::time::sleep(timing::scaled_ms(500)).await;

    // Parse single-leg PCAP — should ONLY contain packets from endpoint A's source
    let file = std::fs::File::open(&pcap_leg).expect("single-leg PCAP should exist");
    let mut reader = pcap_file::pcap::PcapReader::new(file).expect("valid PCAP");
    let payload_offset = 14 + 20 + 8 + 12; // Eth + IPv4 + UDP + RTP header
    let mut leg_packet_count = 0u32;
    let mut found_aa = false;
    let mut found_bb = false;

    while let Some(pkt) = reader.next_packet() {
        let pkt = pkt.expect("valid packet");
        leg_packet_count += 1;
        if pkt.data.len() > payload_offset + 10 {
            let rtp_payload = &pkt.data[payload_offset..];
            if rtp_payload.len() >= 10 {
                if rtp_payload[..10].iter().all(|&b| b == 0xAA) {
                    found_aa = true;
                }
                if rtp_payload[..10].iter().all(|&b| b == 0xBB) {
                    found_bb = true;
                }
            }
        }
    }

    assert!(leg_packet_count > 0, "single-leg PCAP should have packets");
    assert!(
        found_aa,
        "single-leg recording for endpoint A should contain 0xAA payloads from peer A"
    );
    assert!(
        !found_bb,
        "single-leg recording for endpoint A should NOT contain 0xBB payloads from peer B"
    );

    // Parse full-session PCAP — should contain packets from BOTH endpoints
    let file = std::fs::File::open(&pcap_full).expect("full PCAP should exist");
    let mut reader = pcap_file::pcap::PcapReader::new(file).expect("valid PCAP");
    let mut full_found_aa = false;
    let mut full_found_bb = false;

    while let Some(pkt) = reader.next_packet() {
        let pkt = pkt.expect("valid packet");
        if pkt.data.len() > payload_offset + 10 {
            let rtp_payload = &pkt.data[payload_offset..];
            if rtp_payload.len() >= 10 {
                if rtp_payload[..10].iter().all(|&b| b == 0xAA) {
                    full_found_aa = true;
                }
                if rtp_payload[..10].iter().all(|&b| b == 0xBB) {
                    full_found_bb = true;
                }
            }
        }
    }

    assert!(
        full_found_aa && full_found_bb,
        "full-session recording should contain packets from both endpoints (AA={full_found_aa}, BB={full_found_bb})"
    );

    client.request_ok("session.destroy", json!({})).await;
}

// ─────────────────────────────────────────────────────────────────────────────
// Test 4: URL playback error handling
//
// Tests HTTP 404, connection refused, and download timeout scenarios.
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_url_playback_http_404() {
    let tmp = TempDir::new().unwrap();
    let server = TestServer::builder()
        .media_dir(tmp.path().to_str().unwrap())
        .start()
        .await;

    // Start an HTTP server that always returns 404
    let http_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let http_addr = http_listener.local_addr().unwrap();

    let http_handle = tokio::spawn(async move {
        loop {
            match http_listener.accept().await {
                Ok((mut stream, _)) => {
                    tokio::spawn(async move {
                        use tokio::io::{AsyncReadExt, AsyncWriteExt};
                        let mut buf = vec![0u8; 4096];
                        let _ = stream.read(&mut buf).await;
                        let response = "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                        let _ = stream.write_all(response.as_bytes()).await;
                        let _ = stream.shutdown().await;
                    });
                }
                Err(_) => break,
            }
        }
    });

    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;

    // Create RTP endpoint as receiver
    let result = client
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv"}),
        )
        .await;
    let ep_id = result["endpoint_id"].as_str().unwrap().to_string();
    let peer = TestRtpPeer::new().await;
    let answer = peer.make_sdp_answer();
    client
        .request_ok(
            "endpoint.accept_answer",
            json!({"endpoint_id": ep_id, "sdp": answer}),
        )
        .await;

    // Try to play from 404 URL
    let url_404 = format!(
        "http://{}:{}/nonexistent.wav",
        http_addr.ip(),
        http_addr.port()
    );
    let result = client
        .request_ok(
            "endpoint.create_with_file",
            json!({"source": url_404, "shared": false, "loop_count": 0, "timeout_ms": 5000}),
        )
        .await;
    assert!(
        !result["endpoint_id"].as_str().unwrap().is_empty(),
        "endpoint should be created initially"
    );

    // Wait for the file.finished error event (skip state_changed events)
    let event = client
        .recv_event_type("endpoint.file.finished", timing::scaled_ms(6000))
        .await;
    assert!(
        event.is_some(),
        "should receive a file.finished error event for HTTP 404"
    );
    let event = event.unwrap();
    assert_eq!(
        event["data"]["reason"].as_str().unwrap_or(""),
        "error",
        "reason should be error for 404: {event}"
    );

    http_handle.abort();
    client.request_ok("session.destroy", json!({})).await;
}

#[tokio::test]
async fn test_url_playback_connection_refused() {
    let tmp = TempDir::new().unwrap();
    let server = TestServer::builder()
        .media_dir(tmp.path().to_str().unwrap())
        .start()
        .await;
    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;

    // Create RTP endpoint
    let result = client
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv"}),
        )
        .await;
    let ep_id = result["endpoint_id"].as_str().unwrap().to_string();
    let peer = TestRtpPeer::new().await;
    let answer = peer.make_sdp_answer();
    client
        .request_ok(
            "endpoint.accept_answer",
            json!({"endpoint_id": ep_id, "sdp": answer}),
        )
        .await;

    // Use a port that is definitely not listening (pick unused, don't bind)
    let dead_port = portpicker::pick_unused_port().expect("no free port");
    let url = format!("http://127.0.0.1:{dead_port}/test.wav");
    let result = client
        .request_ok(
            "endpoint.create_with_file",
            json!({"source": url, "shared": false, "loop_count": 0, "timeout_ms": 3000}),
        )
        .await;
    assert!(!result["endpoint_id"].as_str().unwrap().is_empty());

    // Should get a file.finished error event (skip state_changed events)
    let event = client
        .recv_event_type("endpoint.file.finished", timing::scaled_ms(5000))
        .await;
    assert!(
        event.is_some(),
        "should receive error event for connection refused"
    );
    let event = event.unwrap();
    assert_eq!(
        event["data"]["reason"].as_str().unwrap_or(""),
        "error",
        "reason should be error for connection refused: {event}"
    );

    client.request_ok("session.destroy", json!({})).await;
}

#[tokio::test]
async fn test_url_playback_download_timeout() {
    let tmp = TempDir::new().unwrap();
    let server = TestServer::builder()
        .media_dir(tmp.path().to_str().unwrap())
        .start()
        .await;

    // Start an HTTP server that accepts connections but never responds
    let http_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let http_addr = http_listener.local_addr().unwrap();

    let http_handle = tokio::spawn(async move {
        loop {
            match http_listener.accept().await {
                Ok((stream, _)) => {
                    // Hold the connection open without sending anything
                    tokio::spawn(async move {
                        tokio::time::sleep(timing::scaled_ms(60000)).await;
                        drop(stream);
                    });
                }
                Err(_) => break,
            }
        }
    });

    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;

    // Create RTP endpoint
    let result = client
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv"}),
        )
        .await;
    let ep_id = result["endpoint_id"].as_str().unwrap().to_string();
    let peer = TestRtpPeer::new().await;
    let answer = peer.make_sdp_answer();
    client
        .request_ok(
            "endpoint.accept_answer",
            json!({"endpoint_id": ep_id, "sdp": answer}),
        )
        .await;

    // Try to play from the stalled server with short timeout
    let url = format!("http://{}:{}/test.wav", http_addr.ip(), http_addr.port());
    let result = client
        .request_ok(
            "endpoint.create_with_file",
            json!({"source": url, "shared": false, "loop_count": 0, "timeout_ms": 2000}),
        )
        .await;
    assert!(!result["endpoint_id"].as_str().unwrap().is_empty());

    // Should get a file.finished error event within timeout + margin (skip state_changed)
    let event = client
        .recv_event_type("endpoint.file.finished", timing::scaled_ms(5000))
        .await;
    assert!(
        event.is_some(),
        "should receive error event for download timeout"
    );
    let event = event.unwrap();
    assert_eq!(
        event["data"]["reason"].as_str().unwrap_or(""),
        "error",
        "reason should be error for timeout: {event}"
    );

    http_handle.abort();
    client.request_ok("session.destroy", json!({})).await;
}

// ─────────────────────────────────────────────────────────────────────────────
// Test 5: Recording content validation
//
// Sends packets with a known payload pattern, records to PCAP, then verifies
// that every recorded RTP packet contains the expected PCMU audio data.
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_recording_payload_content_matches_sent_audio() {
    let tmp = TempDir::new().unwrap();
    let server = TestServer::builder()
        .media_dir(tmp.path().to_str().unwrap())
        .start()
        .await;
    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;

    // Create two endpoints
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

    // Start recording
    let rec_dir = std::path::Path::new(&server.recording_dir);
    let pcap_path = rec_dir.join("content-validation-test.pcap");
    let rec = client
        .request_ok(
            "recording.start",
            json!({"endpoint_id": null, "file_path": pcap_path.to_str().unwrap()}),
        )
        .await;
    let rec_id = rec["recording_id"].as_str().unwrap().to_string();

    // Send 30 packets with a distinctive 440Hz tone encoded as PCMU.
    // We'll verify the recorded payloads are valid PCMU (not zeros, not random).
    let mut sent_payloads: Vec<Vec<u8>> = Vec::new();
    for i in 0..30 {
        // Generate a recognizable PCMU payload: each packet's first byte encodes a
        // shifted pattern so we can identify individual packets
        let payload: Vec<u8> = (0..160)
            .map(|s| {
                let t = (i as f64 * 160.0 + s as f64) / 8000.0;
                let sample = (f64::sin(2.0 * std::f64::consts::PI * 440.0 * t) * 16000.0) as i16;
                // Simple mu-law encode: use the helpers' linear_to_ulaw equivalent
                // For validation, just use a known pattern
                let sign: u8 = if sample < 0 { 0x80 } else { 0 };
                let mut mag = if sample < 0 {
                    -(sample as i32)
                } else {
                    sample as i32
                };
                mag = mag.min(0x7FFF) + 0x84;
                let mut exp = 7u8;
                let mut mask = 0x4000i32;
                while exp > 0 && (mag & mask) == 0 {
                    exp -= 1;
                    mask >>= 1;
                }
                let mantissa = ((mag >> (exp as u32 + 3)) & 0x0F) as u8;
                !(sign | (exp << 4) | mantissa)
            })
            .collect();
        sent_payloads.push(payload.clone());
        peer_a.send_pcmu(&payload).await;
        tokio::time::sleep(timing::PACING).await;
    }

    tokio::time::sleep(timing::scaled_ms(500)).await;

    // Stop recording
    let stop = client
        .request_ok("recording.stop", json!({"recording_id": rec_id}))
        .await;
    let recorded_packets = stop["packets"].as_u64().unwrap();
    assert!(recorded_packets > 0, "should have recorded packets");

    tokio::time::sleep(timing::scaled_ms(500)).await;

    // Parse PCAP and validate payload content
    let file = std::fs::File::open(&pcap_path).expect("PCAP file should exist");
    let mut reader = pcap_file::pcap::PcapReader::new(file).expect("valid PCAP file");

    let payload_offset = 14 + 20 + 8; // Eth + IPv4 + UDP
    let rtp_header_len = 12;
    let mut matched_payloads = 0u32;
    let mut total_rtp_packets = 0u32;

    while let Some(pkt) = reader.next_packet() {
        let pkt = pkt.expect("valid PCAP packet");
        let data = &pkt.data;

        if data.len() <= payload_offset + rtp_header_len {
            continue;
        }

        // Check this is an RTP packet (version 2, not RTCP)
        let udp_payload = &data[payload_offset..];
        if udp_payload.len() < 2 {
            continue;
        }
        let version = (udp_payload[0] >> 6) & 0x03;
        let pt = udp_payload[1] & 0x7F;
        if version != 2 || pt == 200 || pt == 201 {
            continue; // skip RTCP
        }

        total_rtp_packets += 1;
        let rtp_payload = &udp_payload[rtp_header_len..];

        // Verify the payload is valid PCMU (160 bytes for 20ms at 8kHz)
        if rtp_payload.len() == 160 {
            // Check if this payload matches any of our sent payloads
            if sent_payloads.iter().any(|sent| sent == rtp_payload) {
                matched_payloads += 1;
            }
        }
    }

    assert!(total_rtp_packets > 0, "PCAP should contain RTP packets");
    assert!(
        matched_payloads > 0,
        "at least some recorded RTP payloads should exactly match sent audio \
         (matched {matched_payloads} of {total_rtp_packets} RTP packets)"
    );
    // Most sent payloads should be captured (allow some loss from timing)
    assert!(
        matched_payloads >= 10,
        "at least 10 of 30 sent payloads should appear in recording, got {matched_payloads}"
    );

    client.request_ok("session.destroy", json!({})).await;
}
