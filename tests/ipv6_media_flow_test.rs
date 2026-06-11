mod helpers;

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use serde_json::json;

use helpers::control_client::TestControlClient;
use helpers::test_rtp_peer::{TestRtpPeer, parse_rtp_addr_from_sdp};
use helpers::test_server::TestServer;
use helpers::timing;

/// End-to-end IPv6 media flow: an IPv6-only server bridges two plain-RTP peers
/// that both speak over `::1`. Proves the v6 datapath — bind, per-family SDP
/// answer, and actual packet forwarding — not just the unit-level pieces.
///
/// Skips cleanly on hosts without an IPv6 loopback.
#[tokio::test]
async fn test_ipv6_rtp_to_rtp_media_flow() {
    let (Some(mut peer_a), Some(mut peer_b)) =
        (TestRtpPeer::new_v6().await, TestRtpPeer::new_v6().await)
    else {
        eprintln!("skipping test_ipv6_rtp_to_rtp_media_flow: no IPv6 loopback on this host");
        return;
    };

    let server = TestServer::builder()
        .media_ip(vec![IpAddr::V6(Ipv6Addr::LOCALHOST)])
        .start()
        .await;
    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;

    // Endpoint A: IPv6 offer must be answered with an IPv6 connection line,
    // allocated from the v6 pool.
    let offer_a = peer_a.make_sdp_offer();
    let res_a = client
        .request_ok(
            "endpoint.create_from_offer",
            json!({"sdp": offer_a, "direction": "sendrecv"}),
        )
        .await;
    let answer_a = res_a["sdp_answer"].as_str().unwrap();
    assert!(
        answer_a.contains("c=IN IP6"),
        "IPv6 offer must get an IPv6 answer; got:\n{answer_a}"
    );
    peer_a.set_remote(parse_rtp_addr_from_sdp(answer_a).expect("parse v6 server addr A"));

    // Endpoint B: same, the receiving side.
    let offer_b = peer_b.make_sdp_offer();
    let res_b = client
        .request_ok(
            "endpoint.create_from_offer",
            json!({"sdp": offer_b, "direction": "sendrecv"}),
        )
        .await;
    let answer_b = res_b["sdp_answer"].as_str().unwrap();
    assert!(
        answer_b.contains("c=IN IP6"),
        "IPv6 offer must get an IPv6 answer; got:\n{answer_b}"
    );
    peer_b.set_remote(parse_rtp_addr_from_sdp(answer_b).expect("parse v6 server addr B"));

    // B sends one packet so the server learns its remote SSRC (symmetric RTP),
    // then starts receiving.
    peer_b.activate().await;
    tokio::time::sleep(timing::scaled_ms(50)).await;
    peer_b.start_recv();
    tokio::time::sleep(timing::scaled_ms(50)).await;

    // A sends a known PCMU pattern. Both legs are PCMU so it forwards unchanged.
    let known: Vec<u8> = (0..160)
        .map(|i| if i % 2 == 0 { 0x42 } else { 0xBD })
        .collect();
    for _ in 0..10 {
        peer_a.send_pcmu(&known).await;
        tokio::time::sleep(timing::PACING).await;
    }
    tokio::time::sleep(timing::scaled_ms(400)).await;

    let received = peer_b.received_count();
    assert!(
        received > 0,
        "peer B should receive RTP forwarded over IPv6, got {received}"
    );
    assert_eq!(
        peer_b.last_payload().await,
        known,
        "payload should traverse the IPv6 path byte-for-byte (PCMU passthrough)"
    );

    let info = client.request_ok("session.info", json!({})).await;
    assert_eq!(info["endpoints"].as_array().unwrap().len(), 2);

    client.request_ok("session.destroy", json!({})).await;
}

/// End-to-end dual-stack: a server bound on both IPv4 and IPv6 answers each peer
/// in the peer's own family, then bridges media from an IPv4 peer to an IPv6
/// peer. Proves dual-bind + per-family selection + cross-family forwarding in one
/// path.
///
/// Skips cleanly on hosts without an IPv6 loopback.
#[tokio::test]
async fn test_dual_stack_cross_family_media_flow() {
    let Some(mut peer_b) = TestRtpPeer::new_v6().await else {
        eprintln!(
            "skipping test_dual_stack_cross_family_media_flow: no IPv6 loopback on this host"
        );
        return;
    };
    let mut peer_a = TestRtpPeer::new().await; // IPv4

    let server = TestServer::builder()
        .media_ip(vec![
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            IpAddr::V6(Ipv6Addr::LOCALHOST),
        ])
        .start()
        .await;
    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;

    // Endpoint A: IPv4 offer → the server must answer from its IPv4 binding.
    let offer_a = peer_a.make_sdp_offer();
    let res_a = client
        .request_ok(
            "endpoint.create_from_offer",
            json!({"sdp": offer_a, "direction": "sendrecv"}),
        )
        .await;
    let answer_a = res_a["sdp_answer"].as_str().unwrap();
    assert!(
        answer_a.contains("c=IN IP4"),
        "IPv4 offer must get an IPv4 answer on a dual-stack server; got:\n{answer_a}"
    );
    peer_a.set_remote(parse_rtp_addr_from_sdp(answer_a).expect("parse v4 server addr A"));

    // Endpoint B: IPv6 offer → the server must answer from its IPv6 binding.
    let offer_b = peer_b.make_sdp_offer();
    let res_b = client
        .request_ok(
            "endpoint.create_from_offer",
            json!({"sdp": offer_b, "direction": "sendrecv"}),
        )
        .await;
    let answer_b = res_b["sdp_answer"].as_str().unwrap();
    assert!(
        answer_b.contains("c=IN IP6"),
        "IPv6 offer must get an IPv6 answer on a dual-stack server; got:\n{answer_b}"
    );
    peer_b.set_remote(parse_rtp_addr_from_sdp(answer_b).expect("parse v6 server addr B"));

    peer_b.activate().await;
    tokio::time::sleep(timing::scaled_ms(50)).await;
    peer_b.start_recv();
    tokio::time::sleep(timing::scaled_ms(50)).await;

    // IPv4 peer A originates; IPv6 peer B must receive it — media crosses families
    // through the server (recv on the v4 socket, send on the v6 socket).
    let known: Vec<u8> = (0..160).map(|i| (i as u8) ^ 0x5A).collect();
    for _ in 0..10 {
        peer_a.send_pcmu(&known).await;
        tokio::time::sleep(timing::PACING).await;
    }
    tokio::time::sleep(timing::scaled_ms(400)).await;

    let received = peer_b.received_count();
    assert!(
        received > 0,
        "IPv6 peer B should receive media originated by IPv4 peer A (cross-family bridge), got {received}"
    );
    assert_eq!(
        peer_b.last_payload().await,
        known,
        "cross-family PCMU passthrough should be byte-for-byte"
    );

    client.request_ok("session.destroy", json!({})).await;
}
