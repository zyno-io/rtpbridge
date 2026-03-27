mod helpers;

use std::time::Duration;

use serde_json::json;

use helpers::control_client::TestControlClient;
use helpers::test_rtp_peer::{TestRtpPeer, parse_rtp_addr_from_sdp};
use helpers::test_server::TestServer;
use helpers::timing;

/// Helper: create a PCMU endpoint from a peer's SDP offer.
/// Returns the endpoint ID.
async fn create_pcmu_endpoint(client: &mut TestControlClient, peer: &mut TestRtpPeer) -> String {
    let offer = peer.make_sdp_offer();
    let result = client
        .request_ok(
            "endpoint.create_from_offer",
            json!({"sdp": offer, "direction": "sendrecv"}),
        )
        .await;
    let ep_id = result["endpoint_id"].as_str().unwrap().to_string();
    let answer = result["sdp_answer"].as_str().unwrap();
    let server_addr = parse_rtp_addr_from_sdp(answer).expect("parse server addr");
    peer.set_remote(server_addr);
    ep_id
}

/// Helper: create a G.722 endpoint from a peer's SDP offer.
async fn create_g722_endpoint(client: &mut TestControlClient, peer: &mut TestRtpPeer) -> String {
    let offer = peer.make_g722_sdp_offer();
    let result = client
        .request_ok(
            "endpoint.create_from_offer",
            json!({"sdp": offer, "direction": "sendrecv"}),
        )
        .await;
    let ep_id = result["endpoint_id"].as_str().unwrap().to_string();
    let answer = result["sdp_answer"].as_str().unwrap();
    let server_addr = parse_rtp_addr_from_sdp(answer).expect("parse server addr");
    peer.set_remote(server_addr);
    ep_id
}

/// Verify that received timestamps are non-decreasing (monotonic).
fn timestamps_monotonic(timestamps: &[u32]) -> bool {
    if timestamps.len() < 2 {
        return true;
    }
    for i in 1..timestamps.len() {
        // A "backwards" jump would be a very large positive diff (wrapping).
        let diff = timestamps[i].wrapping_sub(timestamps[i - 1]);
        if diff > 0x7FFF_FFFF {
            return false;
        }
    }
    true
}

/// Test: Three PCMU endpoints can exchange audio with mixing.
/// Each endpoint should receive mixed audio from the other two,
/// with monotonically increasing timestamps.
#[tokio::test]
async fn test_three_party_pcmu_mixing() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;

    let mut peer_a = TestRtpPeer::new().await;
    let mut peer_b = TestRtpPeer::new().await;
    let mut peer_c = TestRtpPeer::new().await;

    create_pcmu_endpoint(&mut client, &mut peer_a).await;
    create_pcmu_endpoint(&mut client, &mut peer_b).await;
    create_pcmu_endpoint(&mut client, &mut peer_c).await;

    // Activate symmetric RTP on all endpoints
    peer_a.activate().await;
    peer_b.activate().await;
    peer_c.activate().await;
    tokio::time::sleep(timing::scaled_ms(100)).await;

    // Start receiving on all peers
    peer_a.start_recv();
    peer_b.start_recv();
    peer_c.start_recv();
    tokio::time::sleep(timing::scaled_ms(50)).await;

    // A and B send simultaneously
    let send_a = {
        let mut pa = TestRtpPeer::new().await;
        std::mem::swap(&mut pa, &mut peer_a);
        tokio::spawn(async move {
            pa.send_tone_for(Duration::from_millis(400)).await;
            pa
        })
    };
    let send_b = {
        let mut pb = TestRtpPeer::new().await;
        std::mem::swap(&mut pb, &mut peer_b);
        tokio::spawn(async move {
            pb.send_tone_for(Duration::from_millis(400)).await;
            pb
        })
    };

    let (pa, pb) = tokio::join!(send_a, send_b);
    peer_a = pa.unwrap();
    peer_b = pb.unwrap();

    tokio::time::sleep(timing::scaled_ms(500)).await;

    // C should receive mixed audio from A and B
    let c_count = peer_c.received_count();
    assert!(
        c_count > 0,
        "peer C should receive mixed packets, got {c_count}"
    );

    // Verify C's timestamps are monotonic
    let c_timestamps = peer_c.received_timestamps().await;
    assert!(
        timestamps_monotonic(&c_timestamps),
        "peer C timestamps should be monotonic: {:?}",
        &c_timestamps[..c_timestamps.len().min(20)]
    );

    // A should also receive (mixed from B + C's silence)
    let a_count = peer_a.received_count();
    assert!(
        a_count > 0,
        "peer A should receive mixed packets, got {a_count}"
    );

    let a_timestamps = peer_a.received_timestamps().await;
    assert!(
        timestamps_monotonic(&a_timestamps),
        "peer A timestamps should be monotonic"
    );
}

/// Test: Three G.722 endpoints exchange audio with mixing.
#[tokio::test]
async fn test_three_party_g722_mixing() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;

    let mut peer_a = TestRtpPeer::new().await;
    let mut peer_b = TestRtpPeer::new().await;
    let mut peer_c = TestRtpPeer::new().await;

    create_g722_endpoint(&mut client, &mut peer_a).await;
    create_g722_endpoint(&mut client, &mut peer_b).await;
    create_g722_endpoint(&mut client, &mut peer_c).await;

    peer_a.activate().await;
    peer_b.activate().await;
    peer_c.activate().await;
    tokio::time::sleep(timing::scaled_ms(100)).await;

    peer_c.start_recv();
    tokio::time::sleep(timing::scaled_ms(50)).await;

    // A and B send G.722 audio
    let send_a = {
        let mut pa = TestRtpPeer::new().await;
        std::mem::swap(&mut pa, &mut peer_a);
        tokio::spawn(async move {
            pa.send_g722_tone_for(Duration::from_millis(400)).await;
            pa
        })
    };
    let send_b = {
        let mut pb = TestRtpPeer::new().await;
        std::mem::swap(&mut pb, &mut peer_b);
        tokio::spawn(async move {
            pb.send_g722_tone_for(Duration::from_millis(400)).await;
            pb
        })
    };

    let (pa, pb) = tokio::join!(send_a, send_b);
    let _ = (pa.unwrap(), pb.unwrap());

    tokio::time::sleep(timing::scaled_ms(500)).await;

    let c_count = peer_c.received_count();
    assert!(
        c_count > 0,
        "peer C should receive mixed G.722 packets, got {c_count}"
    );

    // Verify G.722 PT in received packets
    let raw = peer_c.last_received_raw().await;
    if raw.len() >= 2 {
        let pt = raw[1] & 0x7F;
        assert_eq!(pt, 9, "expected G.722 PT=9, got {pt}");
    }

    let c_timestamps = peer_c.received_timestamps().await;
    assert!(
        timestamps_monotonic(&c_timestamps),
        "peer C G.722 timestamps should be monotonic"
    );
}

/// Test: Cross-codec 3-way (PCMU + G.722 endpoints).
#[tokio::test]
async fn test_three_party_cross_codec_mixing() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;

    let mut peer_a = TestRtpPeer::new().await; // PCMU
    let mut peer_b = TestRtpPeer::new().await; // G.722
    let mut peer_c = TestRtpPeer::new().await; // PCMU

    create_pcmu_endpoint(&mut client, &mut peer_a).await;
    create_g722_endpoint(&mut client, &mut peer_b).await;
    create_pcmu_endpoint(&mut client, &mut peer_c).await;

    peer_a.activate().await;
    peer_b.activate().await;
    peer_c.activate().await;
    tokio::time::sleep(timing::scaled_ms(100)).await;

    peer_b.start_recv();
    peer_c.start_recv();
    tokio::time::sleep(timing::scaled_ms(50)).await;

    // A sends PCMU
    peer_a.send_tone_for(Duration::from_millis(400)).await;
    tokio::time::sleep(timing::scaled_ms(500)).await;

    // B should receive G.722 (its native codec)
    let b_count = peer_b.received_count();
    assert!(
        b_count > 0,
        "peer B should receive cross-coded packets, got {b_count}"
    );
    let raw_b = peer_b.last_received_raw().await;
    if raw_b.len() >= 2 {
        let pt = raw_b[1] & 0x7F;
        assert_eq!(pt, 9, "B should receive G.722 PT=9, got {pt}");
    }

    // C should receive PCMU
    let c_count = peer_c.received_count();
    assert!(
        c_count > 0,
        "peer C should receive mixed PCMU, got {c_count}"
    );
    let raw_c = peer_c.last_received_raw().await;
    if raw_c.len() >= 2 {
        let pt = raw_c[1] & 0x7F;
        assert_eq!(pt, 0, "C should receive PCMU PT=0, got {pt}");
    }

    // Both should have monotonic timestamps
    let b_timestamps = peer_b.received_timestamps().await;
    assert!(
        timestamps_monotonic(&b_timestamps),
        "B timestamps should be monotonic"
    );
    let c_timestamps = peer_c.received_timestamps().await;
    assert!(
        timestamps_monotonic(&c_timestamps),
        "C timestamps should be monotonic"
    );
}

/// Test: Adding a 3rd endpoint mid-call activates the mixer.
#[tokio::test]
async fn test_add_third_party_mid_call() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;

    let mut peer_a = TestRtpPeer::new().await;
    let mut peer_b = TestRtpPeer::new().await;

    create_pcmu_endpoint(&mut client, &mut peer_a).await;
    create_pcmu_endpoint(&mut client, &mut peer_b).await;

    peer_a.activate().await;
    peer_b.activate().await;
    tokio::time::sleep(timing::scaled_ms(100)).await;

    // Start 2-party call
    peer_b.start_recv();
    tokio::time::sleep(timing::scaled_ms(50)).await;
    peer_a.send_tone_for(Duration::from_millis(200)).await;
    tokio::time::sleep(timing::scaled_ms(300)).await;

    let b_before = peer_b.received_count();
    assert!(
        b_before > 0,
        "B should receive from A in 2-party call, got {b_before}"
    );

    // Add 3rd endpoint
    let mut peer_c = TestRtpPeer::new().await;
    create_pcmu_endpoint(&mut client, &mut peer_c).await;
    peer_c.activate().await;
    tokio::time::sleep(timing::scaled_ms(100)).await;

    peer_c.start_recv();
    tokio::time::sleep(timing::scaled_ms(50)).await;

    // A sends again — now mixer should be active
    peer_a.send_tone_for(Duration::from_millis(400)).await;
    tokio::time::sleep(timing::scaled_ms(500)).await;

    // C should receive mixed audio
    let c_count = peer_c.received_count();
    assert!(
        c_count > 0,
        "peer C should receive after joining, got {c_count}"
    );

    // B should continue receiving
    let b_after = peer_b.received_count();
    assert!(
        b_after > b_before,
        "B should continue receiving after 3rd joins"
    );

    // C's timestamps should be monotonic
    let c_timestamps = peer_c.received_timestamps().await;
    assert!(
        timestamps_monotonic(&c_timestamps),
        "C timestamps should be monotonic after joining"
    );
}

/// Test: Removing an endpoint from a 3-way call deactivates the mixer.
#[tokio::test]
async fn test_remove_third_party_deactivates_mixer() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;

    let mut peer_a = TestRtpPeer::new().await;
    let mut peer_b = TestRtpPeer::new().await;
    let mut peer_c = TestRtpPeer::new().await;

    create_pcmu_endpoint(&mut client, &mut peer_a).await;
    create_pcmu_endpoint(&mut client, &mut peer_b).await;
    let ep_c_id = create_pcmu_endpoint(&mut client, &mut peer_c).await;

    peer_a.activate().await;
    peer_b.activate().await;
    peer_c.activate().await;
    tokio::time::sleep(timing::scaled_ms(100)).await;

    peer_b.start_recv();
    tokio::time::sleep(timing::scaled_ms(50)).await;

    // 3-party audio
    peer_a.send_tone_for(Duration::from_millis(200)).await;
    tokio::time::sleep(timing::scaled_ms(300)).await;

    let b_mid = peer_b.received_count();
    assert!(b_mid > 0, "B should receive during 3-party call");

    // Remove C
    client
        .request_ok("endpoint.remove", json!({"endpoint_id": ep_c_id}))
        .await;
    tokio::time::sleep(timing::scaled_ms(100)).await;

    let b_before = peer_b.received_count();

    // A sends again — should be passthrough now (2-party)
    peer_a.send_tone_for(Duration::from_millis(400)).await;
    tokio::time::sleep(timing::scaled_ms(500)).await;

    let b_after = peer_b.received_count();
    assert!(
        b_after > b_before,
        "B should receive from A after C removed (passthrough), got {b_before} -> {b_after}"
    );
}

/// Test: Four-party call.
#[tokio::test]
async fn test_four_party_mixing() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;

    let mut peer_a = TestRtpPeer::new().await;
    let mut peer_b = TestRtpPeer::new().await;
    let mut peer_c = TestRtpPeer::new().await;
    let mut peer_d = TestRtpPeer::new().await;

    create_pcmu_endpoint(&mut client, &mut peer_a).await;
    create_pcmu_endpoint(&mut client, &mut peer_b).await;
    create_pcmu_endpoint(&mut client, &mut peer_c).await;
    create_pcmu_endpoint(&mut client, &mut peer_d).await;

    peer_a.activate().await;
    peer_b.activate().await;
    peer_c.activate().await;
    peer_d.activate().await;
    tokio::time::sleep(timing::scaled_ms(100)).await;

    peer_a.start_recv();
    peer_b.start_recv();
    peer_c.start_recv();
    peer_d.start_recv();
    tokio::time::sleep(timing::scaled_ms(50)).await;

    // A and B send
    let send_a = {
        let mut pa = TestRtpPeer::new().await;
        std::mem::swap(&mut pa, &mut peer_a);
        tokio::spawn(async move {
            pa.send_tone_for(Duration::from_millis(400)).await;
            pa
        })
    };
    let send_b = {
        let mut pb = TestRtpPeer::new().await;
        std::mem::swap(&mut pb, &mut peer_b);
        tokio::spawn(async move {
            pb.send_tone_for(Duration::from_millis(400)).await;
            pb
        })
    };

    let (pa, pb) = tokio::join!(send_a, send_b);
    peer_a = pa.unwrap();
    peer_b = pb.unwrap();

    tokio::time::sleep(timing::scaled_ms(500)).await;

    for (name, peer) in [("C", &peer_c), ("D", &peer_d)] {
        let count = peer.received_count();
        assert!(
            count > 0,
            "peer {name} should receive in 4-party call, got {count}"
        );
        let timestamps = peer.received_timestamps().await;
        assert!(
            timestamps_monotonic(&timestamps),
            "peer {name} timestamps should be monotonic in 4-party call"
        );
    }

    let a_count = peer_a.received_count();
    let b_count = peer_b.received_count();
    assert!(
        a_count > 0,
        "peer A should receive in 4-party call, got {a_count}"
    );
    assert!(
        b_count > 0,
        "peer B should receive in 4-party call, got {b_count}"
    );
}

/// Test: Only one sender in a 3-way call.
#[tokio::test]
async fn test_single_sender_in_three_way() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;

    let mut peer_a = TestRtpPeer::new().await;
    let mut peer_b = TestRtpPeer::new().await;
    let mut peer_c = TestRtpPeer::new().await;

    create_pcmu_endpoint(&mut client, &mut peer_a).await;
    create_pcmu_endpoint(&mut client, &mut peer_b).await;
    create_pcmu_endpoint(&mut client, &mut peer_c).await;

    peer_a.activate().await;
    peer_b.activate().await;
    peer_c.activate().await;
    tokio::time::sleep(timing::scaled_ms(100)).await;

    peer_b.start_recv();
    peer_c.start_recv();
    tokio::time::sleep(timing::scaled_ms(50)).await;

    // Only A sends
    peer_a.send_tone_for(Duration::from_millis(400)).await;
    tokio::time::sleep(timing::scaled_ms(500)).await;

    let b_count = peer_b.received_count();
    let c_count = peer_c.received_count();
    assert!(
        b_count > 0,
        "B should receive from single sender, got {b_count}"
    );
    assert!(
        c_count > 0,
        "C should receive from single sender, got {c_count}"
    );

    let b_ts = peer_b.received_timestamps().await;
    let c_ts = peer_c.received_timestamps().await;
    assert!(
        timestamps_monotonic(&b_ts),
        "B timestamps should be monotonic"
    );
    assert!(
        timestamps_monotonic(&c_ts),
        "C timestamps should be monotonic"
    );
}

/// Test: Verify the mixer produces consistent timestamp increments.
#[tokio::test]
async fn test_mixer_timestamp_increments() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;

    let mut peer_a = TestRtpPeer::new().await;
    let mut peer_b = TestRtpPeer::new().await;
    let mut peer_c = TestRtpPeer::new().await;

    create_pcmu_endpoint(&mut client, &mut peer_a).await;
    create_pcmu_endpoint(&mut client, &mut peer_b).await;
    create_pcmu_endpoint(&mut client, &mut peer_c).await;

    peer_a.activate().await;
    peer_b.activate().await;
    peer_c.activate().await;
    tokio::time::sleep(timing::scaled_ms(100)).await;

    peer_c.start_recv();
    tokio::time::sleep(timing::scaled_ms(50)).await;

    // A sends a steady stream
    peer_a.send_tone_for(Duration::from_millis(600)).await;
    tokio::time::sleep(timing::scaled_ms(500)).await;

    let timestamps = peer_c.received_timestamps().await;
    assert!(
        timestamps.len() >= 5,
        "should have enough packets for analysis, got {}",
        timestamps.len()
    );

    // Check that most consecutive timestamps differ by 160 (PCMU 20ms at 8kHz)
    let mut correct_increments = 0;
    let total = timestamps.len() - 1;
    for i in 1..timestamps.len() {
        let diff = timestamps[i].wrapping_sub(timestamps[i - 1]);
        if diff == 160 {
            correct_increments += 1;
        }
    }
    let pct = correct_increments as f64 / total as f64;
    assert!(
        pct > 0.8,
        "at least 80% of timestamp increments should be 160, got {:.0}% ({correct_increments}/{total})",
        pct * 100.0
    );
}
