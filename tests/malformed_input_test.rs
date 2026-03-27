mod helpers;

use serde_json::json;

use helpers::control_client::TestControlClient;
use helpers::test_server::TestServer;

#[tokio::test]
async fn test_malformed_json() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;

    let resp = client.send_raw("not json at all").await;
    assert_eq!(resp["error"]["code"], "PARSE_ERROR");
}

#[tokio::test]
async fn test_missing_method() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;

    let resp = client.send_raw(r#"{"id":"1"}"#).await;
    assert_eq!(resp["error"]["code"], "PARSE_ERROR");
}

#[tokio::test]
async fn test_missing_id() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;

    let resp = client.send_raw(r#"{"method":"session.create"}"#).await;
    assert_eq!(resp["error"]["code"], "PARSE_ERROR");
}

#[tokio::test]
async fn test_unknown_method() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;

    let resp = client
        .send_raw(r#"{"id":"1","method":"nonexistent.method","params":{}}"#)
        .await;
    assert_eq!(resp["error"]["code"], "UNKNOWN_METHOD");
}

#[tokio::test]
async fn test_wrong_param_types() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;

    // Create session first
    client.request_ok("session.create", json!({})).await;

    // Send endpoint.create_offer with wrong type for direction (number instead of string)
    let resp = client
        .request("endpoint.create_offer", json!({"direction": 123}))
        .await;
    assert!(
        resp["error"].is_object(),
        "expected error for wrong param type: {resp}"
    );
    assert_eq!(resp["error"]["code"], "INVALID_PARAMS");
}

#[tokio::test]
async fn test_oversized_sdp() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;

    client.request_ok("session.create", json!({})).await;

    // 65KB SDP string (exceeds MAX_SDP_SIZE of 64KB)
    let huge_sdp = "x".repeat(65 * 1024);
    let resp = client
        .request(
            "endpoint.create_from_offer",
            json!({
                "sdp": huge_sdp,
                "direction": "sendrecv"
            }),
        )
        .await;
    assert_eq!(resp["error"]["code"], "INVALID_PARAMS");
    assert!(
        resp["error"]["message"]
            .as_str()
            .unwrap()
            .contains("too large")
    );
}

#[tokio::test]
async fn test_no_session_bound() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;

    // Send endpoint command without creating a session first
    let resp = client
        .request(
            "endpoint.create_offer",
            json!({
                "direction": "sendrecv"
            }),
        )
        .await;
    assert_eq!(resp["error"]["code"], "NO_SESSION");
}

#[tokio::test]
async fn test_empty_message() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;

    let resp = client.send_raw("").await;
    assert_eq!(resp["error"]["code"], "PARSE_ERROR");
}

/// Test: An empty SDP string does not crash/panic the server.
/// The server may either reject it with an error or accept it leniently
/// (creating a minimal endpoint with only telephone-event).
#[tokio::test]
async fn test_empty_sdp_handled_gracefully() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;

    client.request_ok("session.create", json!({})).await;

    // Send endpoint.create_from_offer with an empty SDP
    let resp = client
        .request(
            "endpoint.create_from_offer",
            json!({
                "sdp": "",
                "direction": "sendrecv"
            }),
        )
        .await;

    // Either outcome is acceptable — the server should not crash
    if let Some(result) = resp.get("result") {
        // If accepted, it should have an endpoint_id and an SDP answer
        assert!(
            result["endpoint_id"].is_string(),
            "accepted empty SDP should return endpoint_id: {resp}"
        );
        assert!(
            result["sdp_answer"].is_string(),
            "accepted empty SDP should return sdp_answer: {resp}"
        );
        // The answer should not contain any audio codec (empty offer has none)
        let answer = result["sdp_answer"].as_str().unwrap();
        assert!(
            !answer.contains("PCMU") && !answer.contains("opus"),
            "answer from empty SDP should not contain audio codecs: {answer}"
        );
    } else {
        // If rejected, it should be a proper error response
        assert!(
            resp.get("error").is_some(),
            "empty SDP should return either result or error: {resp}"
        );
    }

    // The server should still be responsive
    let info = client.request_ok("session.info", json!({})).await;
    assert!(
        !info["session_id"].as_str().unwrap().is_empty(),
        "session should still be valid after empty SDP"
    );

    client.request_ok("session.destroy", json!({})).await;
}

/// Test: A 100KB oversized SDP is rejected (larger than the 64KB MAX_SDP_SIZE)
#[tokio::test]
async fn test_oversized_sdp_100kb_rejected() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;

    client.request_ok("session.create", json!({})).await;

    // 100KB SDP string (well over MAX_SDP_SIZE of 64KB)
    let huge_sdp = "x".repeat(100 * 1024);
    let resp = client
        .request(
            "endpoint.create_from_offer",
            json!({
                "sdp": huge_sdp,
                "direction": "sendrecv"
            }),
        )
        .await;
    assert_eq!(resp["error"]["code"], "INVALID_PARAMS");
    assert!(
        resp["error"]["message"]
            .as_str()
            .unwrap()
            .contains("too large")
    );

    // Server should still be responsive
    let info = client.request_ok("session.info", json!({})).await;
    assert!(
        !info["session_id"].as_str().unwrap().is_empty(),
        "session should still be valid after oversized SDP rejection"
    );

    client.request_ok("session.destroy", json!({})).await;
}

/// Test: Offering only unsupported codecs still creates an endpoint, but with
/// only telephone-event negotiated (no audio codec). The server is lenient —
/// it answers with whatever it can support. Verify the answer has no audio codec.
#[tokio::test]
async fn test_unsupported_codec_only_offer() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;

    client.request_ok("session.create", json!({})).await;

    // Offer with only iLBC (unsupported by bridge) — no telephone-event either
    let bad_offer = "\
        v=0\r\n\
        o=- 100 1 IN IP4 127.0.0.1\r\n\
        s=-\r\n\
        c=IN IP4 127.0.0.1\r\n\
        t=0 0\r\n\
        m=audio 50000 RTP/AVP 97\r\n\
        a=rtpmap:97 iLBC/8000\r\n\
        a=sendrecv\r\n";

    let resp = client
        .request(
            "endpoint.create_from_offer",
            json!({"sdp": bad_offer, "direction": "sendrecv"}),
        )
        .await;

    // The server may accept it (answering with telephone-event only) or reject it.
    // Either way, if it succeeds, the answer should NOT contain iLBC.
    if let Some(result) = resp.get("result") {
        let answer = result["sdp_answer"].as_str().unwrap_or("");
        assert!(
            !answer.contains("iLBC"),
            "answer should not contain unsupported iLBC codec"
        );
    }
    // If it's an error, that's also acceptable behavior

    client.request_ok("session.destroy", json!({})).await;
}

// ── Malformed UUID parameter tests ──────────────────────────────────────

#[tokio::test]
async fn test_malformed_endpoint_id_rejected() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;

    let resp = client
        .request("endpoint.remove", json!({"endpoint_id": "not-a-uuid"}))
        .await;
    assert!(
        resp.get("error").is_some(),
        "malformed endpoint_id should be rejected: {resp}"
    );

    client.request_ok("session.destroy", json!({})).await;
}

#[tokio::test]
async fn test_malformed_session_id_attach_rejected() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;

    let resp = client
        .request("session.attach", json!({"session_id": "not-a-uuid"}))
        .await;
    assert!(
        resp.get("error").is_some(),
        "malformed session_id should be rejected: {resp}"
    );
}

#[tokio::test]
async fn test_malformed_recording_id_rejected() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;

    let resp = client
        .request("recording.stop", json!({"recording_id": "not-a-uuid"}))
        .await;
    assert!(
        resp.get("error").is_some(),
        "malformed recording_id should be rejected: {resp}"
    );

    client.request_ok("session.destroy", json!({})).await;
}

// ── accept_answer error path tests ──────────────────────────────────────

#[tokio::test]
async fn test_accept_answer_nonexistent_endpoint() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;

    let resp = client
        .request(
            "endpoint.accept_answer",
            json!({
                "endpoint_id": "00000000-0000-0000-0000-000000000000",
                "sdp": "v=0\r\no=- 0 0 IN IP4 0.0.0.0\r\ns=-\r\nt=0 0\r\nm=audio 5004 RTP/AVP 0\r\n"
            }),
        )
        .await;
    assert!(
        resp.get("error").is_some(),
        "accept_answer on nonexistent endpoint should fail: {resp}"
    );

    client.request_ok("session.destroy", json!({})).await;
}

#[tokio::test]
async fn test_accept_answer_malformed_sdp() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;

    // Create an offer endpoint first
    let result = client
        .request_ok(
            "endpoint.create_offer",
            json!({"direction": "sendrecv", "type": "rtp"}),
        )
        .await;
    let ep_id = result["endpoint_id"].as_str().unwrap();

    // The SDP parser is lenient — garbage SDP is accepted without error
    // (parsed as empty, which is a no-op for accept_answer).
    // Verify the server at least doesn't crash.
    let resp = client
        .request(
            "endpoint.accept_answer",
            json!({"endpoint_id": ep_id, "sdp": "this is not valid SDP at all"}),
        )
        .await;
    // SDP parser is lenient but accept_answer now validates the parsed result
    // (e.g., requires a connection address). Garbage SDP should return an error.
    assert!(
        resp.get("error").is_some(),
        "garbage SDP should fail validation: {resp}"
    );

    client.request_ok("session.destroy", json!({})).await;
}

// ── URL scheme validation test ──────────────────────────────────────────

/// Verify that non-http(s) URL-like sources are rejected (either as invalid URL or as
/// inaccessible local path). The important thing is they don't succeed.
#[tokio::test]
async fn test_file_url_scheme_validation() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;

    // ftp:// is not recognized as a URL by is_url() (only http/https are),
    // so it's treated as a local file path and rejected when media_dir is not set.
    let resp = client
        .request(
            "endpoint.create_with_file",
            json!({"source": "ftp://evil.com/malware.wav"}),
        )
        .await;
    assert!(
        resp.get("error").is_some(),
        "ftp:// source should be rejected: {resp}"
    );

    client.request_ok("session.destroy", json!({})).await;
}
