mod helpers;

use serde_json::json;

use helpers::control_client::TestControlClient;
use helpers::test_server::TestServer;
use helpers::timing;

/// Test: rapid disconnect and reconnect with session.attach succeeds
#[tokio::test]
async fn test_rapid_disconnect_reconnect() {
    let server = TestServer::builder()
        .disconnect_timeout_secs(60)
        .start()
        .await;

    let mut client = TestControlClient::connect(&server.addr).await;
    let r = client.request_ok("session.create", json!({})).await;
    let sid = r["session_id"].as_str().unwrap().to_string();

    // Disconnect and wait for orphan state
    client.close().await;
    tokio::time::sleep(timing::scaled_ms(200)).await;

    // Reconnect and re-attach
    let mut client2 = TestControlClient::connect(&server.addr).await;
    client2
        .request_ok("session.attach", json!({"session_id": sid}))
        .await;

    // Session should be active
    let list = client2.request_ok("session.list", json!({})).await;
    let sessions = list["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0]["state"].as_str().unwrap(), "active");

    client2.request_ok("session.destroy", json!({})).await;
}

/// Test: attaching to a non-existent session returns error
#[tokio::test]
async fn test_attach_nonexistent_session() {
    let server = TestServer::start().await;

    let mut client = TestControlClient::connect(&server.addr).await;
    let r = client
        .request(
            "session.attach",
            json!({"session_id": "00000000-0000-0000-0000-000000000000"}),
        )
        .await;
    assert!(
        r["error"]["code"].as_str().is_some(),
        "attaching non-existent session should fail: {r}"
    );
}

/// Test: double-attach to same session from different connections
#[tokio::test]
async fn test_double_attach_rejected() {
    let server = TestServer::builder()
        .disconnect_timeout_secs(60)
        .start()
        .await;

    let mut client1 = TestControlClient::connect(&server.addr).await;
    let r = client1.request_ok("session.create", json!({})).await;
    let sid = r["session_id"].as_str().unwrap().to_string();

    // Second client tries to attach to the same (non-orphaned) session
    let mut client2 = TestControlClient::connect(&server.addr).await;
    let r = client2
        .request("session.attach", json!({"session_id": sid}))
        .await;
    assert!(
        r["error"]["code"].as_str().is_some(),
        "double attach should fail while session is active: {r}"
    );

    client1.request_ok("session.destroy", json!({})).await;
}

/// Test: orphan event fires on disconnect, then session is reclaimable
#[tokio::test]
async fn test_orphan_event_on_disconnect() {
    let server = TestServer::builder()
        .disconnect_timeout_secs(60)
        .start()
        .await;

    let mut client = TestControlClient::connect(&server.addr).await;
    let r = client.request_ok("session.create", json!({})).await;
    let sid = r["session_id"].as_str().unwrap().to_string();

    // Close connection
    client.close().await;
    tokio::time::sleep(timing::scaled_ms(300)).await;

    // Verify session is orphaned
    let mut client2 = TestControlClient::connect(&server.addr).await;
    let list = client2.request_ok("session.list", json!({})).await;
    let sessions = list["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0]["session_id"].as_str().unwrap(), sid);
    assert_eq!(sessions[0]["state"].as_str().unwrap(), "orphaned");

    // Re-attach succeeds
    client2
        .request_ok("session.attach", json!({"session_id": sid}))
        .await;

    let list = client2.request_ok("session.list", json!({})).await;
    assert_eq!(list["sessions"][0]["state"].as_str().unwrap(), "active");

    client2.request_ok("session.destroy", json!({})).await;
}

/// Test: rapid multiple disconnect/reconnect cycles don't leak sessions
#[tokio::test]
async fn test_rapid_reconnect_cycles() {
    let server = TestServer::builder()
        .disconnect_timeout_secs(60)
        .start()
        .await;

    let mut client = TestControlClient::connect(&server.addr).await;
    let r = client.request_ok("session.create", json!({})).await;
    let sid = r["session_id"].as_str().unwrap().to_string();

    // Do 5 rapid disconnect/reconnect cycles
    for _ in 0..5 {
        client.close().await;
        tokio::time::sleep(timing::scaled_ms(50)).await;

        client = TestControlClient::connect(&server.addr).await;
        client
            .request_ok("session.attach", json!({"session_id": sid}))
            .await;
    }

    // Session should still be active with no leaks
    let list = client.request_ok("session.list", json!({})).await;
    let sessions = list["sessions"].as_array().unwrap();
    assert_eq!(
        sessions.len(),
        1,
        "should have exactly 1 session after cycles"
    );
    assert_eq!(sessions[0]["state"].as_str().unwrap(), "active");

    client.request_ok("session.destroy", json!({})).await;
}

/// Test: operations on orphaned session fail until re-attached
#[tokio::test]
async fn test_operations_require_session() {
    let server = TestServer::start().await;

    let mut client = TestControlClient::connect(&server.addr).await;

    // Try operations without a session
    let r = client
        .request(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv"}),
        )
        .await;
    assert!(
        r["error"]["code"].as_str().is_some(),
        "endpoint create without session should fail: {r}"
    );

    let r = client.request("session.info", json!({})).await;
    assert!(
        r["error"]["code"].as_str().is_some(),
        "session.info without session should fail: {r}"
    );
}
