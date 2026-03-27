mod helpers;

use helpers::control_client::TestControlClient;
use helpers::test_server::TestServer;
use serde_json::json;
use std::time::Duration;

#[tokio::test]
async fn test_bridge_creation() {
    let server = TestServer::start().await;

    let mut client_a = TestControlClient::connect(&server.addr).await;
    let mut client_b = TestControlClient::connect(&server.addr).await;

    client_a.request_ok("session.create", json!({})).await;
    let r_b = client_b.request_ok("session.create", json!({})).await;
    let session_b_id = r_b["session_id"].as_str().unwrap().to_string();

    // Create bridge
    let result = client_a
        .request_ok("session.bridge", json!({"target_session_id": session_b_id}))
        .await;

    assert!(result["endpoint_id"].as_str().is_some());
    assert!(result["target_endpoint_id"].as_str().is_some());

    // Verify both sessions show bridge endpoints
    let info_a = client_a.request_ok("session.info", json!({})).await;
    assert_eq!(info_a["endpoints"].as_array().unwrap().len(), 1);
    assert_eq!(
        info_a["endpoints"][0]["endpoint_type"].as_str().unwrap(),
        "bridge"
    );

    let info_b = client_b.request_ok("session.info", json!({})).await;
    assert_eq!(info_b["endpoints"].as_array().unwrap().len(), 1);
    assert_eq!(
        info_b["endpoints"][0]["endpoint_type"].as_str().unwrap(),
        "bridge"
    );
}

#[tokio::test]
async fn test_bridge_remove_source_side() {
    let server = TestServer::start().await;

    let mut client_a = TestControlClient::connect(&server.addr).await;
    let mut client_b = TestControlClient::connect(&server.addr).await;

    client_a.request_ok("session.create", json!({})).await;
    let r_b = client_b.request_ok("session.create", json!({})).await;
    let session_b_id = r_b["session_id"].as_str().unwrap().to_string();

    let result = client_a
        .request_ok("session.bridge", json!({"target_session_id": session_b_id}))
        .await;
    let ep_a_id = result["endpoint_id"].as_str().unwrap().to_string();

    // Remove bridge endpoint from source session
    client_a
        .request_ok("endpoint.remove", json!({"endpoint_id": ep_a_id}))
        .await;

    // Give it a moment for the auto-remove to propagate
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Both sessions should have zero endpoints
    let info_a = client_a.request_ok("session.info", json!({})).await;
    assert_eq!(info_a["endpoints"].as_array().unwrap().len(), 0);

    let info_b = client_b.request_ok("session.info", json!({})).await;
    assert_eq!(
        info_b["endpoints"].as_array().unwrap().len(),
        0,
        "paired bridge endpoint should be auto-removed"
    );
}

#[tokio::test]
async fn test_bridge_remove_target_side() {
    let server = TestServer::start().await;

    let mut client_a = TestControlClient::connect(&server.addr).await;
    let mut client_b = TestControlClient::connect(&server.addr).await;

    client_a.request_ok("session.create", json!({})).await;
    let r_b = client_b.request_ok("session.create", json!({})).await;
    let session_b_id = r_b["session_id"].as_str().unwrap().to_string();

    let result = client_a
        .request_ok("session.bridge", json!({"target_session_id": session_b_id}))
        .await;
    let ep_b_id = result["target_endpoint_id"].as_str().unwrap().to_string();

    // Remove bridge from target side
    client_b
        .request_ok("endpoint.remove", json!({"endpoint_id": ep_b_id}))
        .await;

    tokio::time::sleep(Duration::from_millis(100)).await;

    // Both sessions should have zero endpoints
    let info_a = client_a.request_ok("session.info", json!({})).await;
    assert_eq!(
        info_a["endpoints"].as_array().unwrap().len(),
        0,
        "paired bridge endpoint should be auto-removed"
    );

    let info_b = client_b.request_ok("session.info", json!({})).await;
    assert_eq!(info_b["endpoints"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn test_bridge_nonexistent_session() {
    let server = TestServer::start().await;

    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;

    let resp = client
        .request(
            "session.bridge",
            json!({"target_session_id": "00000000-0000-0000-0000-000000000000"}),
        )
        .await;
    assert!(resp.get("error").is_some());
    assert_eq!(resp["error"]["code"].as_str().unwrap(), "SESSION_NOT_FOUND");
}

#[tokio::test]
async fn test_bridge_self() {
    let server = TestServer::start().await;

    let mut client = TestControlClient::connect(&server.addr).await;
    let r = client.request_ok("session.create", json!({})).await;
    let session_id = r["session_id"].as_str().unwrap().to_string();

    let resp = client
        .request("session.bridge", json!({"target_session_id": session_id}))
        .await;
    assert!(resp.get("error").is_some());
    assert_eq!(resp["error"]["code"].as_str().unwrap(), "INVALID_PARAMS");
}

#[tokio::test]
async fn test_bridge_target_full() {
    let server = TestServer::builder()
        .max_endpoints_per_session(1)
        .start()
        .await;

    let mut client_a = TestControlClient::connect(&server.addr).await;
    let mut client_b = TestControlClient::connect(&server.addr).await;

    client_a.request_ok("session.create", json!({})).await;
    let r_b = client_b.request_ok("session.create", json!({})).await;
    let session_b_id = r_b["session_id"].as_str().unwrap().to_string();

    // Fill session B
    client_b
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv"}),
        )
        .await;

    // Bridge should fail (target full)
    let resp = client_a
        .request("session.bridge", json!({"target_session_id": session_b_id}))
        .await;
    assert!(resp.get("error").is_some());

    // Source session should have NO bridge endpoint (cleaned up)
    let info_a = client_a.request_ok("session.info", json!({})).await;
    assert_eq!(
        info_a["endpoints"].as_array().unwrap().len(),
        0,
        "bridge endpoint in source should be cleaned up on target failure"
    );
}

#[tokio::test]
async fn test_bridge_no_bridge_to_bridge_routing() {
    let server = TestServer::start().await;

    let mut client_a = TestControlClient::connect(&server.addr).await;
    let mut client_b = TestControlClient::connect(&server.addr).await;
    let mut client_c = TestControlClient::connect(&server.addr).await;

    client_a.request_ok("session.create", json!({})).await;
    let r_b = client_b.request_ok("session.create", json!({})).await;
    let session_b_id = r_b["session_id"].as_str().unwrap().to_string();
    let r_c = client_c.request_ok("session.create", json!({})).await;
    let session_c_id = r_c["session_id"].as_str().unwrap().to_string();

    // Create A↔B bridge
    client_a
        .request_ok("session.bridge", json!({"target_session_id": session_b_id}))
        .await;

    // Create B↔C bridge
    client_b
        .request_ok("session.bridge", json!({"target_session_id": session_c_id}))
        .await;

    // Session B now has 2 bridge endpoints (one to A, one to C)
    let info_b = client_b.request_ok("session.info", json!({})).await;
    assert_eq!(info_b["endpoints"].as_array().unwrap().len(), 2);

    // Both should be bridge type — the routing table should NOT route
    // bridge-to-bridge, which prevents audio loops
    for ep in info_b["endpoints"].as_array().unwrap() {
        assert_eq!(ep["endpoint_type"].as_str().unwrap(), "bridge");
    }
}

// ── Empty session timeout tests ─────────────────────────────────────────

#[tokio::test]
async fn test_empty_session_timeout_fires() {
    let server = TestServer::builder()
        .empty_session_timeout_secs(2)
        .start()
        .await;

    let mut client = TestControlClient::connect(&server.addr).await;
    let r = client.request_ok("session.create", json!({})).await;
    let _session_id = r["session_id"].as_str().unwrap();

    // Create and remove an endpoint
    let ep = client
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv"}),
        )
        .await;
    let ep_id = ep["endpoint_id"].as_str().unwrap();
    client
        .request_ok("endpoint.remove", json!({"endpoint_id": ep_id}))
        .await;

    // Wait for empty timeout (1s configured + margin)
    // Use recv_event and scan for the right event type
    let mut found = false;
    for _ in 0..10 {
        if let Some(event) = client.recv_event(Duration::from_secs(1)).await {
            if event["event"].as_str() == Some("session.empty_timeout") {
                found = true;
                break;
            }
        }
    }
    assert!(found, "should receive empty_timeout event");
}

#[tokio::test]
async fn test_empty_session_timeout_reset() {
    let server = TestServer::builder()
        .empty_session_timeout_secs(2)
        .start()
        .await;

    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;

    // Create endpoint, remove it, wait 1s, create another
    let ep = client
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv"}),
        )
        .await;
    let ep_id = ep["endpoint_id"].as_str().unwrap().to_string();
    client
        .request_ok("endpoint.remove", json!({"endpoint_id": ep_id}))
        .await;

    tokio::time::sleep(Duration::from_millis(500)).await;

    // Add a new endpoint — should reset the empty timer
    let ep2 = client
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv"}),
        )
        .await;
    assert!(
        ep2["endpoint_id"].as_str().is_some(),
        "session should still be alive"
    );
}

#[tokio::test]
async fn test_empty_session_timeout_disabled() {
    let server = TestServer::start().await; // default: 0 = disabled

    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;

    // Session with no endpoints should persist
    tokio::time::sleep(Duration::from_millis(500)).await;

    let info = client.request_ok("session.info", json!({})).await;
    assert!(
        info.get("endpoints").is_some(),
        "session should still be alive with disabled empty timeout"
    );
}

#[tokio::test]
async fn test_empty_session_timeout_at_creation() {
    let server = TestServer::builder()
        .empty_session_timeout_secs(2)
        .start()
        .await;

    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;

    // Session starts with zero endpoints — timeout should fire (1s configured + margin)
    let mut found = false;
    for _ in 0..10 {
        if let Some(event) = client.recv_event(Duration::from_secs(1)).await {
            if event["event"].as_str() == Some("session.empty_timeout") {
                found = true;
                break;
            }
        }
    }
    assert!(
        found,
        "should receive empty_timeout even when no endpoint was ever added"
    );
}
