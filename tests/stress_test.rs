mod helpers;

use serde_json::json;

use helpers::control_client::TestControlClient;
use helpers::test_rtp_peer::{TestRtpPeer, parse_rtp_addr_from_sdp};
use helpers::test_server::TestServer;
use helpers::timing;

/// Stress test: create many concurrent sessions, each with 2 RTP endpoints,
/// verify session.list sees them all, then tear everything down.
#[tokio::test]
async fn test_stress_many_sessions() {
    let server = TestServer::start().await;
    let addr = &server.addr;

    let num_sessions: usize = 20;
    let mut clients = Vec::with_capacity(num_sessions);

    // ── Phase 1: Create N sessions, each with 2 RTP endpoints ─────────
    for i in 0..num_sessions {
        let mut client = TestControlClient::connect(&addr).await;

        let result = client.request_ok("session.create", json!({})).await;
        let session_id = result["session_id"].as_str().unwrap().to_string();
        assert!(!session_id.is_empty(), "session {i} should have an id");

        // Create first RTP endpoint
        let ep1 = client
            .request_ok(
                "endpoint.create_offer",
                json!({"type": "rtp", "direction": "sendrecv"}),
            )
            .await;
        assert!(
            !ep1["endpoint_id"].as_str().unwrap().is_empty(),
            "session {i} ep1 should have endpoint_id"
        );
        assert!(
            ep1["sdp_offer"].as_str().unwrap().contains("RTP/AVP"),
            "session {i} ep1 should have RTP SDP"
        );

        // Create second RTP endpoint
        let ep2 = client
            .request_ok(
                "endpoint.create_offer",
                json!({"type": "rtp", "direction": "sendrecv"}),
            )
            .await;
        assert!(
            !ep2["endpoint_id"].as_str().unwrap().is_empty(),
            "session {i} ep2 should have endpoint_id"
        );

        clients.push((client, session_id));
    }

    // ── Phase 2: Verify all sessions via session.list ─────────────────
    // session.list does not require a bound session, but we need a WS
    // connection to send the request.  We create a throwaway session for this.
    let mut list_client = TestControlClient::connect(&addr).await;
    list_client.request_ok("session.create", json!({})).await;

    let list_result = list_client.request_ok("session.list", json!({})).await;
    let sessions = list_result["sessions"].as_array().unwrap();
    // We created num_sessions + 1 (the list_client session)
    assert!(
        sessions.len() >= num_sessions,
        "expected at least {num_sessions} sessions, got {}",
        sessions.len()
    );

    // Destroy the list helper session
    list_client.request_ok("session.destroy", json!({})).await;

    // ── Phase 3: Verify each session has 2 endpoints via session.info ─
    for (i, (client, _session_id)) in clients.iter_mut().enumerate() {
        let info = client.request_ok("session.info", json!({})).await;
        let endpoints = info["endpoints"].as_array().unwrap();
        assert_eq!(
            endpoints.len(),
            2,
            "session {i} should have 2 endpoints, has {}",
            endpoints.len()
        );
    }

    // ── Phase 4: Destroy all sessions ─────────────────────────────────
    for (mut client, _session_id) in clients {
        client.request_ok("session.destroy", json!({})).await;
    }

    // Give the server a moment to clean up
    tokio::time::sleep(timing::scaled_ms(200)).await;

    // ── Phase 5: Verify all sessions are gone ─────────────────────────
    let mut verify_client = TestControlClient::connect(&addr).await;
    verify_client.request_ok("session.create", json!({})).await;

    let list_result = verify_client.request_ok("session.list", json!({})).await;
    let sessions = list_result["sessions"].as_array().unwrap();
    // Only the verify_client session should remain
    assert_eq!(
        sessions.len(),
        1,
        "expected 1 session after cleanup, got {}",
        sessions.len()
    );

    verify_client.request_ok("session.destroy", json!({})).await;
}

/// Stress test: rapidly create and destroy sessions to test lifecycle churn.
/// Verifies each iteration produces unique session/endpoint IDs and correct state.
#[tokio::test]
async fn test_stress_session_churn() {
    let server = TestServer::start().await;
    let addr = &server.addr;

    let iterations = 30;
    let mut seen_session_ids = std::collections::HashSet::new();
    let mut seen_endpoint_ids = std::collections::HashSet::new();

    for i in 0..iterations {
        let mut client = TestControlClient::connect(&addr).await;

        // Create session
        let result = client.request_ok("session.create", json!({})).await;
        let session_id = result["session_id"].as_str().unwrap().to_string();
        assert!(
            seen_session_ids.insert(session_id.clone()),
            "iteration {i}: session ID {session_id} was already used in a previous iteration"
        );

        // Verify session is visible
        let info = client.request_ok("session.info", json!({})).await;
        assert_eq!(
            info["session_id"].as_str().unwrap(),
            session_id,
            "iteration {i}: session.info should return the created session"
        );

        // Create an endpoint
        let ep = client
            .request_ok(
                "endpoint.create_offer",
                json!({"type": "rtp", "direction": "sendrecv"}),
            )
            .await;
        let ep_id = ep["endpoint_id"].as_str().unwrap().to_string();
        assert!(
            seen_endpoint_ids.insert(ep_id),
            "iteration {i}: endpoint ID was already used in a previous iteration"
        );

        // Verify endpoint is attached to session
        let info = client.request_ok("session.info", json!({})).await;
        assert_eq!(
            info["endpoints"].as_array().unwrap().len(),
            1,
            "iteration {i}: session should have exactly 1 endpoint before destroy"
        );

        // Immediately destroy
        client.request_ok("session.destroy", json!({})).await;
    }

    // Verify no sessions remain
    tokio::time::sleep(timing::scaled_ms(200)).await;

    let mut verify_client = TestControlClient::connect(&addr).await;
    verify_client.request_ok("session.create", json!({})).await;

    let list_result = verify_client.request_ok("session.list", json!({})).await;
    let sessions = list_result["sessions"].as_array().unwrap();
    assert_eq!(
        sessions.len(),
        1,
        "expected 1 session after churn, got {}",
        sessions.len()
    );

    verify_client.request_ok("session.destroy", json!({})).await;
}

/// Stress test: concurrent connections hitting the server simultaneously.
#[tokio::test]
async fn test_stress_concurrent_connections() {
    let server = TestServer::start().await;
    let addr = &server.addr;

    let num_concurrent = 15;
    let mut handles = Vec::with_capacity(num_concurrent);

    // Spawn many clients concurrently
    for _ in 0..num_concurrent {
        let addr = addr.clone();
        let handle = tokio::spawn(async move {
            let mut client = TestControlClient::connect(&addr).await;

            let result = client.request_ok("session.create", json!({})).await;
            let session_id = result["session_id"].as_str().unwrap().to_string();

            // Create an endpoint
            let ep = client
                .request_ok(
                    "endpoint.create_offer",
                    json!({"type": "rtp", "direction": "sendrecv"}),
                )
                .await;
            assert!(!ep["endpoint_id"].as_str().unwrap().is_empty());

            // Session info
            let info = client.request_ok("session.info", json!({})).await;
            assert_eq!(info["endpoints"].as_array().unwrap().len(), 1);

            // Destroy
            client.request_ok("session.destroy", json!({})).await;

            session_id
        });
        handles.push(handle);
    }

    // Await all tasks and verify they all succeeded
    let mut session_ids = Vec::new();
    for handle in handles {
        let id = handle.await.expect("task should not panic");
        session_ids.push(id);
    }

    // All session IDs should be unique
    session_ids.sort();
    session_ids.dedup();
    assert_eq!(
        session_ids.len(),
        num_concurrent,
        "all session IDs should be unique"
    );
}

/// Stress test: 50 concurrent sessions with real RTP media routing.
/// Verifies media flows across all sessions and resources are cleaned up.
#[tokio::test]
async fn test_stress_concurrent_media_sessions() {
    let server = TestServer::start().await;
    let addr = &server.addr;
    let num_sessions: usize = 50;

    struct SessionState {
        client: TestControlClient,
        peer_a: TestRtpPeer,
        peer_b: TestRtpPeer,
    }

    // Phase 1: Create all sessions with two connected RTP endpoints each
    let mut sessions = Vec::with_capacity(num_sessions);
    for _ in 0..num_sessions {
        let mut client = TestControlClient::connect(addr).await;
        client.request_ok("session.create", json!({})).await;

        // Endpoint A
        let mut peer_a = TestRtpPeer::new().await;
        let offer_a = peer_a.make_sdp_offer();
        let result = client
            .request_ok(
                "endpoint.create_from_offer",
                json!({"sdp": offer_a, "direction": "sendrecv"}),
            )
            .await;
        let answer_a = result["sdp_answer"].as_str().unwrap();
        peer_a.set_remote(parse_rtp_addr_from_sdp(answer_a).expect("parse addr A"));

        // Endpoint B
        let mut peer_b = TestRtpPeer::new().await;
        let offer_b = peer_b.make_sdp_offer();
        let result = client
            .request_ok(
                "endpoint.create_from_offer",
                json!({"sdp": offer_b, "direction": "sendrecv"}),
            )
            .await;
        let answer_b = result["sdp_answer"].as_str().unwrap();
        peer_b.set_remote(parse_rtp_addr_from_sdp(answer_b).expect("parse addr B"));

        peer_b.activate().await;
        tokio::time::sleep(timing::scaled_ms(50)).await;
        peer_b.start_recv();
        sessions.push(SessionState {
            client,
            peer_a,
            peer_b,
        });
    }

    // Phase 2: Verify all sessions are listed
    let mut list_client = TestControlClient::connect(addr).await;
    list_client.request_ok("session.create", json!({})).await;
    let list_result = list_client.request_ok("session.list", json!({})).await;
    let listed = list_result["sessions"].as_array().unwrap().len();
    assert!(
        listed >= num_sessions,
        "expected at least {num_sessions} sessions listed, got {listed}"
    );
    list_client.request_ok("session.destroy", json!({})).await;

    // Phase 3: Send media on all sessions simultaneously
    for session in &mut sessions {
        for _ in 0..10 {
            session.peer_a.send_pcmu(&[0x80u8; 160]).await;
        }
    }

    // Give packets time to route
    tokio::time::sleep(timing::scaled_ms(500)).await;

    // Phase 4: Verify media was received by all peer Bs
    let mut all_received = 0u64;
    for (i, session) in sessions.iter().enumerate() {
        let count = session.peer_b.received_count();
        assert!(
            count > 0,
            "session {i}: peer B should have received at least 1 packet, got {count}"
        );
        all_received += count;
    }
    assert!(
        all_received >= num_sessions as u64,
        "total received across all sessions should be >= {num_sessions}, got {all_received}"
    );

    // Phase 5: Destroy all sessions
    for mut session in sessions {
        session
            .client
            .request_ok("session.destroy", json!({}))
            .await;
    }

    // Phase 6: Verify cleanup
    tokio::time::sleep(timing::scaled_ms(300)).await;
    let mut verify_client = TestControlClient::connect(addr).await;
    verify_client.request_ok("session.create", json!({})).await;
    let list_result = verify_client.request_ok("session.list", json!({})).await;
    let remaining = list_result["sessions"].as_array().unwrap().len();
    assert_eq!(
        remaining, 1,
        "only the verify session should remain, got {remaining}"
    );
    verify_client.request_ok("session.destroy", json!({})).await;
}
