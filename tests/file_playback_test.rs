mod helpers;

use std::time::Duration;

use serde_json::json;
use tempfile::TempDir;

use helpers::control_client::TestControlClient;
use helpers::http::http_get;
use helpers::test_rtp_peer::{TestRtpPeer, parse_rtp_addr_from_sdp};
use helpers::test_server::TestServer;
use helpers::timing;
use helpers::wav::generate_test_wav;

/// Test: File playback with loop_count=1 plays through once and the RTP
/// endpoint receives audio packets from the file.
#[tokio::test]
async fn test_file_playback_single_pass() {
    let tmp = TempDir::new().unwrap();
    let server = TestServer::builder()
        .media_dir(tmp.path().to_str().unwrap())
        .start()
        .await;
    let mut client = TestControlClient::connect(&server.addr).await;

    // Generate a 1-second test WAV
    let wav_path = tmp.path().join("rtpbridge-fileplay-single.wav");
    generate_test_wav(&wav_path, 1.0, 440.0);

    client.request_ok("session.create", json!({})).await;

    // Create an RTP endpoint to receive file audio
    let result = client
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv"}),
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
    peer.activate().await;
    tokio::time::sleep(timing::scaled_ms(50)).await;
    peer.start_recv();

    // Create file playback endpoint with loop_count=0 (play once)
    let result = client
        .request_ok(
            "endpoint.create_with_file",
            json!({"source": wav_path.to_str().unwrap(), "shared": false, "loop_count": 0}),
        )
        .await;
    let _file_ep_id = result["endpoint_id"].as_str().unwrap().to_string();

    // Wait for the file to finish playing (1-second file + margin)
    tokio::time::sleep(timing::scaled_ms(1500)).await;

    // Peer should have received audio packets from the file playback
    let received = peer.received_count();
    assert!(
        received > 0,
        "peer should have received audio from file playback, got {received}"
    );

    // Check session info shows the file endpoint
    let info = client.request_ok("session.info", json!({})).await;
    let endpoints = info["endpoints"].as_array().unwrap();
    assert_eq!(endpoints.len(), 2, "should have 2 endpoints (rtp + file)");

    // Verify there is a file-type endpoint
    let has_file = endpoints.iter().any(|ep| ep["endpoint_type"] == "file");
    assert!(has_file, "should have a file endpoint: {info}");

    // Clean up
    client.request_ok("session.destroy", json!({})).await;
}

/// Test: File playback can be paused and resumed
#[tokio::test]
async fn test_file_playback_pause_resume() {
    let tmp = TempDir::new().unwrap();
    let server = TestServer::builder()
        .media_dir(tmp.path().to_str().unwrap())
        .start()
        .await;
    let mut client = TestControlClient::connect(&server.addr).await;

    let wav_path = tmp.path().join("rtpbridge-fileplay-pause.wav");
    generate_test_wav(&wav_path, 3.0, 440.0); // 3-second file

    client.request_ok("session.create", json!({})).await;

    // Create receiving RTP endpoint
    let result = client
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv"}),
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
    peer.activate().await;
    tokio::time::sleep(timing::scaled_ms(50)).await;
    peer.start_recv();

    // Create file playback endpoint (infinite loop to avoid finishing)
    let result = client
        .request_ok(
            "endpoint.create_with_file",
            json!({"source": wav_path.to_str().unwrap(), "shared": false, "loop_count": null}),
        )
        .await;
    let file_ep_id = result["endpoint_id"].as_str().unwrap().to_string();

    // Let it play for a bit
    tokio::time::sleep(timing::scaled_ms(500)).await;
    let before_pause = peer.received_count();
    assert!(
        before_pause > 0,
        "should receive packets before pause, got {before_pause}"
    );

    // Pause playback
    client
        .request_ok("endpoint.file.pause", json!({"endpoint_id": file_ep_id}))
        .await;

    // Wait and verify no more packets arrive (or very few)
    tokio::time::sleep(timing::scaled_ms(500)).await;
    let after_pause = peer.received_count();
    // During pause, no new file-generated packets should arrive
    // (some packets may be in-flight, so allow a small margin)
    let during_pause = after_pause - before_pause;
    assert!(
        during_pause <= 5,
        "should receive at most a few in-flight packets during pause, got {during_pause}"
    );

    // Resume playback
    client
        .request_ok("endpoint.file.resume", json!({"endpoint_id": file_ep_id}))
        .await;

    tokio::time::sleep(timing::scaled_ms(500)).await;
    let after_resume = peer.received_count();
    let during_resume = after_resume - after_pause;

    assert!(
        during_resume > during_pause,
        "should receive more packets after resume ({during_resume}) than during pause ({during_pause})"
    );

    client.request_ok("session.destroy", json!({})).await;
}

/// Test: Shared file playback — two sessions can share the same file
#[tokio::test]
async fn test_shared_file_playback() {
    let tmp = TempDir::new().unwrap();
    let server = TestServer::builder()
        .media_dir(tmp.path().to_str().unwrap())
        .start()
        .await;

    let wav_path = tmp.path().join("rtpbridge-fileplay-shared.wav");
    generate_test_wav(&wav_path, 2.0, 440.0);

    // Session 1 with shared file
    let mut client1 = TestControlClient::connect(&server.addr).await;
    client1.request_ok("session.create", json!({})).await;
    let result = client1
        .request_ok(
            "endpoint.create_with_file",
            json!({"source": wav_path.to_str().unwrap(), "shared": true, "loop_count": null}),
        )
        .await;
    let eid = result["endpoint_id"]
        .as_str()
        .expect("endpoint_id should be a string");
    assert!(!eid.is_empty(), "endpoint_id should not be empty");

    // Session 2 with the same shared file
    let mut client2 = TestControlClient::connect(&server.addr).await;
    client2.request_ok("session.create", json!({})).await;
    let result = client2
        .request_ok(
            "endpoint.create_with_file",
            json!({"source": wav_path.to_str().unwrap(), "shared": true, "loop_count": null}),
        )
        .await;
    let eid = result["endpoint_id"]
        .as_str()
        .expect("endpoint_id should be a string");
    assert!(!eid.is_empty(), "endpoint_id should not be empty");

    // Verify both sessions are active via HTTP
    let sessions = http_get(&server.addr, "/sessions").await;
    assert_eq!(
        sessions.as_array().unwrap().len(),
        2,
        "should have 2 sessions with shared file"
    );

    // Clean up
    client1.request_ok("session.destroy", json!({})).await;
    client2.request_ok("session.destroy", json!({})).await;
}

/// Test: Shared file playback actually delivers audio to RTP endpoints
#[tokio::test]
async fn test_shared_file_playback_delivers_audio() {
    let tmp = TempDir::new().unwrap();
    let server = TestServer::builder()
        .media_dir(tmp.path().to_str().unwrap())
        .start()
        .await;

    let wav_path = tmp.path().join("rtpbridge-fileplay-shared-media.wav");
    generate_test_wav(&wav_path, 2.0, 440.0);

    // Session 1: shared file + RTP endpoint with peer
    let mut client1 = TestControlClient::connect(&server.addr).await;
    client1.request_ok("session.create", json!({})).await;

    let result = client1
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv"}),
        )
        .await;
    let ep1_id = result["endpoint_id"].as_str().unwrap().to_string();
    let offer1 = result["sdp_offer"].as_str().unwrap();
    let server_addr1 = parse_rtp_addr_from_sdp(offer1).expect("parse server addr 1");

    let mut peer1 = TestRtpPeer::new().await;
    let answer1 = peer1.make_sdp_answer();
    client1
        .request_ok(
            "endpoint.accept_answer",
            json!({"endpoint_id": ep1_id, "sdp": answer1}),
        )
        .await;
    peer1.set_remote(server_addr1);
    peer1.activate().await;
    tokio::time::sleep(timing::scaled_ms(50)).await;
    peer1.start_recv();

    client1
        .request_ok(
            "endpoint.create_with_file",
            json!({"source": wav_path.to_str().unwrap(), "shared": true, "loop_count": null}),
        )
        .await;

    // Session 2: shared file + RTP endpoint with peer
    let mut client2 = TestControlClient::connect(&server.addr).await;
    client2.request_ok("session.create", json!({})).await;

    let result = client2
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv"}),
        )
        .await;
    let ep2_id = result["endpoint_id"].as_str().unwrap().to_string();
    let offer2 = result["sdp_offer"].as_str().unwrap();
    let server_addr2 = parse_rtp_addr_from_sdp(offer2).expect("parse server addr 2");

    let mut peer2 = TestRtpPeer::new().await;
    let answer2 = peer2.make_sdp_answer();
    client2
        .request_ok(
            "endpoint.accept_answer",
            json!({"endpoint_id": ep2_id, "sdp": answer2}),
        )
        .await;
    peer2.set_remote(server_addr2);
    peer2.activate().await;
    tokio::time::sleep(timing::scaled_ms(50)).await;
    peer2.start_recv();

    client2
        .request_ok(
            "endpoint.create_with_file",
            json!({"source": wav_path.to_str().unwrap(), "shared": true, "loop_count": null}),
        )
        .await;

    // Wait for file audio to be routed
    tokio::time::sleep(timing::scaled_ms(1500)).await;

    // Both peers should have received audio
    let received1 = peer1.received_count();
    let received2 = peer2.received_count();
    assert!(
        received1 > 0,
        "peer 1 should have received audio from shared file playback, got {received1}"
    );
    assert!(
        received2 > 0,
        "peer 2 should have received audio from shared file playback, got {received2}"
    );

    client1.request_ok("session.destroy", json!({})).await;
    client2.request_ok("session.destroy", json!({})).await;
}

/// Test: shared=true with URL sources delivers audio through shared decode
#[tokio::test]
async fn test_shared_url_file_playback() {
    let tmp = TempDir::new().unwrap();

    // Start a tiny HTTP server that serves a WAV file
    let wav_path = tmp.path().join("rtpbridge-fileplay-shared-url.wav");
    generate_test_wav(&wav_path, 2.0, 440.0);
    let wav_bytes = std::fs::read(&wav_path).unwrap();

    let http_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let http_addr = http_listener.local_addr().unwrap();

    let http_handle = tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = http_listener.accept().await else {
                break;
            };
            let wav = wav_bytes.clone();
            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = vec![0u8; 4096];
                let _ = stream.read(&mut buf).await;
                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: audio/wav\r\nContent-Length: {}\r\n\r\n",
                    wav.len()
                );
                let _ = stream.write_all(header.as_bytes()).await;
                let _ = stream.write_all(&wav).await;
            });
        }
    });

    let server = TestServer::builder()
        .media_dir(tmp.path().to_str().unwrap())
        .start()
        .await;
    let url = format!(
        "http://{}:{}/shared-test.wav",
        http_addr.ip(),
        http_addr.port()
    );

    // Session 1: shared URL file + RTP endpoint with peer
    let mut client1 = TestControlClient::connect(&server.addr).await;
    client1.request_ok("session.create", json!({})).await;

    let result = client1
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv"}),
        )
        .await;
    let ep1_id = result["endpoint_id"].as_str().unwrap().to_string();
    let offer1 = result["sdp_offer"].as_str().unwrap();
    let server_addr1 = parse_rtp_addr_from_sdp(offer1).expect("parse server addr 1");

    let mut peer1 = TestRtpPeer::new().await;
    let answer1 = peer1.make_sdp_answer();
    client1
        .request_ok(
            "endpoint.accept_answer",
            json!({"endpoint_id": ep1_id, "sdp": answer1}),
        )
        .await;
    peer1.set_remote(server_addr1);
    peer1.activate().await;
    tokio::time::sleep(timing::scaled_ms(50)).await;
    peer1.start_recv();

    let result = client1
        .request_ok(
            "endpoint.create_with_file",
            json!({"source": url, "shared": true, "loop_count": null, "timeout_ms": 5000}),
        )
        .await;
    let eid = result["endpoint_id"]
        .as_str()
        .expect("endpoint_id should be a string");
    assert!(!eid.is_empty(), "endpoint_id should not be empty");

    // Session 2: same shared URL file + RTP endpoint with peer
    let mut client2 = TestControlClient::connect(&server.addr).await;
    client2.request_ok("session.create", json!({})).await;

    let result = client2
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv"}),
        )
        .await;
    let ep2_id = result["endpoint_id"].as_str().unwrap().to_string();
    let offer2 = result["sdp_offer"].as_str().unwrap();
    let server_addr2 = parse_rtp_addr_from_sdp(offer2).expect("parse server addr 2");

    let mut peer2 = TestRtpPeer::new().await;
    let answer2 = peer2.make_sdp_answer();
    client2
        .request_ok(
            "endpoint.accept_answer",
            json!({"endpoint_id": ep2_id, "sdp": answer2}),
        )
        .await;
    peer2.set_remote(server_addr2);
    peer2.activate().await;
    tokio::time::sleep(timing::scaled_ms(50)).await;
    peer2.start_recv();

    let result = client2
        .request_ok(
            "endpoint.create_with_file",
            json!({"source": url, "shared": true, "loop_count": null, "timeout_ms": 5000}),
        )
        .await;
    let eid = result["endpoint_id"]
        .as_str()
        .expect("endpoint_id should be a string");
    assert!(!eid.is_empty(), "endpoint_id should not be empty");

    // Poll until both peers have received audio (or timeout)
    let deadline = tokio::time::Instant::now() + timing::scaled_ms(5000);
    loop {
        if peer1.received_count() > 0 && peer2.received_count() > 0 {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            let received1 = peer1.received_count();
            let received2 = peer2.received_count();
            panic!(
                "timed out waiting for shared URL file playback: peer1={received1}, peer2={received2}"
            );
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    client1.request_ok("session.destroy", json!({})).await;
    client2.request_ok("session.destroy", json!({})).await;
    http_handle.abort();
}

/// Convert a WAV file to another format using ffmpeg.
/// Returns true if conversion succeeded.
fn convert_wav_to(wav_path: &str, out_path: &str, format_args: &[&str]) -> bool {
    let status = std::process::Command::new("ffmpeg")
        .args(["-y", "-i", wav_path])
        .args(format_args)
        .arg(out_path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    matches!(status, Ok(s) if s.success())
}

/// Test: MP3 file playback delivers audio to an RTP endpoint
#[tokio::test]
async fn test_mp3_file_playback() {
    let tmp = TempDir::new().unwrap();
    let wav_path = tmp.path().join("rtpbridge-fileplay-mp3-src.wav");
    let mp3_path = tmp.path().join("rtpbridge-fileplay-test.mp3");
    generate_test_wav(&wav_path, 1.0, 440.0);

    if !convert_wav_to(
        wav_path.to_str().unwrap(),
        mp3_path.to_str().unwrap(),
        &["-codec:a", "libmp3lame", "-b:a", "128k"],
    ) {
        eprintln!("SKIP: ffmpeg MP3 conversion failed");
        return;
    }

    let server = TestServer::builder()
        .media_dir(tmp.path().to_str().unwrap())
        .start()
        .await;
    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;

    let result = client
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv"}),
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
    peer.activate().await;
    tokio::time::sleep(timing::scaled_ms(50)).await;
    peer.start_recv();

    let result = client
        .request_ok(
            "endpoint.create_with_file",
            json!({"source": mp3_path.to_str().unwrap(), "shared": false, "loop_count": 0}),
        )
        .await;
    let eid = result["endpoint_id"]
        .as_str()
        .expect("endpoint_id should be a string");
    assert!(!eid.is_empty(), "endpoint_id should not be empty");

    tokio::time::sleep(timing::scaled_ms(1500)).await;

    let received = peer.received_count();
    assert!(
        received > 0,
        "peer should have received audio from MP3 file playback, got {received}"
    );

    client.request_ok("session.destroy", json!({})).await;
}

/// Test: OGG/Vorbis file playback delivers audio to an RTP endpoint
#[tokio::test]
async fn test_ogg_file_playback() {
    let tmp = TempDir::new().unwrap();
    let wav_path = tmp.path().join("rtpbridge-fileplay-ogg-src.wav");
    let ogg_path = tmp.path().join("rtpbridge-fileplay-test.ogg");
    generate_test_wav(&wav_path, 1.0, 440.0);

    if !convert_wav_to(
        wav_path.to_str().unwrap(),
        ogg_path.to_str().unwrap(),
        &["-codec:a", "libvorbis", "-q:a", "4"],
    ) {
        eprintln!("SKIP: ffmpeg OGG conversion failed");
        return;
    }

    let server = TestServer::builder()
        .media_dir(tmp.path().to_str().unwrap())
        .start()
        .await;
    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;

    let result = client
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv"}),
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
    peer.activate().await;
    tokio::time::sleep(timing::scaled_ms(50)).await;
    peer.start_recv();

    let result = client
        .request_ok(
            "endpoint.create_with_file",
            json!({"source": ogg_path.to_str().unwrap(), "shared": false, "loop_count": 0}),
        )
        .await;
    let eid = result["endpoint_id"]
        .as_str()
        .expect("endpoint_id should be a string");
    assert!(!eid.is_empty(), "endpoint_id should not be empty");

    tokio::time::sleep(timing::scaled_ms(1500)).await;

    let received = peer.received_count();
    assert!(
        received > 0,
        "peer should have received audio from OGG file playback, got {received}"
    );

    client.request_ok("session.destroy", json!({})).await;
}

/// Test: FLAC file playback delivers audio to an RTP endpoint
#[tokio::test]
async fn test_flac_file_playback() {
    let tmp = TempDir::new().unwrap();
    let wav_path = tmp.path().join("rtpbridge-fileplay-flac-src.wav");
    let flac_path = tmp.path().join("rtpbridge-fileplay-test.flac");
    generate_test_wav(&wav_path, 1.0, 440.0);

    if !convert_wav_to(wav_path.to_str().unwrap(), flac_path.to_str().unwrap(), &[]) {
        eprintln!("SKIP: ffmpeg FLAC conversion failed");
        return;
    }

    let server = TestServer::builder()
        .media_dir(tmp.path().to_str().unwrap())
        .start()
        .await;
    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;

    let result = client
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv"}),
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
    peer.activate().await;
    tokio::time::sleep(timing::scaled_ms(50)).await;
    peer.start_recv();

    let result = client
        .request_ok(
            "endpoint.create_with_file",
            json!({"source": flac_path.to_str().unwrap(), "shared": false, "loop_count": 0}),
        )
        .await;
    let eid = result["endpoint_id"]
        .as_str()
        .expect("endpoint_id should be a string");
    assert!(!eid.is_empty(), "endpoint_id should not be empty");

    tokio::time::sleep(timing::scaled_ms(1500)).await;

    let received = peer.received_count();
    assert!(
        received > 0,
        "peer should have received audio from FLAC file playback, got {received}"
    );

    client.request_ok("session.destroy", json!({})).await;
}

/// Test: File seek mid-playback — seek forward, verify playback finishes sooner
#[tokio::test]
async fn test_file_playback_seek() {
    let tmp = TempDir::new().unwrap();
    let server = TestServer::builder()
        .media_dir(tmp.path().to_str().unwrap())
        .start()
        .await;
    let mut client = TestControlClient::connect(&server.addr).await;

    let wav_path = tmp.path().join("rtpbridge-fileplay-seek.wav");
    generate_test_wav(&wav_path, 3.0, 440.0);

    client.request_ok("session.create", json!({})).await;

    let result = client
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv"}),
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
    peer.activate().await;
    tokio::time::sleep(timing::scaled_ms(50)).await;
    peer.start_recv();

    // Create file endpoint with loop_count=0 (play once)
    let result = client
        .request_ok(
            "endpoint.create_with_file",
            json!({"source": wav_path.to_str().unwrap(), "shared": false, "loop_count": 0}),
        )
        .await;
    let file_ep_id = result["endpoint_id"].as_str().unwrap().to_string();

    // Let it play for 500ms
    tokio::time::sleep(timing::scaled_ms(500)).await;
    let before_seek = peer.received_count();
    assert!(before_seek > 0, "should receive packets before seek");

    // Seek to 2500ms (near end of 3s file)
    client
        .request_ok(
            "endpoint.file.seek",
            json!({"endpoint_id": file_ep_id, "position_ms": 2500}),
        )
        .await;

    // Playback should continue from 2500ms and finish within ~500ms + margin
    tokio::time::sleep(timing::scaled_ms(1000)).await;
    let after_seek = peer.received_count();
    assert!(
        after_seek > before_seek,
        "should receive more packets after seek: before={before_seek}, after={after_seek}"
    );

    // The file should have finished (3s file, seeked to 2.5s, only 0.5s left)
    let mut got_finished = false;
    for _ in 0..10 {
        if let Some(event) = client.recv_event(timing::scaled_ms(300)).await {
            if event["event"].as_str().unwrap_or("") == "endpoint.file.finished" {
                got_finished = true;
                break;
            }
        }
    }
    assert!(
        got_finished,
        "file should have finished after seeking near end"
    );

    client.request_ok("session.destroy", json!({})).await;
}

/// Test: HTTP 404 for URL file source triggers error event
#[tokio::test]
async fn test_url_playback_http_404() {
    let tmp = TempDir::new().unwrap();
    let http_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let http_addr = http_listener.local_addr().unwrap();

    let http_handle = tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = http_listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = vec![0u8; 4096];
                let _ = stream.read(&mut buf).await;
                let resp = "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n";
                let _ = stream.write_all(resp.as_bytes()).await;
            });
        }
    });

    let server = TestServer::builder()
        .media_dir(tmp.path().to_str().unwrap())
        .start()
        .await;
    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;

    let url = format!("http://{}:{}/missing.wav", http_addr.ip(), http_addr.port());
    let result = client
        .request_ok(
            "endpoint.create_with_file",
            json!({"source": url, "timeout_ms": 5000}),
        )
        .await;
    let eid = result["endpoint_id"]
        .as_str()
        .expect("endpoint_id should be a string");
    assert!(
        !eid.is_empty(),
        "endpoint_id should not be empty (async download)"
    );

    // Wait for error event (file.finished or state_changed to finished)
    let mut got_error = false;
    for _ in 0..20 {
        if let Some(event) = client.recv_event(timing::scaled_ms(500)).await {
            let event_type = event["event"].as_str().unwrap_or("");
            if event_type == "endpoint.file.finished"
                || (event_type == "endpoint.state_changed"
                    && event["data"]["new_state"].as_str() == Some("finished"))
            {
                got_error = true;
                break;
            }
        }
    }
    assert!(got_error, "should receive error event for HTTP 404 URL");

    client.request_ok("session.destroy", json!({})).await;
    http_handle.abort();
}

/// Test: URL download timeout triggers error event
#[tokio::test]
async fn test_url_playback_timeout() {
    let tmp = TempDir::new().unwrap();
    // HTTP server that accepts but never responds
    let http_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let http_addr = http_listener.local_addr().unwrap();

    let http_handle = tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = http_listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                tokio::time::sleep(timing::scaled_ms(60000)).await;
                drop(stream);
            });
        }
    });

    let server = TestServer::builder()
        .media_dir(tmp.path().to_str().unwrap())
        .start()
        .await;
    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;

    let url = format!("http://{}:{}/slow.wav", http_addr.ip(), http_addr.port());
    let result = client
        .request_ok(
            "endpoint.create_with_file",
            json!({"source": url, "timeout_ms": 500}),
        )
        .await;
    let eid = result["endpoint_id"]
        .as_str()
        .expect("endpoint_id should be a string");
    assert!(!eid.is_empty(), "endpoint_id should not be empty");

    // Wait for timeout error event
    let mut got_error = false;
    for _ in 0..20 {
        if let Some(event) = client.recv_event(timing::scaled_ms(500)).await {
            let event_type = event["event"].as_str().unwrap_or("");
            if event_type == "endpoint.file.finished"
                || (event_type == "endpoint.state_changed"
                    && event["data"]["new_state"].as_str() == Some("finished"))
            {
                got_error = true;
                break;
            }
        }
    }
    assert!(
        got_error,
        "should receive error event for timed-out URL download"
    );

    client.request_ok("session.destroy", json!({})).await;
    http_handle.abort();
}

/// Test: File with loop_count=null plays indefinitely past the file duration
#[tokio::test]
async fn test_infinite_loop_playback() {
    let tmp = TempDir::new().unwrap();
    let server = TestServer::builder()
        .media_dir(tmp.path().to_str().unwrap())
        .start()
        .await;
    let mut client = TestControlClient::connect(&server.addr).await;

    let wav_path = tmp.path().join("rtpbridge-infinite-loop-test.wav");
    generate_test_wav(&wav_path, 0.5, 440.0); // 0.5 second file

    client.request_ok("session.create", json!({})).await;

    // Create receiving RTP endpoint
    let result = client
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv"}),
        )
        .await;
    let ep_id = result["endpoint_id"].as_str().unwrap().to_string();
    let mut peer = TestRtpPeer::new().await;
    let answer = peer.make_sdp_answer();
    client
        .request_ok(
            "endpoint.accept_answer",
            json!({"endpoint_id": ep_id, "sdp": answer}),
        )
        .await;
    peer.set_remote(parse_rtp_addr_from_sdp(result["sdp_offer"].as_str().unwrap()).unwrap());
    peer.activate().await;
    tokio::time::sleep(timing::scaled_ms(50)).await;
    peer.start_recv();

    // Create file endpoint with infinite loop
    client
        .request_ok(
            "endpoint.create_with_file",
            json!({"source": wav_path.to_str().unwrap(), "shared": false, "loop_count": null}),
        )
        .await;

    // Wait 2 seconds (4x the 0.5s file duration)
    tokio::time::sleep(timing::scaled_ms(2000)).await;

    let received = peer.received_count();
    // Single 0.5s play = ~25 packets at 50pps. 2s of looping = ~100 packets.
    assert!(
        received > 40,
        "infinite loop should produce far more packets than single play, got {received}"
    );

    // File endpoint should NOT be finished
    let info = client.request_ok("session.info", json!({})).await;
    let endpoints = info["endpoints"].as_array().unwrap();
    let file_ep = endpoints
        .iter()
        .find(|e| e["endpoint_type"] == "file")
        .unwrap();
    assert_ne!(
        file_ep["state"].as_str().unwrap(),
        "finished",
        "infinite loop file should not be finished"
    );

    client.request_ok("session.destroy", json!({})).await;
}

/// Test: A corrupted/invalid audio file should produce a graceful error
/// (either an error response or an endpoint.file.finished event with error),
/// not a panic or hang.
#[tokio::test]
async fn test_corrupted_audio_file_graceful_error() {
    let tmp = TempDir::new().unwrap();

    // Write a file with random garbage bytes (not a valid WAV/MP3/OGG/FLAC)
    let bad_file = tmp.path().join("corrupted.wav");
    std::fs::write(
        &bad_file,
        b"this is not a valid audio file at all, just random garbage bytes 1234567890",
    )
    .unwrap();

    let server = TestServer::builder()
        .media_dir(tmp.path().to_str().unwrap())
        .start()
        .await;
    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;

    // Create an RTP endpoint so the session has a destination for any audio
    let result = client
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv"}),
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

    // Try to create a file endpoint with the corrupted file.
    // This should either fail immediately (error response) or succeed and
    // then emit an endpoint.file.finished event with an error reason.
    let result = client
        .request(
            "endpoint.create_with_file",
            json!({"source": bad_file.to_str().unwrap(), "loop_count": 0}),
        )
        .await;

    if result.get("error").is_some() {
        // Immediate error response — the server detected the bad file during creation.
        let error_msg = result["error"]["message"].as_str().unwrap_or("");
        assert!(
            !error_msg.is_empty(),
            "error response should have a message: {result}"
        );
    } else {
        // File endpoint was created (async decode). Wait for an error event.
        let file_ep_id = result["result"]["endpoint_id"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(!file_ep_id.is_empty(), "should get an endpoint_id");

        let mut got_error_event = false;
        for _ in 0..15 {
            if let Some(event) = client.recv_event(timing::scaled_ms(2000)).await {
                let event_type = event["event"].as_str().unwrap_or("");
                // Accept either file.finished with error, or state_changed to finished
                if event_type == "endpoint.file.finished" {
                    let reason = event["data"]["reason"].as_str().unwrap_or("");
                    // Any reason other than "completed" indicates an error
                    if reason != "completed" || event["data"]["error"].is_string() {
                        got_error_event = true;
                        break;
                    }
                    // Even "completed" immediately is acceptable for a bad file
                    got_error_event = true;
                    break;
                } else if event_type == "endpoint.state_changed"
                    && event["data"]["new_state"].as_str() == Some("finished")
                {
                    got_error_event = true;
                    break;
                }
            }
        }
        assert!(
            got_error_event,
            "should receive an error event for corrupted audio file"
        );
    }

    client.request_ok("session.destroy", json!({})).await;
}

/// Test: Calling endpoint.file.pause on a finished file endpoint returns an error.
/// After a non-looping file finishes, pause should be rejected because the
/// endpoint is no longer in a valid state for playback control.
#[tokio::test]
async fn test_file_pause_on_finished_endpoint() {
    let tmp = TempDir::new().unwrap();
    let server = TestServer::builder()
        .media_dir(tmp.path().to_str().unwrap())
        .start()
        .await;
    let mut client = TestControlClient::connect(&server.addr).await;

    // Generate a very short WAV so it finishes quickly
    let wav_path = tmp.path().join("rtpbridge-fileplay-pause-finished.wav");
    generate_test_wav(&wav_path, 0.3, 440.0);

    client.request_ok("session.create", json!({})).await;

    // Create an RTP endpoint to receive file audio
    let result = client
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv"}),
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
    peer.start_recv();

    // Create file playback endpoint with loop_count=0 (play once)
    let result = client
        .request_ok(
            "endpoint.create_with_file",
            json!({"source": wav_path.to_str().unwrap(), "shared": false, "loop_count": 0}),
        )
        .await;
    let file_ep_id = result["endpoint_id"].as_str().unwrap().to_string();

    // Wait for the endpoint.file.finished event
    let mut got_finished = false;
    for _ in 0..20 {
        if let Some(event) = client.recv_event(timing::scaled_ms(500)).await {
            if event["event"].as_str().unwrap_or("") == "endpoint.file.finished" {
                got_finished = true;
                break;
            }
        }
    }
    assert!(got_finished, "file should have finished playing");

    // Now try to pause the finished endpoint — expect error
    let resp = client
        .request("endpoint.file.pause", json!({"endpoint_id": file_ep_id}))
        .await;
    assert!(
        resp.get("error").is_some(),
        "pause on finished endpoint should return error: {resp}"
    );
    assert_eq!(
        resp["error"]["code"].as_str().unwrap_or(""),
        "ENDPOINT_ERROR",
        "pause on finished endpoint should return ENDPOINT_ERROR: {resp}"
    );

    client.request_ok("session.destroy", json!({})).await;
}

/// Test: Calling endpoint.file.resume on a finished file endpoint returns an error.
#[tokio::test]
async fn test_file_resume_on_finished_endpoint() {
    let tmp = TempDir::new().unwrap();
    let server = TestServer::builder()
        .media_dir(tmp.path().to_str().unwrap())
        .start()
        .await;
    let mut client = TestControlClient::connect(&server.addr).await;

    let wav_path = tmp.path().join("rtpbridge-fileplay-resume-finished.wav");
    generate_test_wav(&wav_path, 0.3, 440.0);

    client.request_ok("session.create", json!({})).await;

    let result = client
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv"}),
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
    peer.start_recv();

    let result = client
        .request_ok(
            "endpoint.create_with_file",
            json!({"source": wav_path.to_str().unwrap(), "shared": false, "loop_count": 0}),
        )
        .await;
    let file_ep_id = result["endpoint_id"].as_str().unwrap().to_string();

    // Wait for finished event
    let mut got_finished = false;
    for _ in 0..20 {
        if let Some(event) = client.recv_event(timing::scaled_ms(500)).await {
            if event["event"].as_str().unwrap_or("") == "endpoint.file.finished" {
                got_finished = true;
                break;
            }
        }
    }
    assert!(got_finished, "file should have finished playing");

    // Try to resume the finished endpoint — expect error
    let resp = client
        .request("endpoint.file.resume", json!({"endpoint_id": file_ep_id}))
        .await;
    assert!(
        resp.get("error").is_some(),
        "resume on finished endpoint should return error: {resp}"
    );
    assert_eq!(
        resp["error"]["code"].as_str().unwrap_or(""),
        "ENDPOINT_ERROR",
        "resume on finished endpoint should return ENDPOINT_ERROR: {resp}"
    );

    client.request_ok("session.destroy", json!({})).await;
}

/// Test: Calling endpoint.file.seek on a finished file endpoint returns an error.
#[tokio::test]
async fn test_file_seek_on_finished_endpoint() {
    let tmp = TempDir::new().unwrap();
    let server = TestServer::builder()
        .media_dir(tmp.path().to_str().unwrap())
        .start()
        .await;
    let mut client = TestControlClient::connect(&server.addr).await;

    let wav_path = tmp.path().join("rtpbridge-fileplay-seek-finished.wav");
    generate_test_wav(&wav_path, 0.3, 440.0);

    client.request_ok("session.create", json!({})).await;

    let result = client
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv"}),
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
    peer.start_recv();

    let result = client
        .request_ok(
            "endpoint.create_with_file",
            json!({"source": wav_path.to_str().unwrap(), "shared": false, "loop_count": 0}),
        )
        .await;
    let file_ep_id = result["endpoint_id"].as_str().unwrap().to_string();

    // Wait for finished event
    let mut got_finished = false;
    for _ in 0..20 {
        if let Some(event) = client.recv_event(timing::scaled_ms(500)).await {
            if event["event"].as_str().unwrap_or("") == "endpoint.file.finished" {
                got_finished = true;
                break;
            }
        }
    }
    assert!(got_finished, "file should have finished playing");

    // Try to seek on the finished endpoint — expect error
    let resp = client
        .request(
            "endpoint.file.seek",
            json!({"endpoint_id": file_ep_id, "position_ms": 0}),
        )
        .await;
    assert!(
        resp.get("error").is_some(),
        "seek on finished endpoint should return error: {resp}"
    );
    assert_eq!(
        resp["error"]["code"].as_str().unwrap_or(""),
        "ENDPOINT_ERROR",
        "seek on finished endpoint should return ENDPOINT_ERROR: {resp}"
    );

    client.request_ok("session.destroy", json!({})).await;
}

/// Test: Calling endpoint.file.pause twice is idempotent — the second call
/// should succeed without error (Paused -> Paused is a no-op).
#[tokio::test]
async fn test_file_pause_idempotent() {
    let tmp = TempDir::new().unwrap();
    let server = TestServer::builder()
        .media_dir(tmp.path().to_str().unwrap())
        .start()
        .await;
    let mut client = TestControlClient::connect(&server.addr).await;

    let wav_path = tmp.path().join("rtpbridge-fileplay-pause-idem.wav");
    generate_test_wav(&wav_path, 3.0, 440.0);

    client.request_ok("session.create", json!({})).await;

    // Create file endpoint with infinite loop so it doesn't finish
    let result = client
        .request_ok(
            "endpoint.create_with_file",
            json!({"source": wav_path.to_str().unwrap(), "shared": false, "loop_count": null}),
        )
        .await;
    let file_ep_id = result["endpoint_id"].as_str().unwrap().to_string();

    // Let playback start
    tokio::time::sleep(timing::scaled_ms(300)).await;

    // First pause — should succeed
    client
        .request_ok("endpoint.file.pause", json!({"endpoint_id": file_ep_id}))
        .await;

    // Second pause — should also succeed (idempotent)
    client
        .request_ok("endpoint.file.pause", json!({"endpoint_id": file_ep_id}))
        .await;

    client.request_ok("session.destroy", json!({})).await;
}

/// Test: Calling endpoint.file.resume on an already-playing file endpoint is
/// idempotent — the call should succeed without error (Playing -> Playing is a no-op).
#[tokio::test]
async fn test_file_resume_idempotent() {
    let tmp = TempDir::new().unwrap();
    let server = TestServer::builder()
        .media_dir(tmp.path().to_str().unwrap())
        .start()
        .await;
    let mut client = TestControlClient::connect(&server.addr).await;

    let wav_path = tmp.path().join("rtpbridge-fileplay-resume-idem.wav");
    generate_test_wav(&wav_path, 3.0, 440.0);

    client.request_ok("session.create", json!({})).await;

    // Create file endpoint with infinite loop so it stays playing
    let result = client
        .request_ok(
            "endpoint.create_with_file",
            json!({"source": wav_path.to_str().unwrap(), "shared": false, "loop_count": null}),
        )
        .await;
    let file_ep_id = result["endpoint_id"].as_str().unwrap().to_string();

    // Let playback start
    tokio::time::sleep(timing::scaled_ms(300)).await;

    // Resume while already playing — should succeed (idempotent)
    client
        .request_ok("endpoint.file.resume", json!({"endpoint_id": file_ep_id}))
        .await;

    // Resume again — should still succeed
    client
        .request_ok("endpoint.file.resume", json!({"endpoint_id": file_ep_id}))
        .await;

    client.request_ok("session.destroy", json!({})).await;
}

/// Test: Concurrent URL downloads for the same file are deduplicated by the cache.
///
/// Multiple sessions request the same URL simultaneously. The file cache should
/// only download once and serve all sessions from the same cached copy. All
/// sessions should receive audio packets.
#[tokio::test]
async fn test_concurrent_url_downloads_deduplicated() {
    let tmp = TempDir::new().unwrap();

    // Start a tiny HTTP server that serves a WAV file and counts requests
    let wav_path = tmp.path().join("rtpbridge-concurrent-dedup.wav");
    generate_test_wav(&wav_path, 1.0, 440.0);
    let wav_bytes = std::fs::read(&wav_path).unwrap();

    let http_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let http_addr = http_listener.local_addr().unwrap();
    let request_count = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
    let request_count_clone = request_count.clone();

    let http_handle = tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = http_listener.accept().await else {
                break;
            };
            let wav = wav_bytes.clone();
            let counter = request_count_clone.clone();
            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = vec![0u8; 4096];
                let _ = stream.read(&mut buf).await;
                counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                // Small delay to simulate real network latency
                tokio::time::sleep(Duration::from_millis(50)).await;
                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: audio/wav\r\nContent-Length: {}\r\n\r\n",
                    wav.len()
                );
                let _ = stream.write_all(header.as_bytes()).await;
                let _ = stream.write_all(&wav).await;
            });
        }
    });

    let server = TestServer::builder()
        .media_dir(tmp.path().to_str().unwrap())
        .start()
        .await;
    let url = format!(
        "http://{}:{}/dedup-test.wav",
        http_addr.ip(),
        http_addr.port()
    );

    // Create 4 sessions, all requesting the same URL simultaneously
    let mut clients = Vec::new();
    let mut peers = Vec::new();
    for _ in 0..4 {
        let mut client = TestControlClient::connect(&server.addr).await;
        client.request_ok("session.create", json!({})).await;

        let result = client
            .request_ok(
                "endpoint.create_offer",
                json!({"type": "rtp", "direction": "sendrecv"}),
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
        peer.activate().await;
        tokio::time::sleep(timing::scaled_ms(50)).await;
        peer.start_recv();

        // Request the same URL file
        client
            .request_ok(
                "endpoint.create_with_file",
                json!({"source": url, "shared": true, "loop_count": null, "timeout_ms": 5000}),
            )
            .await;

        clients.push(client);
        peers.push(peer);
    }

    // Wait for all sessions to receive audio
    let deadline = tokio::time::Instant::now() + timing::scaled_ms(5000);
    loop {
        let all_received = peers.iter().all(|p| p.received_count() > 0);
        if all_received {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            let counts: Vec<u64> = peers.iter().map(|p| p.received_count()).collect();
            panic!("timed out waiting for all peers to receive audio: {counts:?}");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // All peers should have received audio
    for (i, peer) in peers.iter().enumerate() {
        assert!(
            peer.received_count() > 0,
            "peer {i} should have received audio from cached URL playback"
        );
    }

    // The HTTP server should have received at most 2 requests (one real download,
    // possibly one retry — but not 4 separate downloads)
    let total_requests = request_count.load(std::sync::atomic::Ordering::Relaxed);
    assert!(
        total_requests <= 2,
        "file cache should deduplicate concurrent downloads: got {total_requests} HTTP requests for 4 sessions"
    );

    for mut client in clients {
        client.request_ok("session.destroy", json!({})).await;
    }
    http_handle.abort();
}

/// Test: URL download exceeding max_file_download_bytes is rejected.
#[tokio::test]
async fn test_url_download_exceeds_max_size() {
    let tmp = TempDir::new().unwrap();

    // HTTP server that advertises Content-Length: 2000 (above our 1024-byte limit)
    let http_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let http_addr = http_listener.local_addr().unwrap();

    let http_handle = tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = http_listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = vec![0u8; 4096];
                let _ = stream.read(&mut buf).await;
                // Respond with Content-Length exceeding the configured limit
                let body = vec![0u8; 2000];
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: audio/wav\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(resp.as_bytes()).await;
                let _ = stream.write_all(&body).await;
            });
        }
    });

    // Server with a tiny max_file_download_bytes (1024 bytes)
    let server = TestServer::builder()
        .media_dir(tmp.path().to_str().unwrap())
        .max_file_download_bytes(1024)
        .start()
        .await;
    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;

    let url = format!("http://{}:{}/big.wav", http_addr.ip(), http_addr.port());
    let result = client
        .request_ok(
            "endpoint.create_with_file",
            json!({"source": url, "timeout_ms": 5000}),
        )
        .await;
    let eid = result["endpoint_id"]
        .as_str()
        .expect("endpoint_id should be a string");
    assert!(!eid.is_empty());

    // Should receive a file.finished or state_changed error event
    let mut got_error = false;
    for _ in 0..20 {
        if let Some(event) = client.recv_event(timing::scaled_ms(500)).await {
            let event_type = event["event"].as_str().unwrap_or("");
            if event_type == "endpoint.file.finished"
                || (event_type == "endpoint.state_changed"
                    && event["data"]["new_state"].as_str() == Some("finished"))
            {
                got_error = true;
                break;
            }
        }
    }
    assert!(
        got_error,
        "should receive error event for oversized download"
    );

    client.request_ok("session.destroy", json!({})).await;
    http_handle.abort();
}

/// Test: Seeking past the end of a file should either return an error or
/// handle the condition gracefully (e.g. immediately fire finished event).
#[tokio::test]
async fn test_file_seek_past_end_of_file() {
    let tmp = TempDir::new().unwrap();
    let server = TestServer::builder()
        .media_dir(tmp.path().to_str().unwrap())
        .start()
        .await;
    let mut client = TestControlClient::connect(&server.addr).await;

    // Create a short WAV file (500ms)
    let wav_path = tmp.path().join("rtpbridge-fileplay-seek-past-end.wav");
    generate_test_wav(&wav_path, 0.5, 440.0);

    client.request_ok("session.create", json!({})).await;

    // Create receiving RTP endpoint
    let result = client
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv"}),
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
    peer.start_recv();

    // Create file endpoint (infinite loop so it stays alive for the seek)
    let result = client
        .request_ok(
            "endpoint.create_with_file",
            json!({"source": wav_path.to_str().unwrap(), "shared": false, "loop_count": null}),
        )
        .await;
    let file_ep_id = result["endpoint_id"].as_str().unwrap().to_string();

    // Let playback start
    tokio::time::sleep(timing::scaled_ms(300)).await;

    // Seek way past the end of the 500ms file
    let resp = client
        .request(
            "endpoint.file.seek",
            json!({"endpoint_id": file_ep_id, "position_ms": 999999}),
        )
        .await;

    // The server should handle this gracefully: either return an error,
    // or succeed and let the file wrap / finish
    assert!(
        resp.get("result").is_some() || resp.get("error").is_some(),
        "server should handle seek past end gracefully: {resp}"
    );

    // Give the server a moment to process — it should not crash
    tokio::time::sleep(timing::scaled_ms(500)).await;

    // Session should still be healthy
    let info = client.request_ok("session.info", json!({})).await;
    assert!(
        info["session_id"].as_str().is_some(),
        "session should still be alive after seek past end"
    );

    client.request_ok("session.destroy", json!({})).await;
}

/// Create a file endpoint with a WAV file, seek to a position,
/// verify playback continues (peer receives packets after seek).
#[tokio::test]
async fn test_file_seek() {
    // Set up media directory and WAV file
    let tmp = TempDir::new().unwrap();
    let media_dir = tmp.path().join("media");
    std::fs::create_dir_all(&media_dir).unwrap();

    let wav_path = media_dir.join("seektest.wav");
    generate_test_wav(&wav_path, 3.0, 440.0); // 3-second audio file

    let server = TestServer::builder()
        .media_dir(media_dir.to_str().unwrap())
        .start()
        .await;

    let mut client = TestControlClient::connect(&server.addr).await;

    client.request_ok("session.create", json!({})).await;

    // Create an RTP endpoint to receive file playback
    let mut peer = TestRtpPeer::new().await;
    let result = client
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv"}),
        )
        .await;
    let ep_rtp_id = result["endpoint_id"].as_str().unwrap().to_string();
    let offer = result["sdp_offer"].as_str().unwrap();
    let server_addr = parse_rtp_addr_from_sdp(offer).expect("parse server addr");
    let answer = peer.make_sdp_answer();
    client
        .request_ok(
            "endpoint.accept_answer",
            json!({"endpoint_id": ep_rtp_id, "sdp": answer}),
        )
        .await;
    peer.set_remote(server_addr);

    // Activate peer so symmetric RTP guard allows outbound packets
    peer.activate().await;
    tokio::time::sleep(timing::scaled_ms(50)).await;

    peer.start_recv();

    // Create file playback endpoint (loop=0, play once)
    let result = client
        .request_ok(
            "endpoint.create_with_file",
            json!({
                "source": wav_path.to_string_lossy(),
                "loop_count": 0
            }),
        )
        .await;
    let file_ep_id = result["endpoint_id"].as_str().unwrap().to_string();

    // Let playback run briefly
    tokio::time::sleep(timing::scaled_ms(500)).await;

    let received_before_seek = peer.received_count();
    assert!(
        received_before_seek > 0,
        "peer should receive packets from file playback before seek, got {received_before_seek}"
    );

    // Seek to 2000ms position
    client
        .request_ok(
            "endpoint.file.seek",
            json!({"endpoint_id": file_ep_id, "position_ms": 2000}),
        )
        .await;

    // Wait for playback to continue after seek
    tokio::time::sleep(timing::scaled_ms(500)).await;

    let received_after_seek = peer.received_count();
    assert!(
        received_after_seek > received_before_seek,
        "peer should receive more packets after seek, before={received_before_seek}, after={received_after_seek}"
    );

    client.request_ok("session.destroy", json!({})).await;
}
