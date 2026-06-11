mod helpers;

use serde_json::json;
use tempfile::TempDir;

use helpers::control_client::TestControlClient;
use helpers::test_rtp_peer::{TestRtpPeer, parse_rtp_addr_from_sdp};
use helpers::test_server::TestServer;
use helpers::timing;
use helpers::wav::generate_test_wav;

// PCAP frame layout for the recorder's Ethernet + IPv4 + UDP framing.
const ETH_IP_UDP: usize = 14 + 20 + 8; // 42: start of the RTP payload
const V4_SRC_IP: usize = 26; // IPv4 source address (4 bytes)
const V4_SRC_PORT: usize = 34; // UDP source port (2 bytes)

/// Bring up two plain-RTP endpoints (A engaged because it routes to plain-RTP B),
/// returning peer A, peer B, and endpoint A's id.
async fn two_rtp_endpoints(client: &mut TestControlClient) -> (TestRtpPeer, TestRtpPeer, String) {
    let mut peer_a = TestRtpPeer::new().await;
    let mut peer_b = TestRtpPeer::new().await;

    let res_a = client
        .request_ok(
            "endpoint.create_from_offer",
            json!({"sdp": peer_a.make_sdp_offer(), "direction": "sendrecv"}),
        )
        .await;
    let ep_a_id = res_a["endpoint_id"].as_str().unwrap().to_string();
    peer_a.set_remote(parse_rtp_addr_from_sdp(res_a["sdp_answer"].as_str().unwrap()).unwrap());

    let res_b = client
        .request_ok(
            "endpoint.create_from_offer",
            json!({"sdp": peer_b.make_sdp_offer(), "direction": "sendrecv"}),
        )
        .await;
    peer_b.set_remote(parse_rtp_addr_from_sdp(res_b["sdp_answer"].as_str().unwrap()).unwrap());

    (peer_a, peer_b, ep_a_id)
}

/// Read all PCAP packets' raw frames.
fn read_frames(path: &std::path::Path) -> Vec<Vec<u8>> {
    let file = std::fs::File::open(path).expect("PCAP file should exist");
    let mut reader = pcap_file::pcap::PcapReader::new(file).expect("valid PCAP file");
    let mut frames = Vec::new();
    while let Some(pkt) = reader.next_packet() {
        frames.push(pkt.expect("valid PCAP packet").data.to_vec());
    }
    frames
}

/// The PCAP should carry the peer's REAL source IP:port, not a synthetic 10.x
/// marker.
#[tokio::test]
async fn test_recording_uses_real_remote_address() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;

    let (mut peer_a, _peer_b, ep_a_id) = two_rtp_endpoints(&mut client).await;
    let peer_a_port = peer_a.local_addr.port();

    // Record endpoint A's inbound only.
    let pcap_path = std::path::Path::new(&server.recording_dir).join("real-addr.pcap");
    let rec = client
        .request_ok(
            "recording.start",
            json!({"endpoint_id": ep_a_id, "file_path": pcap_path.to_str().unwrap()}),
        )
        .await;
    let rec_id = rec["recording_id"].as_str().unwrap().to_string();

    for _ in 0..10 {
        peer_a.send_pcmu(&[0x80u8; 160]).await;
        tokio::time::sleep(timing::PACING).await;
    }
    tokio::time::sleep(timing::scaled_ms(400)).await;
    client
        .request_ok("recording.stop", json!({"recording_id": rec_id}))
        .await;
    tokio::time::sleep(timing::scaled_ms(300)).await;

    let frames = read_frames(&pcap_path);
    assert!(!frames.is_empty(), "should have recorded packets");

    // Every inbound frame's source must be peer A's real loopback IP:port.
    let mut checked = 0;
    for f in &frames {
        if f.len() < ETH_IP_UDP {
            continue;
        }
        assert_eq!(&f[12..14], &[0x08, 0x00], "IPv4 frame expected");
        assert_eq!(
            &f[V4_SRC_IP..V4_SRC_IP + 4],
            &[127, 0, 0, 1],
            "source IP should be the real peer loopback, not a synthetic 10.x"
        );
        assert_eq!(
            u16::from_be_bytes([f[V4_SRC_PORT], f[V4_SRC_PORT + 1]]),
            peer_a_port,
            "source port should be peer A's real RTP port"
        );
        checked += 1;
    }
    assert!(
        checked > 0,
        "should have validated at least one inbound frame"
    );

    client.request_ok("session.destroy", json!({})).await;
}

/// Out-of-order arrivals must be recorded in ARRIVAL order, not sorted — the tap
/// is upstream of the playout/jitter buffer. Under the old post-buffer tap the
/// Tracked buffer would have re-sorted these by sequence number.
#[tokio::test]
async fn test_recording_preserves_arrival_order() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;

    // Endpoint A is engaged (routes to plain-RTP B), so its inbound runs through
    // the jitter buffer for routing — but recording must still see raw arrival.
    let (mut peer_a, _peer_b, ep_a_id) = two_rtp_endpoints(&mut client).await;

    let pcap_path = std::path::Path::new(&server.recording_dir).join("arrival-order.pcap");
    let rec = client
        .request_ok(
            "recording.start",
            json!({"endpoint_id": ep_a_id, "file_path": pcap_path.to_str().unwrap()}),
        )
        .await;
    let rec_id = rec["recording_id"].as_str().unwrap().to_string();

    // Deliberately jittered / reordered sequence numbers.
    let send_order: [u16; 6] = [100, 98, 99, 103, 101, 102];
    for &seq in &send_order {
        peer_a
            .send_pcmu_with_seq(seq, seq as u32 * 160, &[0x80u8; 160])
            .await;
        tokio::time::sleep(timing::scaled_ms(25)).await;
    }
    tokio::time::sleep(timing::scaled_ms(400)).await;
    client
        .request_ok("recording.stop", json!({"recording_id": rec_id}))
        .await;
    tokio::time::sleep(timing::scaled_ms(300)).await;

    // Extract the RTP sequence number (RTP header bytes 2..4) from each recorded
    // PCMU packet, in record order.
    let recorded_seqs: Vec<u16> = read_frames(&pcap_path)
        .iter()
        .filter(|f| f.len() >= ETH_IP_UDP + 12)
        .filter(|f| f[ETH_IP_UDP] & 0xC0 == 0x80 && f[ETH_IP_UDP + 1] & 0x7F == 0) // RTP v2, PCMU
        .map(|f| u16::from_be_bytes([f[ETH_IP_UDP + 2], f[ETH_IP_UDP + 3]]))
        .collect();

    assert_eq!(
        recorded_seqs, send_order,
        "recorded order must match arrival order (not sorted by sequence)"
    );

    client.request_ok("session.destroy", json!({})).await;
}

/// Locally-generated file/tone audio (a source with no real socket) must still be
/// captured by an endpoint recording — the pre-buffer tap relocation must not drop
/// generator coverage that the old downstream tap had.
#[tokio::test]
async fn test_recording_captures_file_source() {
    let tmp = TempDir::new().unwrap();
    let tmp_str = tmp.path().to_str().unwrap();
    let server = TestServer::builder()
        .media_dir(tmp_str)
        .recording_dir(tmp_str)
        .start()
        .await;
    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;

    // A connected RTP peer gives the file a routing destination.
    let mut peer = TestRtpPeer::new().await;
    let res = client
        .request_ok(
            "endpoint.create_offer",
            json!({"type": "rtp", "direction": "sendrecv"}),
        )
        .await;
    let ep_id = res["endpoint_id"].as_str().unwrap().to_string();
    peer.set_remote(parse_rtp_addr_from_sdp(res["sdp_offer"].as_str().unwrap()).unwrap());
    client
        .request_ok(
            "endpoint.accept_answer",
            json!({"endpoint_id": ep_id, "sdp": peer.make_sdp_answer()}),
        )
        .await;

    // Create the file playback endpoint.
    let wav = tmp.path().join("rec-file-source.wav");
    generate_test_wav(&wav, 1.0, 440.0);
    let res = client
        .request_ok(
            "endpoint.create_with_file",
            json!({"source": wav.to_str().unwrap(), "shared": false, "loop_count": 0}),
        )
        .await;
    let file_id = res["endpoint_id"].as_str().unwrap().to_string();

    // Record the file endpoint's generated audio (inbound for that source).
    let pcap = std::path::Path::new(&server.recording_dir).join("file-source.pcap");
    let rec = client
        .request_ok(
            "recording.start",
            json!({"endpoint_id": file_id, "file_path": pcap.to_str().unwrap()}),
        )
        .await;
    let rec_id = rec["recording_id"].as_str().unwrap().to_string();

    tokio::time::sleep(timing::scaled_ms(800)).await;
    let stop = client
        .request_ok("recording.stop", json!({"recording_id": rec_id}))
        .await;
    assert!(
        stop["packets"].as_u64().unwrap() > 0,
        "file-source generated audio must be recorded (got {})",
        stop["packets"]
    );

    client.request_ok("session.destroy", json!({})).await;
}
