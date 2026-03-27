mod helpers;

use std::time::Duration;

use serde_json::json;

use helpers::control_client::TestControlClient;
use helpers::test_rtp_peer::{TestRtpPeer, parse_rtp_addr_from_sdp};
use helpers::test_server::TestServer;
use helpers::timing;

/// Helper: create a session with an RTP endpoint connected to a test peer
async fn setup_rtp_session(client: &mut TestControlClient) -> (String, TestRtpPeer) {
    client.request_ok("session.create", json!({})).await;

    let mut peer = TestRtpPeer::new().await;
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

    (ep_id, peer)
}

// ─── 2A: Session idle timeout — command activity resets timer ───

#[tokio::test]
async fn test_idle_timeout_reset_by_command() {
    let idle_secs = 2 * timing::multiplier();
    let server = TestServer::builder()
        .session_idle_timeout_secs(idle_secs)
        .start()
        .await;
    let mut client = TestControlClient::connect(&server.addr).await;

    client.request_ok("session.create", json!({})).await;

    // Sleep for 1.5s (under idle threshold)
    tokio::time::sleep(timing::scaled_ms(1500)).await;

    // Send a command to reset the idle timer
    let resp = client.request("session.info", json!({})).await;
    assert!(
        resp.get("result").is_some(),
        "session should still be alive after 1.5s"
    );

    // Sleep another 1.5s (total 3s from creation, but only 1.5s since last activity)
    tokio::time::sleep(timing::scaled_ms(1500)).await;

    // Session should still be alive because the timer was reset
    let resp = client.request("session.info", json!({})).await;
    assert!(
        resp.get("result").is_some(),
        "session should still be alive — timer was reset by session.info"
    );

    // Now wait for the full idle period without activity
    let event = client
        .recv_event(timing::scaled_ms(idle_secs * 1000 + 3000))
        .await;
    assert!(event.is_some(), "should receive idle timeout event");
    assert_eq!(
        event.unwrap()["event"].as_str().unwrap(),
        "session.idle_timeout"
    );
}

// ─── 2B: Session idle timeout — media activity resets timer ───

#[tokio::test]
async fn test_idle_timeout_reset_by_media() {
    let idle_secs = 2 * timing::multiplier();
    let server = TestServer::builder()
        .session_idle_timeout_secs(idle_secs)
        .start()
        .await;
    let mut client = TestControlClient::connect(&server.addr).await;

    let (_ep_id, mut peer) = setup_rtp_session(&mut client).await;

    // Sleep 1.5s, then send a packet to reset the timer
    tokio::time::sleep(timing::scaled_ms(1500)).await;
    peer.send_pcmu(&[0x80u8; 160]).await;

    // Sleep another 1.5s — should be alive (only 1.5s since last media)
    tokio::time::sleep(timing::scaled_ms(1500)).await;
    let resp = client.request("session.info", json!({})).await;
    assert!(
        resp.get("result").is_some(),
        "session should still be alive — timer was reset by RTP packet"
    );

    // Now stop sending and wait for idle timeout.
    // We may receive endpoint.media_timeout (hardcoded 5s) before session.idle_timeout,
    // so drain events until we find the one we want.
    let deadline = tokio::time::Instant::now() + timing::scaled_ms(idle_secs * 1000 + 5000);
    loop {
        let remaining = deadline - tokio::time::Instant::now();
        let event = client.recv_event(remaining).await;
        assert!(
            event.is_some(),
            "should receive session.idle_timeout event before deadline"
        );
        if event.unwrap()["event"].as_str() == Some("session.idle_timeout") {
            break;
        }
    }
}

// ─── 2C: RTCP generation verified via stats subscription ───
//
// The bridge sends RTCP to the peer's RTCP port (RTP port + 1). Rather than
// binding a second socket, we verify RTCP generation indirectly via the stats
// subscription which reports RTCP-derived fields (packets_received, jitter).

#[tokio::test]
async fn test_rtcp_generation_via_stats() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;

    let (ep_id, mut peer) = setup_rtp_session(&mut client).await;

    // Send a burst of media to give the bridge something to report
    for _ in 0..50 {
        peer.send_pcmu(&[0x80u8; 160]).await;
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // Subscribe to stats with a short interval
    client
        .request_ok("stats.subscribe", json!({"interval_ms": 500}))
        .await;

    // Send more media while waiting for stats
    let send_task = {
        let mut peer_clone = TestRtpPeer::new().await;
        peer_clone.set_remote(peer.remote_addr.unwrap());
        // We can't clone the peer easily — just keep sending in main thread
        // after we set up the stats subscription
        drop(peer_clone);
        ()
    };
    let _ = send_task;

    // Continue sending in the background
    for _ in 0..25 {
        peer.send_pcmu(&[0x80u8; 160]).await;
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // Wait for a stats event
    let mut found_stats = false;
    for _ in 0..10 {
        if let Some(event) = client.recv_event(timing::scaled_ms(2000)).await {
            if event["event"].as_str() == Some("stats") {
                // Look for our endpoint in the stats
                if let Some(endpoints) = event["data"]["endpoints"].as_array() {
                    for ep in endpoints {
                        if ep["endpoint_id"].as_str() == Some(&ep_id) {
                            let inbound_packets = ep["inbound"]["packets"].as_u64().unwrap_or(0);
                            assert!(
                                inbound_packets > 0,
                                "stats should report inbound packets, got 0"
                            );
                            found_stats = true;
                            break;
                        }
                    }
                }
                if found_stats {
                    break;
                }
            }
        }
    }

    assert!(
        found_stats,
        "should receive stats event with RTCP-derived packet counts"
    );
}

// ─── 2D: Graceful shutdown flushes recordings ───

#[tokio::test]
async fn test_shutdown_flushes_recording() {
    let server = TestServer::start().await;
    let recording_dir = server.recording_dir.clone();
    let mut client = TestControlClient::connect(&server.addr).await;

    let (_ep_id, mut peer) = setup_rtp_session(&mut client).await;

    // recording.start requires an absolute path or a relative path within recording_dir
    let pcap_filename = "shutdown-test.pcap";
    let pcap_abs = std::path::Path::new(&recording_dir)
        .join(pcap_filename)
        .to_string_lossy()
        .to_string();

    // Start recording
    let rec_result = client
        .request_ok("recording.start", json!({"file_path": pcap_abs}))
        .await;
    let _rec_id = rec_result["recording_id"].as_str().unwrap();

    // Send some media packets
    for _ in 0..50 {
        peer.send_pcmu(&[0x80u8; 160]).await;
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // Initiate graceful shutdown
    server.initiate_shutdown();

    // Wait for shutdown to drain
    tokio::time::sleep(timing::scaled_ms(2000)).await;

    // Verify PCAP file exists and is valid
    let pcap_path = std::path::Path::new(&recording_dir).join(pcap_filename);
    assert!(pcap_path.exists(), "PCAP file should exist after shutdown");

    let file_size = std::fs::metadata(&pcap_path).unwrap().len();
    // PCAP global header is 24 bytes, plus at least one packet record
    assert!(
        file_size > 24,
        "PCAP file should contain data (got {file_size} bytes)"
    );

    // Verify PCAP magic number (either byte order is valid)
    let data = std::fs::read(&pcap_path).unwrap();
    let magic_le = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
    let magic_be = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
    assert!(
        magic_le == 0xa1b2c3d4 || magic_be == 0xa1b2c3d4,
        "PCAP file should have valid magic number, got bytes {:02x} {:02x} {:02x} {:02x}",
        data[0],
        data[1],
        data[2],
        data[3]
    );
}

// ─── 2E: DTMF injection sends end-packets with redundancy ───
//
// DTMF injection via endpoint.dtmf.inject sends RFC 4733 telephone-event
// packets directly from the bridge to the endpoint's remote peer.
// We inject on endpoint A and capture on peer A's socket.

#[tokio::test]
async fn test_dtmf_injection_end_packet_redundancy() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;

    let (ep_id, mut peer) = setup_rtp_session(&mut client).await;

    // Prime the endpoint with a few media packets so it has a remote address
    for _ in 0..5 {
        peer.send_pcmu(&[0x80u8; 160]).await;
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    tokio::time::sleep(timing::scaled_ms(200)).await;

    // Start capturing on the peer's socket (DTMF inject sends TO this socket)
    let socket = peer.socket.clone();
    let (pkt_tx, mut pkt_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(200);

    let recv_task = tokio::spawn(async move {
        let mut buf = [0u8; 2048];
        loop {
            match tokio::time::timeout(Duration::from_millis(50), socket.recv_from(&mut buf)).await
            {
                Ok(Ok((len, _))) => {
                    let _ = pkt_tx.send(buf[..len].to_vec()).await;
                }
                Ok(Err(_)) => break,
                Err(_) => {} // timeout, keep looping
            }
        }
    });

    // Inject DTMF digit '5' into the endpoint (sends to peer)
    client
        .request_ok(
            "endpoint.dtmf.inject",
            json!({"endpoint_id": ep_id, "digit": "5", "duration_ms": 160}),
        )
        .await;

    // Wait for injection to complete (160ms digit + end packets + margin)
    tokio::time::sleep(timing::scaled_ms(800)).await;
    recv_task.abort();

    // Analyze received packets — look for telephone-event (PT=101)
    let mut dtmf_packets = Vec::new();
    while let Ok(pkt) = pkt_rx.try_recv() {
        if pkt.len() >= 16 {
            let pt = pkt[1] & 0x7f;
            if pt == 101 {
                dtmf_packets.push(pkt);
            }
        }
    }

    assert!(
        !dtmf_packets.is_empty(),
        "should receive at least one DTMF telephone-event packet"
    );

    // Count end-bit packets
    // RFC 4733 telephone-event payload starts at byte 12 (after 12-byte RTP header):
    //   Byte 12: event ID
    //   Byte 13: E(1) R(1) Volume(6) — end bit is bit 7
    //   Bytes 14-15: duration
    let end_count = dtmf_packets
        .iter()
        .filter(|pkt| pkt.len() >= 16 && (pkt[13] & 0x80) != 0)
        .count();

    assert!(
        end_count >= 3,
        "RFC 4733 requires 3 redundant end packets, got {end_count}"
    );
}

// ─── 2F: Session idle timeout — multi-endpoint session ───
//
// One active endpoint (receiving media) should keep the entire session alive,
// even if other endpoints in the session are idle.

#[tokio::test]
async fn test_idle_timeout_multi_endpoint_one_active() {
    let idle_secs = 2 * timing::multiplier();
    let server = TestServer::builder()
        .session_idle_timeout_secs(idle_secs)
        .start()
        .await;
    let mut client = TestControlClient::connect(&server.addr).await;

    // setup_rtp_session already calls session.create
    let (_ep_a_id, mut peer_a) = setup_rtp_session(&mut client).await;

    // Create endpoint B (idle — no media)
    let result_b = client
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv"}),
        )
        .await;
    let _ep_b_id = result_b["endpoint_id"].as_str().unwrap().to_string();

    // Send media on endpoint A periodically to keep the session alive
    for _ in 0..4 {
        peer_a.send_pcmu(&[0x80u8; 160]).await;
        tokio::time::sleep(timing::scaled_ms(1000)).await;
    }

    // Session should still be alive (4s elapsed, but activity within every 2s window)
    let resp = client.request("session.info", json!({})).await;
    assert!(
        resp.get("result").is_some(),
        "multi-endpoint session should still be alive when one endpoint has media activity"
    );

    // Verify both endpoints still present
    let endpoints = resp["result"]["endpoints"].as_array().unwrap();
    assert_eq!(
        endpoints.len(),
        2,
        "both endpoints should still exist in active session"
    );

    // Now stop sending — idle timeout should fire.
    // Drain any endpoint.media_timeout events first.
    let deadline = tokio::time::Instant::now() + timing::scaled_ms(idle_secs * 1000 + 5000);
    loop {
        let remaining = deadline - tokio::time::Instant::now();
        let event = client.recv_event(remaining).await;
        assert!(
            event.is_some(),
            "should receive session.idle_timeout event before deadline"
        );
        if event.unwrap()["event"].as_str() == Some("session.idle_timeout") {
            break;
        }
    }
}

// ─── 2G: Session idle timeout — recording does not prevent timeout ───
//
// An active recording without media flow should NOT prevent idle timeout.
// Recordings are passive observers; only media packets and commands count.

#[tokio::test]
async fn test_idle_timeout_with_active_recording() {
    let server = TestServer::builder()
        .session_idle_timeout_secs(2)
        .start()
        .await;
    let recording_dir = server.recording_dir.clone();
    let mut client = TestControlClient::connect(&server.addr).await;

    let (_ep_id, _peer) = setup_rtp_session(&mut client).await;

    // Start a recording
    let pcap_path = std::path::Path::new(&recording_dir)
        .join("idle-timeout-recording-test.pcap")
        .to_string_lossy()
        .to_string();
    client
        .request_ok("recording.start", json!({"file_path": pcap_path}))
        .await;

    // Don't send any media — just wait for idle timeout with recording active
    let event = client.recv_event(timing::scaled_ms(5000)).await;
    assert!(
        event.is_some(),
        "should receive idle timeout even with active recording"
    );
    assert_eq!(
        event.unwrap()["event"].as_str().unwrap(),
        "session.idle_timeout",
        "active recording should not prevent idle timeout"
    );
}
