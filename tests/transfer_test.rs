mod helpers;

use helpers::control_client::TestControlClient;
use helpers::test_server::TestServer;
use serde_json::json;
use std::time::Duration;

#[tokio::test]
async fn test_transfer_rtp_endpoint() {
    let server = TestServer::start().await;

    let mut client_a = TestControlClient::connect(&server.addr).await;
    let mut client_b = TestControlClient::connect(&server.addr).await;

    let r_a = client_a.request_ok("session.create", json!({})).await;
    let r_b = client_b.request_ok("session.create", json!({})).await;
    let session_b_id = r_b["session_id"].as_str().unwrap().to_string();

    // Create an RTP endpoint in session A
    let ep = client_a
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv"}),
        )
        .await;
    let ep_id = ep["endpoint_id"].as_str().unwrap().to_string();

    // Verify it's in session A
    let info_a = client_a.request_ok("session.info", json!({})).await;
    assert_eq!(info_a["endpoints"].as_array().unwrap().len(), 1);

    // Transfer to session B
    let result = client_a
        .request_ok(
            "endpoint.transfer",
            json!({"endpoint_id": ep_id, "target_session_id": session_b_id}),
        )
        .await;
    assert_eq!(result["endpoint_id"].as_str().unwrap(), ep_id);

    // Verify it's gone from session A
    let info_a = client_a.request_ok("session.info", json!({})).await;
    assert_eq!(info_a["endpoints"].as_array().unwrap().len(), 0);

    // Verify it's in session B
    let info_b = client_b.request_ok("session.info", json!({})).await;
    assert_eq!(info_b["endpoints"].as_array().unwrap().len(), 1);
    assert_eq!(
        info_b["endpoints"][0]["endpoint_id"].as_str().unwrap(),
        ep_id
    );
}

#[tokio::test]
async fn test_transfer_nonexistent_endpoint() {
    let server = TestServer::start().await;

    let mut client_a = TestControlClient::connect(&server.addr).await;
    let mut client_b = TestControlClient::connect(&server.addr).await;

    client_a.request_ok("session.create", json!({})).await;
    let r_b = client_b.request_ok("session.create", json!({})).await;
    let session_b_id = r_b["session_id"].as_str().unwrap();

    let resp = client_a
        .request(
            "endpoint.transfer",
            json!({
                "endpoint_id": "00000000-0000-0000-0000-000000000000",
                "target_session_id": session_b_id
            }),
        )
        .await;
    assert!(resp.get("error").is_some());
}

#[tokio::test]
async fn test_transfer_nonexistent_session() {
    let server = TestServer::start().await;

    let mut client_a = TestControlClient::connect(&server.addr).await;
    client_a.request_ok("session.create", json!({})).await;

    let ep = client_a
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv"}),
        )
        .await;
    let ep_id = ep["endpoint_id"].as_str().unwrap();

    let resp = client_a
        .request(
            "endpoint.transfer",
            json!({
                "endpoint_id": ep_id,
                "target_session_id": "00000000-0000-0000-0000-000000000000"
            }),
        )
        .await;
    assert!(resp.get("error").is_some());
    assert_eq!(resp["error"]["code"].as_str().unwrap(), "SESSION_NOT_FOUND");
}

#[tokio::test]
async fn test_transfer_self() {
    let server = TestServer::start().await;

    let mut client = TestControlClient::connect(&server.addr).await;
    let r = client.request_ok("session.create", json!({})).await;
    let session_id = r["session_id"].as_str().unwrap().to_string();

    let ep = client
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv"}),
        )
        .await;
    let ep_id = ep["endpoint_id"].as_str().unwrap();

    let resp = client
        .request(
            "endpoint.transfer",
            json!({"endpoint_id": ep_id, "target_session_id": session_id}),
        )
        .await;
    assert!(resp.get("error").is_some());
    assert_eq!(resp["error"]["code"].as_str().unwrap(), "INVALID_PARAMS");
}

#[tokio::test]
async fn test_transfer_target_full() {
    let server = TestServer::builder()
        .max_endpoints_per_session(1)
        .start()
        .await;

    let mut client_a = TestControlClient::connect(&server.addr).await;
    let mut client_b = TestControlClient::connect(&server.addr).await;

    client_a.request_ok("session.create", json!({})).await;
    let r_b = client_b.request_ok("session.create", json!({})).await;
    let session_b_id = r_b["session_id"].as_str().unwrap().to_string();

    // Create endpoint in A
    let ep_a = client_a
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv"}),
        )
        .await;
    let ep_a_id = ep_a["endpoint_id"].as_str().unwrap().to_string();

    // Fill session B
    client_b
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv"}),
        )
        .await;

    // Transfer should fail (target full)
    let resp = client_a
        .request(
            "endpoint.transfer",
            json!({"endpoint_id": ep_a_id, "target_session_id": session_b_id}),
        )
        .await;
    assert!(resp.get("error").is_some());

    // Endpoint should be rolled back to source
    let info_a = client_a.request_ok("session.info", json!({})).await;
    assert_eq!(
        info_a["endpoints"].as_array().unwrap().len(),
        1,
        "endpoint should be rolled back to source session"
    );
}

#[tokio::test]
async fn test_transfer_no_session() {
    let server = TestServer::start().await;

    let mut client = TestControlClient::connect(&server.addr).await;

    let resp = client
        .request(
            "endpoint.transfer",
            json!({
                "endpoint_id": "00000000-0000-0000-0000-000000000000",
                "target_session_id": "00000000-0000-0000-0000-000000000000"
            }),
        )
        .await;
    assert!(resp.get("error").is_some());
    assert_eq!(resp["error"]["code"].as_str().unwrap(), "NO_SESSION");
}

#[tokio::test]
async fn test_transfer_events() {
    let server = TestServer::start().await;

    let mut client_a = TestControlClient::connect(&server.addr).await;
    let mut client_b = TestControlClient::connect(&server.addr).await;

    client_a.request_ok("session.create", json!({})).await;
    let r_b = client_b.request_ok("session.create", json!({})).await;
    let session_b_id = r_b["session_id"].as_str().unwrap().to_string();

    let ep = client_a
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv"}),
        )
        .await;
    let ep_id = ep["endpoint_id"].as_str().unwrap().to_string();

    // Transfer
    client_a
        .request_ok(
            "endpoint.transfer",
            json!({"endpoint_id": ep_id, "target_session_id": session_b_id}),
        )
        .await;

    // Source should get transferred_out event
    let event = client_a
        .recv_event_type("endpoint.transferred_out", Duration::from_secs(2))
        .await;
    assert!(event.is_some(), "should receive transferred_out event");

    // Target should get transferred_in event
    let event = client_b
        .recv_event_type("endpoint.transferred_in", Duration::from_secs(2))
        .await;
    assert!(event.is_some(), "should receive transferred_in event");
    let data = &event.unwrap()["data"];
    assert_eq!(data["endpoint_id"].as_str().unwrap(), ep_id);
    assert_eq!(data["endpoint_type"].as_str().unwrap(), "rtp");
}

#[tokio::test]
async fn test_transfer_source_session_info() {
    let server = TestServer::start().await;

    let mut client_a = TestControlClient::connect(&server.addr).await;
    let mut client_b = TestControlClient::connect(&server.addr).await;

    client_a.request_ok("session.create", json!({})).await;
    let r_b = client_b.request_ok("session.create", json!({})).await;
    let session_b_id = r_b["session_id"].as_str().unwrap().to_string();

    // Create 2 endpoints in A
    let ep1 = client_a
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv"}),
        )
        .await;
    let ep1_id = ep1["endpoint_id"].as_str().unwrap().to_string();

    client_a
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv"}),
        )
        .await;

    // Transfer one
    client_a
        .request_ok(
            "endpoint.transfer",
            json!({"endpoint_id": ep1_id, "target_session_id": session_b_id}),
        )
        .await;

    // Source should have 1 endpoint left
    let info_a = client_a.request_ok("session.info", json!({})).await;
    assert_eq!(info_a["endpoints"].as_array().unwrap().len(), 1);

    // Target should have 1 endpoint
    let info_b = client_b.request_ok("session.info", json!({})).await;
    assert_eq!(info_b["endpoints"].as_array().unwrap().len(), 1);
}
