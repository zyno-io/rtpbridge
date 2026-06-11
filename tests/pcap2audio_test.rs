mod helpers;

use serde_json::json;
use std::process::Command;
use tempfile::TempDir;

use helpers::control_client::TestControlClient;
use helpers::test_rtp_peer::{TestRtpPeer, parse_rtp_addr_from_sdp};
use helpers::test_server::TestServer;
use helpers::timing;
use helpers::wav::generate_test_wav;

/// Parse a little-endian 16-bit PCM WAV: returns (channels, sample_rate, samples).
fn read_wav(path: &std::path::Path) -> (u16, u32, Vec<i16>) {
    let bytes = std::fs::read(path).expect("wav exists");
    assert_eq!(&bytes[0..4], b"RIFF", "RIFF header");
    assert_eq!(&bytes[8..12], b"WAVE", "WAVE header");
    let channels = u16::from_le_bytes([bytes[22], bytes[23]]);
    let rate = u32::from_le_bytes([bytes[24], bytes[25], bytes[26], bytes[27]]);
    // Find the "data" chunk.
    let mut i = 12;
    let mut samples = Vec::new();
    while i + 8 <= bytes.len() {
        let id = &bytes[i..i + 4];
        let len =
            u32::from_le_bytes([bytes[i + 4], bytes[i + 5], bytes[i + 6], bytes[i + 7]]) as usize;
        let body = &bytes[i + 8..(i + 8 + len).min(bytes.len())];
        if id == b"data" {
            for c in body.chunks_exact(2) {
                samples.push(i16::from_le_bytes([c[0], c[1]]));
            }
            break;
        }
        i += 8 + len;
    }
    (channels, rate, samples)
}

/// Record a file-playback tone, decode the PCAP with the `pcap2audio` binary, and
/// verify the WAV is valid 48 kHz audio carrying the tone (not silence).
#[tokio::test]
async fn test_pcap2audio_decodes_file_recording_to_wav() {
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

    let wav_in = tmp.path().join("tone.wav");
    generate_test_wav(&wav_in, 1.0, 440.0);
    let res = client
        .request_ok(
            "endpoint.create_with_file",
            json!({"source": wav_in.to_str().unwrap(), "shared": false, "loop_count": 0}),
        )
        .await;
    let file_id = res["endpoint_id"].as_str().unwrap().to_string();

    let pcap = std::path::Path::new(&server.recording_dir).join("decode.pcap");
    let rec = client
        .request_ok(
            "recording.start",
            json!({"endpoint_id": file_id, "file_path": pcap.to_str().unwrap()}),
        )
        .await;
    let rec_id = rec["recording_id"].as_str().unwrap().to_string();

    tokio::time::sleep(timing::scaled_ms(600)).await;
    client
        .request_ok("recording.stop", json!({"recording_id": rec_id}))
        .await;
    tokio::time::sleep(timing::scaled_ms(300)).await;

    // Decode with the pcap2audio binary (multichannel → 1 channel, one endpoint).
    let wav_out = tmp.path().join("decoded.wav");
    let status = Command::new(env!("CARGO_BIN_EXE_pcap2audio"))
        .arg(&pcap)
        .arg("-o")
        .arg(&wav_out)
        .arg("--mode")
        .arg("multichannel")
        .output()
        .expect("run pcap2audio");
    assert!(
        status.status.success(),
        "pcap2audio failed: {}",
        String::from_utf8_lossy(&status.stderr)
    );

    let (channels, rate, samples) = read_wav(&wav_out);
    assert_eq!(channels, 1, "one endpoint -> one channel");
    assert_eq!(rate, 48000, "default output rate");
    assert!(!samples.is_empty(), "decoded audio should not be empty");
    let peak = samples.iter().map(|s| s.unsigned_abs()).max().unwrap_or(0);
    assert!(
        peak > 1000,
        "decoded WAV should carry the 440 Hz tone, not silence (peak {peak})"
    );

    client.request_ok("session.destroy", json!({})).await;
}

/// Stereo downmix: two RTP peers each sending audio produce a 2-channel WAV with
/// the first endpoint on the left and the second summed into the right.
#[tokio::test]
async fn test_pcap2audio_stereo_downmix() {
    let server = TestServer::start().await;
    let mut client = TestControlClient::connect(&server.addr).await;
    client.request_ok("session.create", json!({})).await;

    let mut peer_a = TestRtpPeer::new().await;
    let mut peer_b = TestRtpPeer::new().await;
    let ra = client
        .request_ok(
            "endpoint.create_from_offer",
            json!({"sdp": peer_a.make_sdp_offer(), "direction": "sendrecv"}),
        )
        .await;
    peer_a.set_remote(parse_rtp_addr_from_sdp(ra["sdp_answer"].as_str().unwrap()).unwrap());
    let rb = client
        .request_ok(
            "endpoint.create_from_offer",
            json!({"sdp": peer_b.make_sdp_offer(), "direction": "sendrecv"}),
        )
        .await;
    peer_b.set_remote(parse_rtp_addr_from_sdp(rb["sdp_answer"].as_str().unwrap()).unwrap());

    let pcap = std::path::Path::new(&server.recording_dir).join("stereo.pcap");
    let rec = client
        .request_ok(
            "recording.start",
            json!({"endpoint_id": null, "file_path": pcap.to_str().unwrap()}),
        )
        .await;
    let rec_id = rec["recording_id"].as_str().unwrap().to_string();

    // Both peers send non-trivial PCMU (0x20 µ-law decodes to a large magnitude).
    for _ in 0..15 {
        peer_a.send_pcmu(&[0x20u8; 160]).await;
        peer_b.send_pcmu(&[0x20u8; 160]).await;
        tokio::time::sleep(timing::PACING).await;
    }
    tokio::time::sleep(timing::scaled_ms(400)).await;
    client
        .request_ok("recording.stop", json!({"recording_id": rec_id}))
        .await;
    tokio::time::sleep(timing::scaled_ms(300)).await;

    let wav_out = std::path::Path::new(&server.recording_dir).join("stereo.wav");
    let out = Command::new(env!("CARGO_BIN_EXE_pcap2audio"))
        .arg(&pcap)
        .arg("-o")
        .arg(&wav_out)
        .arg("--mode")
        .arg("stereo")
        .output()
        .expect("run pcap2audio");
    assert!(
        out.status.success(),
        "pcap2audio failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let (channels, rate, samples) = read_wav(&wav_out);
    assert_eq!(channels, 2, "stereo");
    assert_eq!(rate, 48000);
    // Both left and right should carry audio.
    let left_peak = samples
        .iter()
        .step_by(2)
        .map(|s| s.unsigned_abs())
        .max()
        .unwrap_or(0);
    let right_peak = samples
        .iter()
        .skip(1)
        .step_by(2)
        .map(|s| s.unsigned_abs())
        .max()
        .unwrap_or(0);
    assert!(left_peak > 500, "left (first endpoint) should have audio");
    assert!(
        right_peak > 500,
        "right (other endpoints) should have audio"
    );

    client.request_ok("session.destroy", json!({})).await;
}
