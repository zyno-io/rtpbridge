use std::fs::File;
use std::process::Command;
use std::time::Duration;

use pcap_file::pcap::{PcapPacket, PcapWriter};
use rtpbridge::recording::meta::{StreamDescriptor, VERSION};
use rtpbridge::recording::pcap_writer::build_pcap_frame;
use tempfile::TempDir;

const ORIGIN_MS: u64 = 1_790_000_000_000;
const RATE: usize = 16_000;

struct Packet {
    endpoint: u8,
    capture_ms: u64,
    seq: u16,
    ts: u32,
    ssrc: u32,
    value: u8,
}

fn write_packet(writer: &mut PcapWriter<File>, endpoint: u8, capture_ms: u64, payload: &[u8]) {
    let src = format!("10.0.0.{}:10000", endpoint + 1).parse().unwrap();
    let dst = "10.255.0.1:10000".parse().unwrap();
    let frame = build_pcap_frame(src, dst, payload);
    let packet = PcapPacket::new(
        Duration::from_millis(capture_ms),
        frame.len() as u32,
        &frame,
    );
    writer.write_packet(&packet).unwrap();
}

/// Exercise the real CLI, PCAP parser, encoded spool, stereo renderer and timeline
/// sidecar. No descriptor is replayed at the media restart.
fn assert_restart(initial_seq: u16, resumed_seq: u16, resumed_ssrc: u32, resumed_ts: u32) {
    let tmp = TempDir::new().unwrap();
    let pcap = tmp.path().join("restart.pcap");
    let wav = tmp.path().join("restart.wav");
    let metadata = tmp.path().join("restart.timeline.json");
    let mut writer = PcapWriter::new(File::create(&pcap).unwrap()).unwrap();
    for endpoint in 0..2 {
        let descriptor = StreamDescriptor {
            v: VERSION,
            endpoint_id: format!("endpoint-{endpoint}"),
            role: "remote".into(),
            ep_type: "rtp".into(),
            codec: "PCMU".into(),
            pt: 0,
            clock_rate: 8000,
            channels: 1,
            endian: None,
            ssrc: Some(1),
            local: String::new(),
            remote: String::new(),
        };
        write_packet(&mut writer, endpoint, ORIGIN_MS - 20, &descriptor.encode());
    }
    // Delayed packets on both sides of the restart must still be reordered
    // within their own epoch. The other endpoint keeps its original RTP clock.
    let packets = [
        Packet {
            endpoint: 0,
            capture_ms: 0,
            seq: initial_seq,
            ts: 8000,
            ssrc: 1,
            value: 0x20,
        },
        Packet {
            endpoint: 0,
            capture_ms: 40,
            seq: initial_seq + 2,
            ts: 8320,
            ssrc: 1,
            value: 0xa0,
        },
        Packet {
            endpoint: 1,
            capture_ms: 40,
            seq: 100,
            ts: 10000,
            ssrc: 1,
            value: 0xa0,
        },
        Packet {
            endpoint: 0,
            capture_ms: 45,
            seq: initial_seq + 1,
            ts: 8160,
            ssrc: 1,
            value: 0x20,
        },
        Packet {
            endpoint: 0,
            capture_ms: 3000,
            seq: resumed_seq,
            ts: resumed_ts,
            ssrc: resumed_ssrc,
            value: 0x20,
        },
        Packet {
            endpoint: 1,
            capture_ms: 3000,
            seq: 101,
            ts: 33680,
            ssrc: 1,
            value: 0xa0,
        },
        Packet {
            endpoint: 0,
            capture_ms: 3040,
            seq: resumed_seq + 2,
            ts: resumed_ts.wrapping_add(320),
            ssrc: resumed_ssrc,
            value: 0xa0,
        },
        Packet {
            endpoint: 0,
            capture_ms: 3045,
            seq: resumed_seq + 1,
            ts: resumed_ts.wrapping_add(160),
            ssrc: resumed_ssrc,
            value: 0x20,
        },
    ];
    for packet in packets {
        let mut rtp = vec![0x80, 0];
        rtp.extend_from_slice(&packet.seq.to_be_bytes());
        rtp.extend_from_slice(&packet.ts.to_be_bytes());
        rtp.extend_from_slice(&packet.ssrc.to_be_bytes());
        rtp.extend_from_slice(&[packet.value; 160]);
        write_packet(
            &mut writer,
            packet.endpoint,
            ORIGIN_MS + packet.capture_ms,
            &rtp,
        );
    }
    drop(writer);

    let result = Command::new(env!("CARGO_BIN_EXE_pcap2audio"))
        .arg(&pcap)
        .args(["--mode", "stereo", "--rate", "16000", "--output"])
        .arg(&wav)
        .arg("--metadata")
        .arg(&metadata)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let bytes = std::fs::read(wav).unwrap();
    assert_eq!(&bytes[..4], b"RIFF");
    assert_eq!(u16::from_le_bytes(bytes[22..24].try_into().unwrap()), 2);
    assert_eq!(
        u32::from_le_bytes(bytes[24..28].try_into().unwrap()),
        RATE as u32
    );
    assert_eq!(&bytes[36..40], b"data");
    let samples: Vec<i16> = bytes[44..]
        .chunks_exact(2)
        .map(|sample| i16::from_le_bytes(sample.try_into().unwrap()))
        .collect();
    assert_eq!(
        samples.len(),
        3060 * RATE / 1000 * 2,
        "capture-aligned duration"
    );
    let sample_at = |ms: usize, channel: usize| samples[ms * RATE / 1000 * 2 + channel];
    for start in [0, 20, 40, 3000, 3020, 3040] {
        assert!(
            sample_at(start + 10, 0).unsigned_abs() > 1000,
            "left audio at {start} ms"
        );
    }
    // Packet 102 carries the opposite sign to 101, exposing cross-epoch or
    // arrival-order sorting mistakes, rather than only checking nonempty audio.
    assert!(sample_at(30, 0) < 0);
    assert!(sample_at(50, 0) > 0);
    assert!(sample_at(3030, 0) < 0);
    assert!(sample_at(3050, 0) > 0);
    assert!(
        samples[60 * RATE / 1000 * 2..3000 * RATE / 1000 * 2]
            .iter()
            .all(|&s| s == 0),
        "hold gap stays silent"
    );
    for ms in [0, 20, 3020, 3040] {
        assert_eq!(sample_at(ms + 10, 1), 0, "right stays silent at {ms} ms");
    }
    for ms in [40, 3000] {
        assert!(
            sample_at(ms + 10, 1).unsigned_abs() > 1000,
            "right remains aligned at {ms} ms"
        );
    }
    let metadata: serde_json::Value =
        serde_json::from_slice(&std::fs::read(metadata).unwrap()).unwrap();
    assert_eq!(metadata["audio_origin_epoch_ms"], ORIGIN_MS);
    assert_eq!(metadata["decoded_duration_ms"], 3060);
    assert_eq!(metadata["sample_rate"], RATE);
    assert_eq!(metadata["channels"], 2);
}

#[test]
fn large_forward_sequence_jump_preserves_audio_and_hold_gap() {
    assert_restart(100, 20_000, 1, 17);
}

#[test]
fn large_backward_sequence_jump_preserves_audio_and_hold_gap() {
    assert_restart(30_000, 7, 1, 3_000_000_000);
}

#[test]
fn ssrc_change_with_continuous_sequence_reanchors_rtp_timestamps() {
    // Without an SSRC boundary, this new clock would place resumed audio 1.5 s
    // late, inside the existing 2 s timestamp/capture fallback tolerance.
    assert_restart(100, 103, 2, 44_000);
}

#[test]
fn ssrc_change_with_sequence_reset_preserves_audio_and_hold_gap() {
    assert_restart(30_000, 7, 2, 17);
}
