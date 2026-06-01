//! Integration tests for the WebSocket audio-streaming endpoint.
//!
//! Exercises the full dial-in path: create over the control plane, connect the
//! audio socket to `/audio/<token>`, and verify PCM routes between endpoints,
//! events fire, tokens are single-use, and outbound coalescing works.

mod helpers;

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use helpers::control_client::TestControlClient;
use helpers::test_rtp_peer::{TestRtpPeer, parse_rtp_addr_from_sdp};
use helpers::test_server::TestServer;
use serde_json::json;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

type AudioClient = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Dial the audio plane for a given connect token.
async fn connect_audio(addr: &str, token: &str) -> AudioClient {
    let url = format!("ws://{addr}/audio/{token}");
    let (ws, _) = connect_async(&url).await.expect("audio ws connect failed");
    ws
}

/// One 20 ms frame of constant-DC PCM at 8 kHz (160 samples, little-endian i16).
fn dc_frame_8k(value: i16) -> Vec<u8> {
    std::iter::repeat_n(value, 160)
        .flat_map(|s| s.to_le_bytes())
        .collect()
}

/// Read binary audio from a client until `want_bytes` collected or timeout.
async fn read_pcm(ws: &mut AudioClient, want_bytes: usize, timeout: Duration) -> Vec<u8> {
    let mut out = Vec::new();
    let deadline = tokio::time::Instant::now() + timeout;
    while out.len() < want_bytes {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, ws.next()).await {
            Ok(Some(Ok(Message::Binary(b)))) => out.extend_from_slice(&b),
            Ok(Some(Ok(_))) => {} // ignore ping/pong/text
            _ => break,
        }
    }
    out
}

/// Read the next binary message from a client (or None on timeout/close).
async fn next_binary(ws: &mut AudioClient, timeout: Duration) -> Option<Vec<u8>> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return None;
        }
        match tokio::time::timeout(remaining, ws.next()).await {
            Ok(Some(Ok(Message::Binary(b)))) => return Some(b.to_vec()),
            Ok(Some(Ok(_))) => continue,
            _ => return None,
        }
    }
}

#[tokio::test]
async fn ws_audio_routes_pcm_between_two_ws_endpoints() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;

    let a = client
        .request_ok("endpoint.create_websocket", json!({"sample_rate": 8000}))
        .await;
    let b = client
        .request_ok("endpoint.create_websocket", json!({"sample_rate": 8000}))
        .await;
    let token_a = a["connect_token"].as_str().unwrap().to_string();
    let token_b = b["connect_token"].as_str().unwrap().to_string();

    let mut ws_a = connect_audio(&server.addr, &token_a).await;
    let mut ws_b = connect_audio(&server.addr, &token_b).await;

    // Both must report connected before routing includes them.
    client
        .recv_event_type("endpoint.ws.connected", Duration::from_secs(3))
        .await
        .expect("first ws connected event");
    client
        .recv_event_type("endpoint.ws.connected", Duration::from_secs(3))
        .await
        .expect("second ws connected event");

    // Stream a constant DC level into A; it should route to B.
    let frame = dc_frame_8k(1000);
    for _ in 0..25 {
        ws_a.send(Message::Binary(frame.clone().into()))
            .await
            .expect("send into A");
    }

    let pcm = read_pcm(&mut ws_b, 320 * 5, Duration::from_secs(3)).await;
    assert!(!pcm.is_empty(), "expected routed audio to emerge on B");
    assert_eq!(pcm.len() % 2, 0, "PCM must be whole 16-bit samples");

    let samples: Vec<i16> = pcm
        .chunks_exact(2)
        .map(|c| i16::from_le_bytes([c[0], c[1]]))
        .collect();
    let mean = samples.iter().map(|&s| s as i64).sum::<i64>() / samples.len() as i64;
    // 8k->48k->8k roundtrip of a constant preserves the DC level (a few
    // boundary samples are near zero, so use a loose floor well above silence).
    assert!(
        mean > 500,
        "expected ~1000 DC level routed through, got mean {mean} over {} samples",
        samples.len()
    );
}

#[tokio::test]
async fn ws_audio_emits_connected_and_disconnected_events() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;

    let ep = client
        .request_ok("endpoint.create_websocket", json!({"sample_rate": 16000}))
        .await;
    let endpoint_id = ep["endpoint_id"].as_str().unwrap().to_string();
    let token = ep["connect_token"].as_str().unwrap().to_string();

    let mut ws = connect_audio(&server.addr, &token).await;
    let connected = client
        .recv_event_type("endpoint.ws.connected", Duration::from_secs(3))
        .await
        .expect("connected event");
    assert_eq!(
        connected["data"]["endpoint_id"].as_str().unwrap(),
        endpoint_id
    );

    // Close the audio socket; the session should report a disconnect.
    ws.close(None).await.ok();
    let disconnected = client
        .recv_event_type("endpoint.ws.disconnected", Duration::from_secs(3))
        .await
        .expect("disconnected event");
    assert_eq!(
        disconnected["data"]["endpoint_id"].as_str().unwrap(),
        endpoint_id
    );
}

#[tokio::test]
async fn ws_audio_unknown_token_is_rejected() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;

    // A well-formed but never-issued token must be refused (socket closed).
    let mut ws = connect_audio(&server.addr, "00000000-0000-0000-0000-0000000000ff").await;
    match tokio::time::timeout(Duration::from_secs(3), ws.next()).await {
        Ok(None) | Ok(Some(Ok(Message::Close(_)))) | Ok(Some(Err(_))) => {}
        other => panic!("expected close for unknown token, got {other:?}"),
    }
}

#[tokio::test]
async fn ws_audio_connect_token_is_single_use() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;

    let ep = client
        .request_ok("endpoint.create_websocket", json!({"sample_rate": 8000}))
        .await;
    let token = ep["connect_token"].as_str().unwrap().to_string();

    let _ws1 = connect_audio(&server.addr, &token).await;
    client
        .recv_event_type("endpoint.ws.connected", Duration::from_secs(3))
        .await
        .expect("first connect succeeds");

    // Reusing the (now consumed) token must be refused.
    let mut ws2 = connect_audio(&server.addr, &token).await;
    match tokio::time::timeout(Duration::from_secs(3), ws2.next()).await {
        Ok(None) | Ok(Some(Ok(Message::Close(_)))) | Ok(Some(Err(_))) => {}
        other => panic!("expected close for reused token, got {other:?}"),
    }
}

#[tokio::test]
async fn ws_audio_endpoint_listed_in_session_info() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;

    client
        .request_ok("endpoint.create_websocket", json!({"sample_rate": 8000}))
        .await;

    let info = client.request_ok("session.info", json!({})).await;
    let eps = info["endpoints"].as_array().unwrap();
    assert_eq!(eps.len(), 1);
    assert_eq!(eps[0]["endpoint_type"].as_str().unwrap(), "websocket");
    assert_eq!(eps[0]["codec"].as_str().unwrap(), "L16/8000");
}

#[tokio::test]
async fn ws_audio_outbound_flush_ms_coalesces_frames() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;

    // Source sends 20 ms frames; receiver coalesces 100 ms (5 frames) per message.
    let src = client
        .request_ok("endpoint.create_websocket", json!({"sample_rate": 8000}))
        .await;
    let dst = client
        .request_ok(
            "endpoint.create_websocket",
            json!({"sample_rate": 8000, "flush_ms": 100}),
        )
        .await;
    let token_src = src["connect_token"].as_str().unwrap().to_string();
    let token_dst = dst["connect_token"].as_str().unwrap().to_string();

    let mut ws_src = connect_audio(&server.addr, &token_src).await;
    let mut ws_dst = connect_audio(&server.addr, &token_dst).await;
    client
        .recv_event_type("endpoint.ws.connected", Duration::from_secs(3))
        .await
        .unwrap();
    client
        .recv_event_type("endpoint.ws.connected", Duration::from_secs(3))
        .await
        .unwrap();

    let frame = dc_frame_8k(2000);
    for _ in 0..30 {
        ws_src
            .send(Message::Binary(frame.clone().into()))
            .await
            .expect("send into source");
    }

    // Each coalesced message is 5 frames * 160 samples * 2 bytes = 1600 bytes at 8 kHz.
    let msg = next_binary(&mut ws_dst, Duration::from_secs(3))
        .await
        .expect("expected a coalesced outbound message");
    assert_eq!(
        msg.len(),
        1600,
        "100ms flush window at 8kHz mono should be 1600 bytes, got {}",
        msg.len()
    );
}

#[tokio::test]
async fn ws_audio_to_rtp_peer_advances_timestamps() {
    // Validates the core design decision: a WS source must synthesize a monotonic
    // RTP timeline. If it emitted bridge-style ts=0, the RTP destination's
    // advance_outbound_timeline would treat every frame as a duplicate and freeze
    // the wire timestamp. We assert the PCMU peer sees strictly increasing stamps.
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;

    // PCMU RTP peer as the destination.
    let mut peer = TestRtpPeer::new().await;
    let offer = peer.make_sdp_offer();
    let result = client
        .request_ok(
            "endpoint.create_from_offer",
            json!({"sdp": offer, "direction": "sendrecv"}),
        )
        .await;
    let server_addr =
        parse_rtp_addr_from_sdp(result["sdp_answer"].as_str().unwrap()).expect("parse server addr");
    peer.set_remote(server_addr);
    // Activating sends a packet so rtpbridge learns the peer's address/SSRC and
    // the symmetric-RTP guard permits outbound packets.
    peer.activate().await;
    peer.start_recv();

    // WS source.
    let ws_ep = client
        .request_ok("endpoint.create_websocket", json!({"sample_rate": 8000}))
        .await;
    let token = ws_ep["connect_token"].as_str().unwrap().to_string();
    let mut ws = connect_audio(&server.addr, &token).await;
    client
        .recv_event_type("endpoint.ws.connected", Duration::from_secs(3))
        .await
        .expect("ws connected");

    // Stream ~25 frames (500 ms) of audio into the WS endpoint.
    let frame = dc_frame_8k(4000);
    for _ in 0..25 {
        ws.send(Message::Binary(frame.clone().into()))
            .await
            .expect("send into ws");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    tokio::time::sleep(Duration::from_millis(300)).await;

    let stamps = peer.received_timestamps().await;
    assert!(
        stamps.len() >= 3,
        "expected several RTP packets at the PCMU peer, got {}",
        stamps.len()
    );
    // The whole point: timestamps must advance, not freeze.
    for w in stamps.windows(2) {
        assert!(
            w[1].wrapping_sub(w[0]) > 0,
            "RTP timestamp must advance between frames, saw {} then {}",
            w[0],
            w[1]
        );
    }
}

#[tokio::test]
async fn ws_audio_malformed_token_path_rejected() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;

    // `/audio/<not-a-uuid>` must be handled as a (rejected) audio request, not
    // silently accepted as a control connection.
    let mut ws = connect_audio(&server.addr, "not-a-uuid").await;
    match tokio::time::timeout(Duration::from_secs(3), ws.next()).await {
        Ok(None) | Ok(Some(Ok(Message::Close(_)))) | Ok(Some(Err(_))) => {}
        other => panic!("expected close for malformed token path, got {other:?}"),
    }
}

#[tokio::test]
async fn ws_audio_responds_to_ping() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;

    let ep = client
        .request_ok("endpoint.create_websocket", json!({"sample_rate": 8000}))
        .await;
    let token = ep["connect_token"].as_str().unwrap().to_string();
    let mut ws = connect_audio(&server.addr, &token).await;
    client
        .recv_event_type("endpoint.ws.connected", Duration::from_secs(3))
        .await
        .expect("connected");

    ws.send(Message::Ping(vec![9, 9, 9].into()))
        .await
        .expect("send ping");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    let mut got_pong = false;
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(500), ws.next()).await {
            Ok(Some(Ok(Message::Pong(p)))) => {
                assert_eq!(&p[..], &[9, 9, 9]);
                got_pong = true;
                break;
            }
            Ok(Some(Ok(_))) => continue,
            _ => break,
        }
    }
    assert!(got_pong, "expected a Pong in response to Ping");
}

#[tokio::test]
async fn ws_audio_tolerates_odd_length_frames() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;

    let a = client
        .request_ok("endpoint.create_websocket", json!({"sample_rate": 8000}))
        .await;
    let b = client
        .request_ok("endpoint.create_websocket", json!({"sample_rate": 8000}))
        .await;
    let mut ws_a = connect_audio(&server.addr, a["connect_token"].as_str().unwrap()).await;
    let mut ws_b = connect_audio(&server.addr, b["connect_token"].as_str().unwrap()).await;
    client
        .recv_event_type("endpoint.ws.connected", Duration::from_secs(3))
        .await
        .unwrap();
    client
        .recv_event_type("endpoint.ws.connected", Duration::from_secs(3))
        .await
        .unwrap();

    // 321-byte frames: 160 samples + a stray odd byte that must be buffered, not
    // dropped or misaligned, across reads.
    let mut frame = dc_frame_8k(1500);
    frame.push(0x7f);
    assert_eq!(frame.len() % 2, 1);
    for _ in 0..30 {
        ws_a.send(Message::Binary(frame.clone().into()))
            .await
            .expect("send odd frame");
    }

    let pcm = read_pcm(&mut ws_b, 320 * 3, Duration::from_secs(3)).await;
    assert!(
        !pcm.is_empty(),
        "audio must still route despite odd-length input frames"
    );
}

#[tokio::test]
async fn ws_audio_rejects_invalid_params() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;

    let bad_rate = client
        .request("endpoint.create_websocket", json!({"sample_rate": 12345}))
        .await;
    assert_eq!(
        bad_rate["error"]["code"].as_str().unwrap(),
        "INVALID_PARAMS"
    );

    let bad_flush = client
        .request(
            "endpoint.create_websocket",
            json!({"sample_rate": 8000, "flush_ms": 30}),
        )
        .await;
    assert_eq!(
        bad_flush["error"]["code"].as_str().unwrap(),
        "INVALID_PARAMS"
    );
}
