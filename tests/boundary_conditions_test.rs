mod helpers;

use serde_json::json;

use helpers::control_client::TestControlClient;
use helpers::http::http_get;
use helpers::test_rtp_peer::{TestRtpPeer, parse_rtp_addr_from_sdp};
use helpers::test_server::TestServer;
use helpers::timing;

/// Helper: create session + RTP endpoint, return (endpoint_id, peer)
async fn setup_rtp_session(client: &mut TestControlClient) -> (String, TestRtpPeer) {
    client.request_ok("session.create", json!({})).await;

    let ep_result = client
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv"}),
        )
        .await;
    let ep_id = ep_result["endpoint_id"].as_str().unwrap().to_string();
    let sdp_offer = ep_result["sdp_offer"].as_str().unwrap();

    let remote_addr = parse_rtp_addr_from_sdp(sdp_offer).unwrap();
    let mut peer = TestRtpPeer::new().await;
    peer.set_remote(remote_addr);

    let answer = peer.make_sdp_answer();
    client
        .request_ok(
            "endpoint.accept_answer",
            json!({"endpoint_id": ep_id, "sdp": answer}),
        )
        .await;

    (ep_id, peer)
}

/// Test: DTMF inject with duration_ms=0 either succeeds or returns a clean error
#[tokio::test]
async fn test_dtmf_duration_zero() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;
    let (ep_id, _peer) = setup_rtp_session(&mut client).await;

    let resp = client
        .request(
            "endpoint.dtmf.inject",
            json!({"endpoint_id": ep_id, "digit": "5", "duration_ms": 0, "volume": 10}),
        )
        .await;

    // duration_ms=0 should be rejected with INVALID_PARAMS
    assert!(
        resp.get("error").is_some(),
        "duration_ms=0 should be rejected: {resp}"
    );
    assert_eq!(
        resp["error"]["code"].as_str().unwrap_or(""),
        "INVALID_PARAMS",
        "duration_ms=0 should return INVALID_PARAMS: {resp}"
    );

    client.request_ok("session.destroy", json!({})).await;
}

/// Test: DTMF inject with volume > 63 returns INVALID_PARAMS
#[tokio::test]
async fn test_dtmf_volume_out_of_range() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;
    let (ep_id, _peer) = setup_rtp_session(&mut client).await;

    let resp = client
        .request(
            "endpoint.dtmf.inject",
            json!({"endpoint_id": ep_id, "digit": "5", "duration_ms": 100, "volume": 64}),
        )
        .await;

    assert_eq!(
        resp["error"]["code"], "INVALID_PARAMS",
        "volume=64 should be rejected: {resp}"
    );

    client.request_ok("session.destroy", json!({})).await;
}

/// Test: DTMF inject with invalid digit character returns INVALID_PARAMS
#[tokio::test]
async fn test_dtmf_invalid_digit_char() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;
    let (ep_id, _peer) = setup_rtp_session(&mut client).await;

    let resp = client
        .request(
            "endpoint.dtmf.inject",
            json!({"endpoint_id": ep_id, "digit": "Z", "duration_ms": 100, "volume": 10}),
        )
        .await;

    assert_eq!(
        resp["error"]["code"], "INVALID_PARAMS",
        "digit 'Z' should be rejected: {resp}"
    );

    client.request_ok("session.destroy", json!({})).await;
}

/// Test: SDP with embedded NUL bytes doesn't panic
#[tokio::test]
async fn test_sdp_with_nul_bytes() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;

    // SDP with NUL bytes embedded
    let sdp_with_nuls = "v=0\r\no=- 1 1 IN IP4 127.0.0.1\r\ns=\0test\r\nt=0 0\r\nm=audio 9 RTP/AVP 0\r\nc=IN IP4 0.0.0.0\r\na=sendrecv\r\n";

    let resp = client
        .request(
            "endpoint.create_from_offer",
            json!({"sdp": sdp_with_nuls, "direction": "sendrecv"}),
        )
        .await;

    // SDP with NUL bytes: either succeeds (NUL in non-critical field) or fails with specific error
    if let Some(error) = resp.get("error") {
        let error_code = error["code"].as_str().unwrap_or("");
        assert!(
            error_code == "ENDPOINT_ERROR" || error_code == "INVALID_PARAMS",
            "SDP with NUL bytes error should be ENDPOINT_ERROR or INVALID_PARAMS, got: {error_code}"
        );
    } else {
        assert!(
            resp.get("result").is_some(),
            "SDP with NUL bytes should return a valid result or a specific error: {resp}"
        );
    }

    client.request_ok("session.destroy", json!({})).await;
}

/// Test: Deeply nested JSON doesn't cause stack overflow
#[tokio::test]
async fn test_deeply_nested_json() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;

    // Build a deeply nested JSON object
    let mut nested = String::from(r#"{"id":"1","method":"session.info","params":"#);
    for _ in 0..100 {
        nested.push_str(r#"{"a":"#);
    }
    nested.push_str(r#""v""#);
    for _ in 0..100 {
        nested.push('}');
    }
    nested.push('}');

    let resp = client.send_raw(&nested).await;

    // The deeply nested params should either be accepted (session.info succeeds)
    // or rejected with a specific parse/validation error
    if let Some(error) = resp.get("error") {
        let error_code = error["code"].as_str().unwrap_or("");
        assert!(
            error_code == "INVALID_PARAMS" || error_code == "PARSE_ERROR",
            "deeply nested JSON error should be INVALID_PARAMS or PARSE_ERROR, got: {error_code}"
        );
    } else {
        assert!(
            resp.get("result").is_some(),
            "deeply nested JSON should return a valid result: {resp}"
        );
    }

    client.request_ok("session.destroy", json!({})).await;
}

/// Test: WS disconnect during file playback — session orphans but survives
#[tokio::test]
async fn test_ws_disconnect_during_file_playback() {
    // Create a test WAV file
    let media_dir = tempfile::tempdir().unwrap();
    let wav_path = media_dir.path().join("test-tone.wav");
    helpers::wav::generate_test_wav(wav_path.to_str().unwrap(), 2.0, 440.0);

    let server = TestServer::builder()
        .media_dir(media_dir.path().to_str().unwrap())
        .disconnect_timeout_secs(5)
        .start()
        .await;

    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;

    // Create a file playback endpoint
    let _resp = client
        .request(
            "endpoint.create_with_file",
            json!({"source": wav_path.to_str().unwrap(), "shared": false, "loop_count": 0}),
        )
        .await;

    // Close WS mid-playback
    client.close().await;

    tokio::time::sleep(timing::scaled_ms(500)).await;

    // Session should be orphaned but alive
    let sessions = http_get(&server.addr, "/sessions").await;
    let arr = sessions.as_array().unwrap();
    assert!(
        arr.len() >= 1,
        "session should survive WS disconnect during playback: {:?}",
        arr
    );
}

/// Test: HTTP path traversal on /recordings endpoint returns 403 or 404
#[tokio::test]
async fn test_recording_download_path_traversal() {
    let server = TestServer::start().await;

    // Use raw TCP to avoid reqwest resolving ".." client-side
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::net::TcpStream::connect(&server.addr).await.unwrap();
    let request = "GET /recordings/..%2F..%2Fetc%2Fpasswd HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
    stream.write_all(request.as_bytes()).await.unwrap();

    let mut response = Vec::new();
    stream.read_to_end(&mut response).await.unwrap();
    let resp_str = String::from_utf8_lossy(&response);

    assert!(
        resp_str.contains("403 Forbidden") || resp_str.contains("404 Not Found"),
        "path traversal should be blocked: {}",
        resp_str
    );
}

/// Test: Recording list pagination via HTTP
#[tokio::test]
async fn test_recording_list_pagination_http() {
    let server = TestServer::start().await;

    // Create some PCAP files in the recording dir
    let rec_dir = &server.recording_dir;
    for i in 0..5 {
        std::fs::write(format!("{rec_dir}/test-{i}.pcap"), b"fake pcap data").unwrap();
    }

    let result = http_get(&server.addr, "/recordings?skip=1&limit=2").await;
    assert_eq!(result["total"], 5);
    assert_eq!(result["skip"], 1);
    assert_eq!(result["limit"], 2);
    assert_eq!(result["recordings"].as_array().unwrap().len(), 2);
}

/// Test: Endpoint limit enforced via max_endpoints_per_session
#[tokio::test]
async fn test_endpoint_limit_returns_error() {
    // Use max_endpoints_per_session=1 so the second endpoint always fails,
    // regardless of port availability.
    let server = TestServer::builder()
        .max_endpoints_per_session(1)
        .start()
        .await;

    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;

    // First endpoint should succeed
    let ep1 = client
        .request(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv"}),
        )
        .await;
    assert!(
        ep1.get("result").is_some(),
        "first endpoint should succeed: {ep1}"
    );

    // Second endpoint must fail — limit is 1
    let ep2 = client
        .request(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv"}),
        )
        .await;
    assert!(
        ep2.get("error").is_some(),
        "second endpoint should fail due to endpoint limit: {ep2}"
    );
    let error_code = ep2["error"]["code"].as_str().unwrap_or("");
    assert!(
        error_code == "ENDPOINT_ERROR"
            || error_code == "RESOURCE_ERROR"
            || error_code == "LIMIT_REACHED",
        "should return a specific error code, got: {error_code}"
    );

    // Session should still be healthy
    let info = client.request_ok("session.info", json!({})).await;
    assert!(info["endpoints"].as_array().is_some());

    client.request_ok("session.destroy", json!({})).await;
}
