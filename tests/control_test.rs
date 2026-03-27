mod helpers;

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

use helpers::control_client::TestControlClient;
use helpers::test_server::TestServer;
use helpers::timing;

type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Connect to the WebSocket server with retries to handle startup race conditions
async fn connect_with_retry(addr: &str) -> WsStream {
    let url = format!("ws://{addr}");
    let retries = 20;
    for i in 0..retries {
        match connect_async(&url).await {
            Ok((ws, _)) => return ws,
            Err(_) if i < retries - 1 => {
                tokio::time::sleep(timing::scaled_ms(250)).await;
            }
            Err(e) => panic!("failed to connect after {retries} retries: {e}"),
        }
    }
    unreachable!()
}

/// Send a JSON request and receive the response
async fn send_request(ws: &mut WsStream, id: &str, method: &str, params: Value) -> Value {
    let req = json!({ "id": id, "method": method, "params": params });
    ws.send(Message::Text(req.to_string().into()))
        .await
        .unwrap();

    match ws.next().await.unwrap().unwrap() {
        Message::Text(text) => serde_json::from_str(&text).unwrap(),
        other => panic!("Expected text message, got {:?}", other),
    }
}

#[tokio::test]
async fn test_session_lifecycle() {
    let server = TestServer::start().await;
    let addr = &server.addr;

    let mut ws = connect_with_retry(addr).await;

    // 1. Create session
    let resp = send_request(&mut ws, "1", "session.create", json!({})).await;
    assert!(
        !resp["result"]["session_id"].as_str().unwrap().is_empty(),
        "expected session_id: {resp}"
    );
    let session_id = resp["result"]["session_id"].as_str().unwrap().to_string();

    // 2. List sessions
    let resp = send_request(&mut ws, "2", "session.list", json!({})).await;
    let sessions = resp["result"]["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0]["session_id"].as_str().unwrap(), session_id);

    // 3. Session info
    let resp = send_request(&mut ws, "3", "session.info", json!({})).await;
    assert_eq!(resp["result"]["session_id"].as_str().unwrap(), session_id);

    // 4. Try creating an endpoint offer (now implemented!)
    let resp = send_request(
        &mut ws,
        "4",
        "endpoint.create_offer",
        json!({
            "type": "webrtc",
            "direction": "sendrecv"
        }),
    )
    .await;
    assert!(
        !resp["result"]["endpoint_id"].as_str().unwrap().is_empty(),
        "expected endpoint_id in result: {resp}"
    );
    let sdp = resp["result"]["sdp_offer"].as_str().unwrap();
    assert!(sdp.contains("v=0"), "expected sdp_offer in result: {resp}");

    // 5. Unknown method
    let resp = send_request(&mut ws, "5", "foo.bar", json!({})).await;
    assert_eq!(resp["error"]["code"].as_str().unwrap(), "UNKNOWN_METHOD");

    // 6. Destroy session
    let resp = send_request(&mut ws, "6", "session.destroy", json!({})).await;
    assert!(resp["result"].is_object(), "expected empty result: {resp}");

    // 7. List should be empty now
    // Need a new connection since destroy unbinds
    let _resp = send_request(&mut ws, "7", "session.list", json!({})).await;
    // Give the session task time to clean up
    tokio::time::sleep(timing::scaled_ms(100)).await;

    // Close and reconnect to verify list is empty
    ws.close(None).await.ok();
    let mut ws2 = connect_with_retry(addr).await;
    let resp = send_request(&mut ws2, "8", "session.list", json!({})).await;
    let sessions = resp["result"]["sessions"].as_array().unwrap();
    assert_eq!(
        sessions.len(),
        0,
        "expected 0 sessions after destroy: {resp}"
    );

    ws2.close(None).await.ok();
}

#[tokio::test]
async fn test_session_create_requires_no_existing() {
    let server = TestServer::start().await;
    let addr = &server.addr;

    let mut ws = connect_with_retry(addr).await;

    // Create first session
    let resp = send_request(&mut ws, "1", "session.create", json!({})).await;
    assert!(!resp["result"]["session_id"].as_str().unwrap().is_empty());

    // Try to create second session on same connection
    let resp = send_request(&mut ws, "2", "session.create", json!({})).await;
    assert_eq!(
        resp["error"]["code"].as_str().unwrap(),
        "SESSION_ALREADY_BOUND"
    );

    ws.close(None).await.ok();
}

#[tokio::test]
async fn test_plain_rtp_endpoint() {
    let server = TestServer::start().await;
    let addr = &server.addr;

    let mut ws = connect_with_retry(addr).await;

    // Create session
    let resp = send_request(&mut ws, "1", "session.create", json!({})).await;
    assert!(!resp["result"]["session_id"].as_str().unwrap().is_empty());

    // Create a plain RTP endpoint offer
    let resp = send_request(
        &mut ws,
        "2",
        "endpoint.create_offer",
        json!({
            "type": "rtp",
            "direction": "sendrecv"
        }),
    )
    .await;
    assert!(
        !resp["result"]["endpoint_id"].as_str().unwrap().is_empty(),
        "expected endpoint_id: {resp}"
    );
    let sdp = resp["result"]["sdp_offer"].as_str().unwrap();
    assert!(sdp.contains("v=0"), "expected sdp_offer: {resp}");

    assert!(sdp.contains("m=audio"), "SDP should have audio m-line");
    assert!(sdp.contains("RTP/AVP"), "SDP should use RTP/AVP");
    assert!(sdp.contains("PCMU"), "SDP should offer PCMU");
    assert!(
        sdp.contains("telephone-event"),
        "SDP should offer telephone-event"
    );

    // Create a plain RTP endpoint with SRTP
    let resp = send_request(
        &mut ws,
        "3",
        "endpoint.create_offer",
        json!({
            "type": "rtp",
            "direction": "sendrecv",
            "srtp": true
        }),
    )
    .await;
    let sdp_srtp = resp["result"]["sdp_offer"].as_str().unwrap();
    assert!(
        sdp_srtp.contains("RTP/SAVP"),
        "SRTP SDP should use RTP/SAVP"
    );
    assert!(
        sdp_srtp.contains("a=crypto:"),
        "SRTP SDP should have crypto"
    );

    // Create endpoint from a plain RTP offer (auto-detect)
    let fake_offer = "v=0\r\n\
        o=- 123 1 IN IP4 192.168.1.100\r\n\
        s=-\r\n\
        c=IN IP4 192.168.1.100\r\n\
        t=0 0\r\n\
        m=audio 20000 RTP/AVP 0 101\r\n\
        a=rtpmap:0 PCMU/8000\r\n\
        a=rtpmap:101 telephone-event/8000\r\n\
        a=fmtp:101 0-16\r\n\
        a=sendrecv\r\n";

    let resp = send_request(
        &mut ws,
        "4",
        "endpoint.create_from_offer",
        json!({
            "sdp": fake_offer,
            "direction": "sendrecv"
        }),
    )
    .await;
    assert!(
        !resp["result"]["endpoint_id"].as_str().unwrap().is_empty(),
        "expected endpoint_id from offer: {resp}"
    );
    let answer = resp["result"]["sdp_answer"].as_str().unwrap();
    assert!(answer.contains("v=0"), "expected sdp_answer: {resp}");
    assert!(answer.contains("RTP/AVP"), "answer should be plain RTP");

    ws.close(None).await.ok();
}

/// Test: session.list returns all sessions with correct state and endpoint counts
#[tokio::test]
async fn test_session_list_multiple_sessions() {
    let server = TestServer::start().await;

    let mut client1 = TestControlClient::connect(&server.addr).await;
    let mut client2 = TestControlClient::connect(&server.addr).await;
    let mut client3 = TestControlClient::connect(&server.addr).await;

    let r1 = client1.request_ok("session.create", json!({})).await;
    let r2 = client2.request_ok("session.create", json!({})).await;
    let r3 = client3.request_ok("session.create", json!({})).await;

    let sid1 = r1["session_id"].as_str().unwrap().to_string();
    let sid2 = r2["session_id"].as_str().unwrap().to_string();
    let sid3 = r3["session_id"].as_str().unwrap().to_string();

    // List should show all 3 sessions
    let list = client1.request_ok("session.list", json!({})).await;
    let sessions = list["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 3, "should list 3 sessions");

    let ids: Vec<&str> = sessions
        .iter()
        .map(|s| s["session_id"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&sid1.as_str()));
    assert!(ids.contains(&sid2.as_str()));
    assert!(ids.contains(&sid3.as_str()));

    // All active, 0 endpoints
    for s in sessions {
        assert_eq!(s["state"].as_str().unwrap(), "active");
        assert_eq!(s["endpoint_count"].as_u64().unwrap(), 0);
    }

    // Add endpoint to session 1, verify endpoint_count updates
    client1
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv"}),
        )
        .await;

    let list = client1.request_ok("session.list", json!({})).await;
    let sessions = list["sessions"].as_array().unwrap();
    let s1 = sessions
        .iter()
        .find(|s| s["session_id"].as_str().unwrap() == sid1)
        .unwrap();
    assert_eq!(s1["endpoint_count"].as_u64().unwrap(), 1);

    // Destroy session 2 → list shows 2
    client2.request_ok("session.destroy", json!({})).await;
    tokio::time::sleep(timing::scaled_ms(200)).await;

    let list = client1.request_ok("session.list", json!({})).await;
    let sessions = list["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 2);
    let ids: Vec<&str> = sessions
        .iter()
        .map(|s| s["session_id"].as_str().unwrap())
        .collect();
    assert!(!ids.contains(&sid2.as_str()));

    client1.request_ok("session.destroy", json!({})).await;
    client3.request_ok("session.destroy", json!({})).await;
}

/// Test: session.list shows orphaned state after client disconnect, and active after re-attach
#[tokio::test]
async fn test_session_list_shows_orphaned_state() {
    let server = TestServer::builder()
        .disconnect_timeout_secs(60)
        .start()
        .await;

    let mut client1 = TestControlClient::connect(&server.addr).await;
    let r = client1.request_ok("session.create", json!({})).await;
    let sid = r["session_id"].as_str().unwrap().to_string();

    // Close client → session becomes orphaned
    client1.close().await;
    tokio::time::sleep(timing::scaled_ms(500)).await;

    // Verify orphaned from another connection
    let mut client2 = TestControlClient::connect(&server.addr).await;
    let list = client2.request_ok("session.list", json!({})).await;
    let sessions = list["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0]["state"].as_str().unwrap(), "orphaned");

    // Re-attach → active again
    client2
        .request_ok("session.attach", json!({"session_id": sid}))
        .await;

    let list = client2.request_ok("session.list", json!({})).await;
    let sessions = list["sessions"].as_array().unwrap();
    assert_eq!(sessions[0]["state"].as_str().unwrap(), "active");

    client2.request_ok("session.destroy", json!({})).await;
}
