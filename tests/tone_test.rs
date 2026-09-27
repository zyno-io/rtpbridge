mod helpers;

use std::time::Duration;

use rtpbridge::media::codec::{AudioCodec, make_decoder};

use serde_json::json;

use helpers::control_client::TestControlClient;
use helpers::test_rtp_peer::{TestRtpPeer, parse_rtp_addr_from_sdp};
use helpers::test_server::TestServer;
use helpers::timing;

#[tokio::test]
async fn test_silence_keeps_negotiated_opus_rtp_flowing_until_removed() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;
    let mut peer = TestRtpPeer::new().await;
    let offer = format!(
        "v=0\r\no=- 1 1 IN IP4 {ip}\r\ns=-\r\nc=IN IP4 {ip}\r\nt=0 0\r\nm=audio {port} RTP/AVP 111\r\na=rtpmap:111 opus/48000/2\r\na=sendrecv\r\n",
        ip = peer.local_addr.ip(),
        port = peer.local_addr.port()
    );
    let endpoint = client
        .request_ok("endpoint.create_from_offer", json!({"sdp": offer}))
        .await;
    let address = parse_rtp_addr_from_sdp(endpoint["sdp_answer"].as_str().unwrap()).unwrap();
    peer.set_remote(address);
    peer.activation_pt = 111;
    peer.activate().await;
    peer.start_recv();
    let silence = client
        .request_ok("endpoint.create_tone", json!({"tone": "silence"}))
        .await;
    assert_eq!(silence["tone"], "silence");
    tokio::time::sleep(timing::scaled_ms(700)).await;
    let packets = peer.all_received_raw().await;
    assert!(
        packets.len() >= 20,
        "silence must produce paced RTP, got {} packets",
        packets.len()
    );
    let mut decoder = make_decoder(AudioCodec::Opus).unwrap();
    for packet in &packets {
        assert_eq!(packet[1] & 127, 111, "silence must use negotiated Opus PT");
        let mut pcm = Vec::new();
        decoder.decode(&packet[12..], &mut pcm).unwrap();
        assert_eq!(pcm.len(), 960, "silence must retain the 20 ms audio clock");
        // Opus decoding may dither digital silence; keep it below -60 dBFS.
        let rms = (pcm
            .iter()
            .map(|sample| (*sample as f64).powi(2))
            .sum::<f64>()
            / pcm.len() as f64)
            .sqrt();
        assert!(rms < 32.0, "silence must remain inaudible, RMS={rms}");
    }
    for pair in packets.windows(2) {
        let previous = u32::from_be_bytes(pair[0][4..8].try_into().unwrap());
        let current = u32::from_be_bytes(pair[1][4..8].try_into().unwrap());
        assert_eq!(
            current.wrapping_sub(previous),
            960,
            "RTP timestamps must advance normally"
        );
    }

    // Prompts must remain audible while the silence source shares their mixer.
    let beep = client
        .request_ok("endpoint.create_tone", json!({"tone": "beep"}))
        .await;
    tokio::time::sleep(timing::scaled_ms(300)).await;
    let mixed_packets = peer.all_received_raw().await;
    let mut audible_frames = 0;
    for packet in mixed_packets.iter().skip(packets.len()) {
        assert_eq!(packet[1] & 127, 111);
        let mut pcm = Vec::new();
        decoder.decode(&packet[12..], &mut pcm).unwrap();
        if pcm.iter().any(|sample| (*sample as i32).abs() > 1000) {
            audible_frames += 1;
        }
    }
    assert!(audible_frames >= 5, "silence must not suppress the beep");
    client
        .request_ok(
            "endpoint.remove",
            json!({"endpoint_id": beep["endpoint_id"]}),
        )
        .await;
    tokio::time::sleep(timing::scaled_ms(100)).await;
    let after_beep = peer.received_count();
    tokio::time::sleep(timing::scaled_ms(200)).await;
    assert!(
        peer.received_count() >= after_beep + 5,
        "silence must continue after the prompt is removed"
    );
    client
        .request_ok(
            "endpoint.remove",
            json!({"endpoint_id": silence["endpoint_id"]}),
        )
        .await;
    tokio::time::sleep(timing::scaled_ms(100)).await;
    let stopped_count = peer.received_count();
    tokio::time::sleep(timing::scaled_ms(200)).await;
    assert_eq!(
        peer.received_count(),
        stopped_count,
        "removing silence must stop its RTP"
    );
    client.request_ok("session.destroy", json!({})).await;
}

/// Helper: create a PCMU endpoint and activate symmetric RTP.
async fn setup_rtp_endpoint(client: &mut TestControlClient, peer: &mut TestRtpPeer) -> String {
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
    peer.activate().await;
    ep_id
}

/// Test: A ringback tone endpoint sends PCMU packets to an RTP peer.
#[tokio::test]
async fn test_tone_ringback_delivers_audio() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;

    let mut peer = TestRtpPeer::new().await;
    setup_rtp_endpoint(&mut client, &mut peer).await;

    // Create ringback tone
    let result = client
        .request_ok("endpoint.create_tone", json!({"tone": "ringback"}))
        .await;
    let tone_id = result["endpoint_id"].as_str().unwrap().to_string();
    assert_eq!(result["tone"].as_str().unwrap(), "ringback");

    peer.start_recv();
    tokio::time::sleep(timing::scaled_ms(50)).await;

    // Wait for audio to arrive (ringback starts with 2s on phase)
    tokio::time::sleep(timing::scaled_ms(500)).await;

    let count = peer.received_count();
    assert!(
        count > 5,
        "should receive ringback tone packets, got {count}"
    );

    // Remove the tone
    client
        .request_ok("endpoint.remove", json!({"endpoint_id": tone_id}))
        .await;
}

/// Test: A sine tone endpoint with custom frequency.
#[tokio::test]
async fn test_tone_sine_custom_frequency() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;

    let mut peer = TestRtpPeer::new().await;
    setup_rtp_endpoint(&mut client, &mut peer).await;

    let result = client
        .request_ok(
            "endpoint.create_tone",
            json!({"tone": "sine", "frequency": 1000.0}),
        )
        .await;
    assert!(result["endpoint_id"].as_str().is_some());

    peer.start_recv();
    tokio::time::sleep(timing::scaled_ms(400)).await;

    let count = peer.received_count();
    assert!(count > 5, "should receive sine tone packets, got {count}");
}

/// Test: A tone with duration_ms finishes and emits an event.
#[tokio::test]
async fn test_tone_duration_limit_emits_finished_event() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;

    let mut peer = TestRtpPeer::new().await;
    setup_rtp_endpoint(&mut client, &mut peer).await;
    peer.start_recv();

    // Create a 200ms tone
    client
        .request_ok(
            "endpoint.create_tone",
            json!({"tone": "sine", "frequency": 440.0, "duration_ms": 200}),
        )
        .await;

    // Wait for the tone to finish
    let event = client
        .recv_event_type("endpoint.tone.finished", Duration::from_secs(3))
        .await;
    assert!(event.is_some(), "should receive tone.finished event");
}

/// Test: Busy tone has correct cadence (0.5s on / 0.5s off).
#[tokio::test]
async fn test_tone_busy_delivers_audio() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;

    let mut peer = TestRtpPeer::new().await;
    setup_rtp_endpoint(&mut client, &mut peer).await;

    client
        .request_ok("endpoint.create_tone", json!({"tone": "busy"}))
        .await;

    peer.start_recv();
    tokio::time::sleep(timing::scaled_ms(600)).await;

    let count = peer.received_count();
    assert!(count > 5, "should receive busy tone packets, got {count}");
}

/// Test: Invalid frequency is rejected.
#[tokio::test]
async fn test_tone_invalid_frequency_rejected() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;

    let result = client
        .request(
            "endpoint.create_tone",
            json!({"tone": "sine", "frequency": 50000.0}),
        )
        .await;
    assert!(
        result["error"].is_object(),
        "frequency > 20kHz should be rejected"
    );
}
