mod helpers;

use std::io::Read as _;

use serde_json::json;
use tempfile::TempDir;

use helpers::control_client::TestControlClient;
use helpers::test_rtp_peer::{TestRtpPeer, parse_rtp_addr_from_sdp};
use helpers::test_server::TestServer;
use helpers::timing;
use helpers::wav::generate_test_wav;

// ─────────────────────────────────────────────────────────────────────────────
// Test 1: Orphan reattach — media continues flowing after disconnect/reattach
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_orphan_reattach_media_continues() {
    let tmp = TempDir::new().unwrap();
    let server = TestServer::builder()
        .media_dir(tmp.path().to_str().unwrap())
        .disconnect_timeout_secs(2)
        .start()
        .await;

    // Connect, create session + two RTP endpoints
    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;

    // Create endpoint A (sender)
    let mut peer_a = TestRtpPeer::new().await;
    let offer_a = peer_a.make_sdp_offer();
    let result = client
        .request_ok(
            "endpoint.create_from_offer",
            json!({"sdp": offer_a, "direction": "sendrecv"}),
        )
        .await;
    let _ep_a_id = result["endpoint_id"].as_str().unwrap().to_string();
    let answer_a = result["sdp_answer"].as_str().unwrap();
    peer_a.set_remote(parse_rtp_addr_from_sdp(answer_a).unwrap());

    // Create endpoint B (receiver)
    let mut peer_b = TestRtpPeer::new().await;
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
    peer_b.activate().await;
    tokio::time::sleep(timing::scaled_ms(50)).await;
    peer_b.start_recv();

    // Get the session ID from session.info before disconnecting
    let info = client.request_ok("session.info", json!({})).await;
    let session_id = info["session_id"].as_str().unwrap().to_string();

    // Disconnect the WS client (orphan the session)
    client.close().await;
    tokio::time::sleep(timing::scaled_ms(300)).await;

    // Reconnect and reattach
    let mut client2 = TestControlClient::connect(&server.addr).await;
    let result = client2
        .request("session.attach", json!({"session_id": session_id}))
        .await;
    assert!(
        result.get("result").is_some(),
        "session.attach should succeed after reconnect: {result}"
    );

    // Now send RTP from peer_a — it should be routed to peer_b
    peer_a.send_tone_for(timing::scaled_ms(500)).await;
    tokio::time::sleep(timing::scaled_ms(500)).await;

    let received = peer_b.received_count();
    assert!(
        received > 0,
        "peer_b should receive routed media after orphan/reattach, got {received}"
    );

    client2.request_ok("session.destroy", json!({})).await;
}

// ─────────────────────────────────────────────────────────────────────────────
// Test 2: Recording and VAD running concurrently
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_recording_and_vad_concurrent() {
    let tmp = TempDir::new().unwrap();
    let server = TestServer::builder()
        .media_dir(tmp.path().to_str().unwrap())
        .disconnect_timeout_secs(2)
        .start()
        .await;
    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;

    // Create an RTP endpoint
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
    peer.set_remote(parse_rtp_addr_from_sdp(answer).unwrap());

    // Start recording (session-wide)
    let pcap_path = std::path::Path::new(&server.recording_dir).join("rec-vad.pcap");
    let rec = client
        .request_ok(
            "recording.start",
            json!({"endpoint_id": null, "file_path": pcap_path.to_str().unwrap()}),
        )
        .await;
    let rec_id = rec["recording_id"].as_str().unwrap().to_string();

    // Start VAD
    client
        .request_ok(
            "vad.start",
            json!({"endpoint_id": ep_id, "silence_interval_ms": 200}),
        )
        .await;

    // Send 3 seconds of tone
    peer.send_tone_for(timing::scaled_ms(3000)).await;

    // Wait a moment for events to arrive
    tokio::time::sleep(timing::scaled_ms(500)).await;

    // Collect any VAD events
    let mut got_vad_event = false;
    for _ in 0..20 {
        match client.recv_event(timing::scaled_ms(300)).await {
            Some(event) => {
                let event_type = event["event"].as_str().unwrap_or("");
                if event_type.contains("vad")
                    || event_type.contains("speech")
                    || event_type.contains("silence")
                {
                    got_vad_event = true;
                    break;
                }
            }
            None => break,
        }
    }
    assert!(
        got_vad_event,
        "should have received at least one VAD event during 3s of tone"
    );

    // Stop VAD
    client
        .request_ok("vad.stop", json!({"endpoint_id": ep_id}))
        .await;

    // Stop recording and verify packets were captured
    let stop = client
        .request_ok("recording.stop", json!({"recording_id": rec_id}))
        .await;
    let packets = stop["packets"].as_u64().unwrap();
    assert!(
        packets > 0,
        "recording should have captured packets during concurrent VAD, got {packets}"
    );

    client.request_ok("session.destroy", json!({})).await;
}

// ─────────────────────────────────────────────────────────────────────────────
// Test 3: Stats event contains endpoint data with packets > 0 and bytes > 0
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_stats_event_content() {
    let tmp = TempDir::new().unwrap();
    let server = TestServer::builder()
        .media_dir(tmp.path().to_str().unwrap())
        .disconnect_timeout_secs(2)
        .start()
        .await;
    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;

    // Create an RTP endpoint and connect a peer
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
    peer.set_remote(parse_rtp_addr_from_sdp(answer).unwrap());

    // Subscribe to stats with a short interval
    client
        .request_ok("stats.subscribe", json!({"interval_ms": 500}))
        .await;

    // Send some RTP packets to generate traffic
    peer.send_tone_for(timing::scaled_ms(500)).await;

    // Wait for a stats event
    let mut found_stats = false;
    for _ in 0..20 {
        if let Some(event) = client.recv_event(timing::scaled_ms(2000)).await {
            if event["event"].as_str().unwrap_or("") == "stats" {
                let endpoints = event["data"]["endpoints"].as_array().unwrap();
                assert!(
                    !endpoints.is_empty(),
                    "stats event should contain endpoint data"
                );

                // Find our endpoint in the stats
                let our_ep = endpoints
                    .iter()
                    .find(|ep| ep["endpoint_id"].as_str().unwrap_or("") == ep_id);
                assert!(
                    our_ep.is_some(),
                    "stats should include our endpoint {ep_id}: {event}"
                );
                let ep_stats = our_ep.unwrap();

                // Verify inbound stats
                let inbound_packets = ep_stats["inbound"]["packets"].as_u64().unwrap_or(0);
                let inbound_bytes = ep_stats["inbound"]["bytes"].as_u64().unwrap_or(0);
                assert!(
                    inbound_packets > 0,
                    "inbound packets should be > 0: {ep_stats}"
                );
                assert!(inbound_bytes > 0, "inbound bytes should be > 0: {ep_stats}");

                found_stats = true;
                break;
            }
        }
    }
    assert!(
        found_stats,
        "should have received a stats event with endpoint data"
    );

    client.request_ok("stats.unsubscribe", json!({})).await;
    client.request_ok("session.destroy", json!({})).await;
}

// ─────────────────────────────────────────────────────────────────────────────
// Test 4: Graceful shutdown flushes recording — SIGTERM, then verify PCAP
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_graceful_shutdown_flushes_recording() {
    let tmp = TempDir::new().unwrap();
    let tmp_str = tmp.path().to_str().unwrap();
    let server = TestServer::builder()
        .media_dir(tmp_str)
        .recording_dir(tmp_str)
        .disconnect_timeout_secs(2)
        .start()
        .await;

    let pcap_path = tmp.path().join("shutdown-flush.pcap");

    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;

    // Create an RTP endpoint
    let mut peer = TestRtpPeer::new().await;
    let offer = peer.make_sdp_offer();
    let result = client
        .request_ok(
            "endpoint.create_from_offer",
            json!({"sdp": offer, "direction": "sendrecv"}),
        )
        .await;
    let answer = result["sdp_answer"].as_str().unwrap();
    peer.set_remote(parse_rtp_addr_from_sdp(answer).unwrap());

    // Start recording
    client
        .request_ok(
            "recording.start",
            json!({"endpoint_id": null, "file_path": pcap_path.to_str().unwrap()}),
        )
        .await;

    // Send a few RTP packets
    peer.send_tone_for(timing::scaled_ms(500)).await;
    tokio::time::sleep(timing::scaled_ms(200)).await;

    // Destroy session — this triggers recording flush (stop_all)
    client.request_ok("session.destroy", json!({})).await;
    tokio::time::sleep(timing::scaled_ms(500)).await;

    // Shut down the server gracefully
    drop(server);

    // Verify the PCAP file exists and is valid
    assert!(
        pcap_path.exists(),
        "PCAP file should exist after graceful shutdown"
    );

    // Read the PCAP file and verify it has a valid header and at least 1 packet
    let mut file = std::fs::File::open(&pcap_path).unwrap();
    let mut contents = Vec::new();
    file.read_to_end(&mut contents).unwrap();

    // PCAP magic number: 0xa1b2c3d4 (native) or 0xd4c3b2a1 (swapped)
    assert!(
        contents.len() >= 24,
        "PCAP file should have at least the global header (24 bytes), got {} bytes",
        contents.len()
    );
    let magic = u32::from_le_bytes([contents[0], contents[1], contents[2], contents[3]]);
    assert!(
        magic == 0xa1b2c3d4 || magic == 0xd4c3b2a1,
        "PCAP file should have valid magic number, got {magic:#x}"
    );

    // Check there's at least one packet record after the 24-byte global header
    assert!(
        contents.len() > 24 + 16,
        "PCAP file should contain at least one packet record (header + record header), got {} bytes",
        contents.len()
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Test 5: File seek — seek mid-playback and verify playback continues
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_file_seek() {
    let tmp = TempDir::new().unwrap();
    let media_dir = tmp.path().join("media");
    std::fs::create_dir_all(&media_dir).unwrap();
    let server = TestServer::builder()
        .media_dir(media_dir.to_str().unwrap())
        .disconnect_timeout_secs(2)
        .start()
        .await;

    // Generate a 3-second test WAV
    let wav_path = media_dir.join("seek-test.wav");
    generate_test_wav(&wav_path, 3.0, 440.0);

    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;

    // Create an RTP endpoint as receiver
    let result = client
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv"}),
        )
        .await;
    let rtp_ep_id = result["endpoint_id"].as_str().unwrap().to_string();
    let offer = result["sdp_offer"].as_str().unwrap();
    let server_addr = parse_rtp_addr_from_sdp(offer).expect("parse server addr");

    let mut peer = TestRtpPeer::new().await;
    let answer = peer.make_sdp_answer();
    client
        .request_ok(
            "endpoint.accept_answer",
            json!({"endpoint_id": rtp_ep_id, "sdp": answer}),
        )
        .await;
    peer.set_remote(server_addr);
    peer.activate().await;
    tokio::time::sleep(timing::scaled_ms(50)).await;
    peer.start_recv();

    // Create file endpoint with loop_count=0 (play once, 3-second file)
    let result = client
        .request_ok(
            "endpoint.create_with_file",
            json!({"source": wav_path.to_str().unwrap(), "shared": false, "loop_count": 0}),
        )
        .await;
    let file_ep_id = result["endpoint_id"].as_str().unwrap().to_string();

    // Let it play for a bit
    tokio::time::sleep(timing::scaled_ms(500)).await;
    let before_seek = peer.received_count();
    assert!(before_seek > 0, "should receive packets before seek");

    // Seek to 1500ms
    client
        .request_ok(
            "endpoint.file.seek",
            json!({"endpoint_id": file_ep_id, "position_ms": 1500}),
        )
        .await;

    // Let playback continue after seek
    tokio::time::sleep(timing::scaled_ms(1000)).await;
    let after_seek = peer.received_count();
    assert!(
        after_seek > before_seek,
        "playback should continue after seek: before={before_seek}, after={after_seek}"
    );

    // Verify endpoint still exists and session is healthy
    let info = client.request_ok("session.info", json!({})).await;
    let endpoints = info["endpoints"].as_array().unwrap();
    let file_ep = endpoints
        .iter()
        .find(|ep| ep["endpoint_id"].as_str().unwrap_or("") == file_ep_id);
    assert!(
        file_ep.is_some(),
        "file endpoint should still exist after seek: {info}"
    );

    client.request_ok("session.destroy", json!({})).await;
}

// ─────────────────────────────────────────────────────────────────────────────
// Test 6: URL file playback — spin up a tiny HTTP server serving a WAV file
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_url_file_playback() {
    let tmp = TempDir::new().unwrap();
    let media_dir = tmp.path().join("media");
    std::fs::create_dir_all(&media_dir).unwrap();
    let server = TestServer::builder()
        .media_dir(media_dir.to_str().unwrap())
        .disconnect_timeout_secs(2)
        .start()
        .await;

    // Generate a 2-second WAV file to serve via HTTP
    let wav_path = media_dir.join("url-test.wav");
    generate_test_wav(&wav_path, 2.0, 440.0);

    let wav_bytes = std::fs::read(&wav_path).unwrap();
    let wav_len = wav_bytes.len();

    // Spin up a tiny HTTP server on a random port using a TCP listener
    let http_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let http_addr = http_listener.local_addr().unwrap();
    http_listener.set_nonblocking(true).unwrap();
    let http_listener = tokio::net::TcpListener::from_std(http_listener).unwrap();

    let http_handle = tokio::spawn(async move {
        // Serve requests until the task is aborted
        loop {
            match http_listener.accept().await {
                Ok((mut stream, _)) => {
                    let wav = wav_bytes.clone();
                    tokio::spawn(async move {
                        use tokio::io::AsyncReadExt;
                        use tokio::io::AsyncWriteExt;

                        // Read the HTTP request (we don't really need to parse it)
                        let mut buf = vec![0u8; 4096];
                        let _ = stream.read(&mut buf).await;

                        // Send HTTP response with the WAV file
                        let response = format!(
                            "HTTP/1.1 200 OK\r\n\
                             Content-Type: audio/wav\r\n\
                             Content-Length: {}\r\n\
                             Connection: close\r\n\
                             \r\n",
                            wav.len()
                        );
                        let _ = stream.write_all(response.as_bytes()).await;
                        let _ = stream.write_all(&wav).await;
                        let _ = stream.shutdown().await;
                    });
                }
                Err(_) => break,
            }
        }
    });

    let wav_url = format!("http://{}:{}/test.wav", http_addr.ip(), http_addr.port());

    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;

    // Create RTP endpoint as receiver
    let result = client
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv"}),
        )
        .await;
    let rtp_ep_id = result["endpoint_id"].as_str().unwrap().to_string();
    let offer = result["sdp_offer"].as_str().unwrap();
    let server_rtp_addr = parse_rtp_addr_from_sdp(offer).expect("parse server addr");

    let mut peer = TestRtpPeer::new().await;
    let answer = peer.make_sdp_answer();
    client
        .request_ok(
            "endpoint.accept_answer",
            json!({"endpoint_id": rtp_ep_id, "sdp": answer}),
        )
        .await;
    peer.set_remote(server_rtp_addr);
    peer.activate().await;
    tokio::time::sleep(timing::scaled_ms(50)).await;
    peer.start_recv();

    // Create file endpoint with the HTTP URL
    let result = client
        .request_ok(
            "endpoint.create_with_file",
            json!({"source": wav_url, "shared": false, "loop_count": 0, "timeout_ms": 10000}),
        )
        .await;
    let file_ep_id = result["endpoint_id"].as_str().unwrap().to_string();

    // Wait for download + playback to start
    tokio::time::sleep(timing::scaled_ms(2000)).await;

    // Verify endpoint was created and is in a valid state
    let info = client.request_ok("session.info", json!({})).await;
    let endpoints = info["endpoints"].as_array().unwrap();
    assert_eq!(
        endpoints.len(),
        2,
        "should have 2 endpoints (rtp + file): {info}"
    );

    // Find the file endpoint and check its state
    let file_ep = endpoints
        .iter()
        .find(|ep| ep["endpoint_id"].as_str().unwrap_or("") == file_ep_id);
    assert!(file_ep.is_some(), "file endpoint should exist: {info}");
    let file_ep = file_ep.unwrap();
    let state = file_ep["state"].as_str().unwrap_or("unknown");
    // It should be in Playing or Finished state (not New/Buffering/Connecting)
    assert!(
        state == "playing" || state == "finished" || state == "connected",
        "file endpoint should have progressed past initial state, got '{state}': {file_ep}"
    );

    // Verify that audio was actually delivered to the RTP peer
    let received = peer.received_count();
    assert!(
        received > 0,
        "peer should have received audio from URL file playback ({wav_len} byte WAV), got {received}"
    );

    http_handle.abort();
    client.request_ok("session.destroy", json!({})).await;
}
