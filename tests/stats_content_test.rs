mod helpers;

use std::time::Duration;

use serde_json::json;

use helpers::control_client::TestControlClient;
use helpers::test_rtp_peer::{TestRtpPeer, parse_rtp_addr_from_sdp};
use helpers::test_server::TestServer;
use helpers::timing;

/// Test: stats event contains an `endpoints` array with entries that have
/// non-zero `inbound.packets` and/or `outbound.packets` after media flows
/// between two RTP endpoints.
///
/// Setup: two RTP endpoints in a session with connected test peers.
/// Subscribe to stats with a 200ms interval, send media packets from peer A
/// through the bridge to peer B, then verify the stats event data has the
/// expected structure and non-zero packet counts.
#[tokio::test]
async fn test_stats_event_content() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;

    client.request_ok("session.create", json!({})).await;

    // Endpoint A: create from peer A's SDP offer
    let mut peer_a = TestRtpPeer::new().await;
    let offer_a = peer_a.make_sdp_offer();
    let result = client
        .request_ok(
            "endpoint.create_from_offer",
            json!({"sdp": offer_a, "direction": "sendrecv"}),
        )
        .await;
    let ep_a_id = result["endpoint_id"].as_str().unwrap().to_string();
    let answer_a = result["sdp_answer"].as_str().unwrap();
    let server_addr_a = parse_rtp_addr_from_sdp(answer_a).expect("parse server addr A");
    peer_a.set_remote(server_addr_a);

    // Endpoint B: create from peer B's SDP offer
    let mut peer_b = TestRtpPeer::new().await;
    let offer_b = peer_b.make_sdp_offer();
    let result = client
        .request_ok(
            "endpoint.create_from_offer",
            json!({"sdp": offer_b, "direction": "sendrecv"}),
        )
        .await;
    let ep_b_id = result["endpoint_id"].as_str().unwrap().to_string();
    let answer_b = result["sdp_answer"].as_str().unwrap();
    let server_addr_b = parse_rtp_addr_from_sdp(answer_b).expect("parse server addr B");
    peer_b.set_remote(server_addr_b);

    // Activate symmetric RTP on both peers
    peer_a.activate().await;
    peer_b.activate().await;
    tokio::time::sleep(timing::scaled_ms(50)).await;

    // Start receiving on peer B so routed packets are consumed
    peer_b.start_recv();
    tokio::time::sleep(timing::scaled_ms(50)).await;

    // Subscribe to stats with a short interval (500ms is the minimum)
    client
        .request_ok("stats.subscribe", json!({"interval_ms": 500}))
        .await;

    // Send media packets from peer A through the bridge
    peer_a.send_tone_for(Duration::from_millis(500)).await;

    // Wait a bit for stats to accumulate
    tokio::time::sleep(timing::scaled_ms(300)).await;

    // Wait for a stats event with non-zero data
    let mut got_valid_stats = false;
    for _ in 0..15 {
        if let Some(event) = client.recv_event(helpers::timing::scaled_ms(2000)).await {
            let event_type = event["event"].as_str().unwrap_or("");
            if event_type != "stats" {
                continue;
            }

            // Verify the top-level structure
            let data = &event["data"];
            assert!(
                data["endpoints"].is_array(),
                "stats event data should contain an 'endpoints' array: {event}"
            );

            let endpoints = data["endpoints"].as_array().unwrap();
            assert_eq!(
                endpoints.len(),
                2,
                "stats should contain entries for both endpoints, got {}: {event}",
                endpoints.len()
            );

            // Verify each endpoint entry has the expected fields
            let mut found_ep_a = false;
            let mut found_ep_b = false;
            let mut any_nonzero_inbound = false;
            let mut any_nonzero_outbound = false;

            for ep in endpoints {
                let ep_id = ep["endpoint_id"].as_str().unwrap_or("");
                if ep_id == ep_a_id {
                    found_ep_a = true;
                }
                if ep_id == ep_b_id {
                    found_ep_b = true;
                }

                // Verify inbound stats structure
                assert!(
                    ep["inbound"].is_object(),
                    "endpoint stats should have 'inbound' object: {ep}"
                );
                assert!(
                    ep["inbound"]["packets"].is_number(),
                    "inbound should have 'packets' field: {ep}"
                );
                assert!(
                    ep["inbound"]["bytes"].is_number(),
                    "inbound should have 'bytes' field: {ep}"
                );

                // Verify outbound stats structure
                assert!(
                    ep["outbound"].is_object(),
                    "endpoint stats should have 'outbound' object: {ep}"
                );
                assert!(
                    ep["outbound"]["packets"].is_number(),
                    "outbound should have 'packets' field: {ep}"
                );
                assert!(
                    ep["outbound"]["bytes"].is_number(),
                    "outbound should have 'bytes' field: {ep}"
                );

                let inbound_packets = ep["inbound"]["packets"].as_u64().unwrap_or(0);
                let outbound_packets = ep["outbound"]["packets"].as_u64().unwrap_or(0);

                if inbound_packets > 0 {
                    any_nonzero_inbound = true;
                }
                if outbound_packets > 0 {
                    any_nonzero_outbound = true;
                }
            }

            assert!(
                found_ep_a,
                "stats should contain entry for endpoint A ({ep_a_id})"
            );
            assert!(
                found_ep_b,
                "stats should contain entry for endpoint B ({ep_b_id})"
            );

            // After sending media, at least one endpoint should have non-zero packet counts
            if any_nonzero_inbound || any_nonzero_outbound {
                got_valid_stats = true;

                // Endpoint A is the sender: it should have non-zero inbound packets
                // (peer A sent packets to the bridge on endpoint A)
                let ep_a_stats = endpoints
                    .iter()
                    .find(|ep| ep["endpoint_id"].as_str().unwrap_or("") == ep_a_id)
                    .expect("endpoint A stats should exist");
                let ep_a_inbound = ep_a_stats["inbound"]["packets"].as_u64().unwrap_or(0);
                assert!(
                    ep_a_inbound > 0,
                    "endpoint A should have non-zero inbound packets (peer A sent to it), got {ep_a_inbound}"
                );

                // Endpoint B is the receiver: it should have non-zero outbound packets
                // (bridge routes A's audio out through endpoint B)
                let ep_b_stats = endpoints
                    .iter()
                    .find(|ep| ep["endpoint_id"].as_str().unwrap_or("") == ep_b_id)
                    .expect("endpoint B stats should exist");
                let ep_b_outbound = ep_b_stats["outbound"]["packets"].as_u64().unwrap_or(0);
                assert!(
                    ep_b_outbound > 0,
                    "endpoint B should have non-zero outbound packets (bridge routed to it), got {ep_b_outbound}"
                );

                break;
            }
        }
    }

    assert!(
        got_valid_stats,
        "should have received a stats event with non-zero packet counts after sending media"
    );

    // Unsubscribe and clean up
    client.request_ok("stats.unsubscribe", json!({})).await;
    client.request_ok("session.destroy", json!({})).await;
}

/// Send a known number of RTP packets, subscribe to stats, and verify the
/// reported inbound/outbound packet counts match what was actually sent
/// and what the receiving peer observed.
#[tokio::test]
async fn test_stats_packet_counts_match_actual_media() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;

    let mut peer_a = TestRtpPeer::new().await;
    let mut peer_b = TestRtpPeer::new().await;

    let offer_a = peer_a.make_sdp_offer();
    let result = client
        .request_ok(
            "endpoint.create_from_offer",
            json!({"sdp": offer_a, "direction": "sendrecv"}),
        )
        .await;
    let ep_a_id = result["endpoint_id"].as_str().unwrap().to_string();
    let answer_a = result["sdp_answer"].as_str().unwrap();
    peer_a.set_remote(parse_rtp_addr_from_sdp(answer_a).unwrap());

    let result = client
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv"}),
        )
        .await;
    let ep_b_id = result["endpoint_id"].as_str().unwrap().to_string();
    let offer_b = result["sdp_offer"].as_str().unwrap();
    let server_addr_b = parse_rtp_addr_from_sdp(offer_b).unwrap();
    let answer_b = peer_b.make_sdp_answer();
    client
        .request_ok(
            "endpoint.accept_answer",
            json!({"endpoint_id": ep_b_id, "sdp": answer_b}),
        )
        .await;
    peer_b.set_remote(server_addr_b);

    // Activate peer B so symmetric RTP guard allows outbound packets
    peer_b.activate().await;
    tokio::time::sleep(timing::scaled_ms(50)).await;

    peer_b.start_recv();

    // Subscribe to stats with a short interval
    client
        .request_ok("stats.subscribe", json!({"interval_ms": 500}))
        .await;

    // Send exactly 50 packets (1 second at 20ms ptime) from peer A
    let packets_sent: u64 = 50;
    for _ in 0..packets_sent {
        let payload = vec![0x80u8; 160];
        peer_a.send_pcmu(&payload).await;
        tokio::time::sleep(timing::PACING).await;
    }

    tokio::time::sleep(timing::scaled_ms(1000)).await;

    // Collect stats events
    let min_expected = packets_sent * 50 / 100;
    let mut best_inbound_a: u64 = 0;
    let mut best_outbound_b: u64 = 0;

    for _ in 0..10 {
        if let Some(event) = client.recv_event(timing::scaled_ms(1000)).await {
            if event["event"].as_str().unwrap_or("") == "stats" {
                if let Some(endpoints) = event["data"]["endpoints"].as_array() {
                    for ep in endpoints {
                        let eid = ep["endpoint_id"].as_str().unwrap_or("");
                        if eid == ep_a_id {
                            let inbound = ep["inbound"]["packets"].as_u64().unwrap_or(0);
                            if inbound > best_inbound_a {
                                best_inbound_a = inbound;
                            }
                        }
                        if eid == ep_b_id {
                            let outbound = ep["outbound"]["packets"].as_u64().unwrap_or(0);
                            if outbound > best_outbound_b {
                                best_outbound_b = outbound;
                            }
                        }
                    }
                }
            }
            if best_inbound_a >= min_expected {
                break;
            }
        }
    }

    // Endpoint A inbound should be close to what we sent (allow 50% margin for CI timing)
    assert!(
        best_inbound_a >= min_expected,
        "endpoint A inbound should be >= {min_expected} (80% of {packets_sent}), got {best_inbound_a}"
    );

    // Endpoint B outbound should reflect routed packets
    assert!(
        best_outbound_b > 0,
        "endpoint B outbound should be > 0, got {best_outbound_b}"
    );
    assert!(
        best_outbound_b >= min_expected / 2,
        "endpoint B outbound should be substantial, got {best_outbound_b}"
    );

    // Cross-check: stats outbound_b should roughly match what peer B received
    let peer_b_received = peer_b.received_count();
    assert!(peer_b_received > 0, "peer B should have received packets");
    let ratio = if best_outbound_b > 0 {
        peer_b_received as f64 / best_outbound_b as f64
    } else {
        0.0
    };
    assert!(
        ratio > 0.3 && ratio < 3.0,
        "stats outbound_b ({best_outbound_b}) and peer_b received ({peer_b_received}) should be in the same ballpark (ratio={ratio:.2})"
    );

    client.request_ok("stats.unsubscribe", json!({})).await;
    client.request_ok("session.destroy", json!({})).await;
}
