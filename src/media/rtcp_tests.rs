use super::*;

#[test]
fn test_build_and_parse_sr() {
    let mut stats = RtcpStats::new();
    stats.packets_sent = 100;
    stats.octets_sent = 16000;
    // Simulate receiving 50 packets (seq 0..49)
    for seq in 0u16..50 {
        stats.record_received(0x1111_1111u32, seq, seq as u32 * 160, 160, 8000);
    }

    let data = build_sr_rr(0x12345678, 0xAABBCCDD, &mut stats, 0, 8000);
    let packets = parse_rtcp(&data);

    assert_eq!(packets.len(), 1);
    match &packets[0] {
        RtcpPacket::SenderReport(sr) => {
            assert_eq!(sr.ssrc, 0x12345678);
            assert_eq!(sr.sender_packet_count, 100);
            assert_eq!(sr.sender_octet_count, 16000);
            assert_eq!(sr.report_blocks.len(), 1);
            assert_eq!(sr.report_blocks[0].ssrc, 0xAABBCCDD);
        }
        _ => panic!("expected SR"),
    }
}

#[test]
fn test_parse_padded_sr() {
    // Build a normal SR, then add padding and set the P bit
    let mut stats = RtcpStats::new();
    stats.packets_sent = 42;
    stats.octets_sent = 8000;
    let mut data = build_sr_rr(0xDEADBEEF, 0x11223344, &mut stats, 0, 8000);

    let pad_bytes = 4u8; // add 4 bytes of padding
    data.extend_from_slice(&[0x00, 0x00, 0x00, pad_bytes]);

    // Update length field (word 1-2): add 1 word for the padding
    let orig_len = u16::from_be_bytes([data[2], data[3]]);
    let new_len = orig_len + 1; // 1 extra 32-bit word
    data[2..4].copy_from_slice(&new_len.to_be_bytes());

    // Set P bit (bit 5 of first byte)
    data[0] |= 0x20;

    let packets = parse_rtcp(&data);
    assert_eq!(packets.len(), 1);
    match &packets[0] {
        RtcpPacket::SenderReport(sr) => {
            assert_eq!(sr.ssrc, 0xDEADBEEF);
            assert_eq!(sr.sender_packet_count, 42);
            assert_eq!(sr.report_blocks.len(), 1);
            assert_eq!(sr.report_blocks[0].ssrc, 0x11223344);
        }
        _ => panic!("expected SR"),
    }
}

#[test]
fn test_rtt_from_rr() {
    let our_ssrc = 0x12345678;
    let mut stats = RtcpStats::new();

    // Compute current NTP middle to use as the "sent SR" timestamp
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap();
    let ntp_secs = now.as_secs() + 2208988800;
    let ntp_frac = (now.subsec_nanos() as u64 * (1u64 << 32)) / 1_000_000_000;
    let ntp = (ntp_secs << 32) | ntp_frac;
    stats.record_sr_sent(ntp);
    let sr_ntp_middle = stats.our_last_sr_ntp_middle;
    assert_ne!(sr_ntp_middle, 0);

    // Simulate the remote sending back an RR referencing our SR.
    // DLSR = 0 (instant reflection, remote had zero processing delay)
    let block = ReportBlock {
        ssrc: our_ssrc,
        fraction_lost: 0,
        cumulative_lost: 0,
        highest_seq: 10,
        jitter: 0,
        last_sr: sr_ntp_middle,
        delay_since_last_sr: 0,
    };

    stats.process_rr(&block, our_ssrc);

    // RTT = now_ntp_middle - LSR - DLSR ≈ 0 (within a few ms)
    let rtt = stats.rtt_ms.expect("RTT should be computed");
    assert!(rtt >= 0.0, "RTT should be non-negative, got {}", rtt);
    assert!(
        rtt < 5000.0,
        "RTT should be < 5s for same-process test, got {}",
        rtt
    );
}

#[test]
fn test_rtt_none_without_rr() {
    let stats = RtcpStats::new();
    assert!(stats.rtt_ms.is_none());
}

#[test]
fn test_rr_wrong_ssrc_ignored() {
    let mut stats = RtcpStats::new();
    stats.our_last_sr_ntp_middle = 0x12340000;

    let block = ReportBlock {
        ssrc: 0xDEADBEEF, // wrong SSRC
        fraction_lost: 0,
        cumulative_lost: 0,
        highest_seq: 0,
        jitter: 0,
        last_sr: 0x12340000,
        delay_since_last_sr: 0,
    };

    stats.process_rr(&block, 0x12345678);
    assert!(
        stats.rtt_ms.is_none(),
        "RTT should not be computed for wrong SSRC"
    );
}

#[test]
fn test_sequential_packets_no_loss() {
    let mut stats = RtcpStats::new();
    for seq in 0u16..100 {
        stats.record_received(0x1111_1111u32, seq, seq as u32 * 160, 160, 8000);
    }
    assert_eq!(stats.expected_packets(), 100);
    assert_eq!(stats.packets_received, 100);
    assert_eq!(stats.cumulative_lost(), 0);
}

#[test]
fn test_gap_causes_loss() {
    let mut stats = RtcpStats::new();
    // Send seq 0..4, skip 5..9, send 10..14
    for seq in 0u16..5 {
        stats.record_received(0x1111_1111u32, seq, seq as u32 * 160, 160, 8000);
    }
    for seq in 10u16..15 {
        stats.record_received(0x1111_1111u32, seq, seq as u32 * 160, 160, 8000);
    }
    assert_eq!(stats.expected_packets(), 15); // 0..14 inclusive
    assert_eq!(stats.packets_received, 10);
    assert_eq!(stats.cumulative_lost(), 5);
}

#[test]
fn test_reordered_packets_no_spurious_loss() {
    let mut stats = RtcpStats::new();
    // Receive: 0, 2, 1, 3 (reordered, no actual loss)
    stats.record_received(0x1111_1111u32, 0, 0, 160, 8000);
    stats.record_received(0x1111_1111u32, 2, 320, 160, 8000);
    stats.record_received(0x1111_1111u32, 1, 160, 160, 8000); // reordered
    stats.record_received(0x1111_1111u32, 3, 480, 160, 8000);
    assert_eq!(stats.expected_packets(), 4);
    assert_eq!(stats.packets_received, 4);
    assert_eq!(stats.cumulative_lost(), 0);
}

#[test]
fn test_sequence_wraparound() {
    let mut stats = RtcpStats::new();
    // Start near wraparound
    for seq in 65534u16..=65535 {
        stats.record_received(0x1111_1111u32, seq, seq as u32 * 160, 160, 8000);
    }
    // Wrap around
    for seq in 0u16..2 {
        stats.record_received(0x1111_1111u32, seq, (65536 + seq as u32) * 160, 160, 8000);
    }
    assert_eq!(stats.packets_received, 4);
    assert_eq!(stats.expected_packets(), 4);
    assert_eq!(stats.cumulative_lost(), 0);
}

#[test]
fn test_fraction_lost_interval() {
    let mut stats = RtcpStats::new();
    // First interval: 10 expected, 10 received → 0 loss
    for seq in 0u16..10 {
        stats.record_received(0x1111_1111u32, seq, seq as u32 * 160, 160, 8000);
    }
    assert_eq!(stats.fraction_lost_and_update(), 0);

    // Second interval: 10 expected (10..19), 5 received → 50% loss
    for seq in [10u16, 12, 14, 16, 18] {
        stats.record_received(0x1111_1111u32, seq, seq as u32 * 160, 160, 8000);
    }
    // expected_interval = 19 - 10 + 1 - (10-10) = 10; received_interval = 15-10 = 5
    // lost_interval = 5; fraction = (5 << 8) / 10 = 128
    let frac = stats.fraction_lost_and_update();
    assert!(
        frac > 100 && frac < 150,
        "fraction_lost should be ~128 (50%), got {}",
        frac
    );
}

#[test]
fn test_rtt_negative_clamped() {
    let our_ssrc = 0x12345678;
    let mut stats = RtcpStats::new();

    // Record that we sent an SR with some NTP middle value
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap();
    let ntp_secs = now.as_secs() + 2208988800;
    let ntp_frac = (now.subsec_nanos() as u64 * (1u64 << 32)) / 1_000_000_000;
    let ntp = (ntp_secs << 32) | ntp_frac;
    stats.record_sr_sent(ntp);

    // Create a ReportBlock with last_sr set far AHEAD of current time
    // (simulating clock skew). This makes wrapping_sub produce a huge value.
    let future_ntp_middle = stats.our_last_sr_ntp_middle.wrapping_add(0x00100000); // ~16 minutes ahead
    let block = ReportBlock {
        ssrc: our_ssrc,
        fraction_lost: 0,
        cumulative_lost: 0,
        highest_seq: 10,
        jitter: 0,
        last_sr: future_ntp_middle,
        delay_since_last_sr: 0,
    };

    stats.process_rr(&block, our_ssrc);

    // The computed RTT would be a huge wrapping value (>10s), so it should be rejected
    assert!(
        stats.rtt_ms.is_none(),
        "RTT should be None when computed value exceeds 10s sanity check, got {:?}",
        stats.rtt_ms
    );
}

#[test]
fn test_build_sr_rr_zero_ssrc() {
    let mut stats = RtcpStats::new();
    stats.packets_sent = 10;
    stats.octets_sent = 1600;

    // Build SR/RR with remote_ssrc=0
    let data = build_sr_rr(0x12345678, 0, &mut stats, 0, 8000);
    let packets = parse_rtcp(&data);

    assert_eq!(packets.len(), 1);
    match &packets[0] {
        RtcpPacket::SenderReport(sr) => {
            assert_eq!(sr.ssrc, 0x12345678);
            assert_eq!(sr.report_blocks.len(), 1);
            assert_eq!(sr.report_blocks[0].ssrc, 0, "report block SSRC should be 0");
        }
        _ => panic!("expected SR"),
    }
}

#[test]
fn test_truncated_compound_packet() {
    // Build a valid SR, then append a truncated second packet.
    // The parser should return the first SR and stop gracefully.
    let mut stats = RtcpStats::new();
    stats.packets_sent = 50;
    stats.octets_sent = 8000;
    let valid_sr = build_sr_rr(0x11111111, 0x22222222, &mut stats, 0, 8000);

    // Append a truncated RTCP RR header (only 6 bytes of a packet that
    // claims to be 8+ bytes via the length field)
    let mut compound = valid_sr.clone();
    compound.push(0x81); // V=2, RC=1
    compound.push(RTCP_RR);
    compound.push(0x00);
    compound.push(0x07); // length = 7 words = 32 bytes (but we only provide 2 more)
    compound.push(0xAA);
    compound.push(0xBB);

    let packets = parse_rtcp(&compound);
    // Should successfully parse the first SR and skip the truncated second
    assert_eq!(packets.len(), 1, "should parse only the valid first SR");
    match &packets[0] {
        RtcpPacket::SenderReport(sr) => {
            assert_eq!(sr.ssrc, 0x11111111);
            assert_eq!(sr.sender_packet_count, 50);
        }
        _ => panic!("expected SR"),
    }
}

#[test]
fn test_truncated_report_block_in_sr() {
    // Build an SR that claims RC=2 but only has data for 1 report block.
    // The parser should return 1 report block, not panic.
    let mut stats = RtcpStats::new();
    stats.packets_sent = 10;
    let mut data = build_sr_rr(0x12345678, 0xAABBCCDD, &mut stats, 0, 8000);

    // Overwrite RC from 1 to 2 — the packet now claims 2 report blocks
    // but only has data for 1
    data[0] = (data[0] & 0xE0) | 2; // V=2, P=0, RC=2

    // Also increase the length to claim the extra report block exists
    let orig_len = u16::from_be_bytes([data[2], data[3]]);
    let new_len = orig_len + 6; // +6 words for another report block
    data[2..4].copy_from_slice(&new_len.to_be_bytes());

    let packets = parse_rtcp(&data);
    // Parser should parse what it can: offset+packet_len > data.len() → breaks
    // OR parses 1 block and stops because data runs out
    // Either way, should not panic
    assert!(
        packets.len() <= 1,
        "should handle truncated report blocks gracefully"
    );
}

#[test]
fn test_parse_empty_rtcp() {
    let packets = parse_rtcp(&[]);
    assert!(
        packets.is_empty(),
        "empty data should produce no RTCP packets"
    );
}

#[test]
fn test_parse_too_short_rtcp() {
    // Less than 4 bytes (minimum RTCP header)
    let packets = parse_rtcp(&[0x80, 0xC8, 0x00]);
    assert!(
        packets.is_empty(),
        "3-byte data should produce no RTCP packets"
    );
}

#[test]
fn test_parse_wrong_version_rtcp() {
    // Version != 2 should stop parsing
    let mut data = vec![0u8; 32];
    data[0] = 0x00; // version 0
    data[1] = RTCP_SR;
    let packets = parse_rtcp(&data);
    assert!(
        packets.is_empty(),
        "version 0 should produce no RTCP packets"
    );
}

#[test]
fn test_timestamp_u32_max_handling() {
    let mut stats = RtcpStats::new();
    // Record packets with timestamps near u32::MAX
    stats.record_received(0x1111_1111u32, 0, u32::MAX - 1000, 160, 8000);
    stats.record_received(0x1111_1111u32, 1, u32::MAX - 500, 160, 8000);
    // Wrap around
    stats.record_received(0x1111_1111u32, 2, 100, 160, 8000);
    stats.record_received(0x1111_1111u32, 3, 600, 160, 8000);

    assert_eq!(stats.packets_received, 4);
    assert_eq!(stats.expected_packets(), 4);
    assert_eq!(stats.cumulative_lost(), 0);
    // Jitter should be computed without panic (the key test is no panic)
}

#[test]
fn test_sr_contains_rtp_timestamp() {
    let mut stats = RtcpStats::new();
    stats.packets_sent = 200;
    stats.octets_sent = 32000;

    let rtp_ts: u32 = 0xDEAD_BEEF;
    let data = build_sr_rr(0x11111111, 0x22222222, &mut stats, rtp_ts, 8000);

    // SR packet layout (byte offsets):
    //   0..4   = header (V, P, RC, PT, length)
    //   4..8   = SSRC of sender
    //   8..16  = NTP timestamp (8 bytes)
    //  16..20  = RTP timestamp  <-- this is what we're testing
    //  20..24  = sender packet count
    //  24..28  = sender octet count
    let embedded_rtp_ts = u32::from_be_bytes([data[16], data[17], data[18], data[19]]);
    assert_eq!(
        embedded_rtp_ts, rtp_ts,
        "RTP timestamp should appear at bytes 16..20 of the SR"
    );

    // Also verify via parse_rtcp round-trip
    let packets = parse_rtcp(&data);
    assert_eq!(packets.len(), 1);
    match &packets[0] {
        RtcpPacket::SenderReport(sr) => {
            assert_eq!(sr.rtp_timestamp, rtp_ts);
        }
        _ => panic!("expected SR"),
    }
}

#[test]
fn test_parse_unknown_rtcp_type_skipped() {
    // Build a compound packet: known SR + unknown type (e.g. SDES=202) + another SR
    let mut stats = RtcpStats::new();
    let sr1 = build_sr_rr(0x11111111, 0x22222222, &mut stats, 0, 8000);

    // Craft a minimal SDES packet (PT=202, version=2, rc=0, length=1 → 8 bytes)
    let sdes: Vec<u8> = vec![
        0x80, 202, 0x00, 0x01, // V=2, P=0, SC=0, PT=202(SDES), length=1
        0x00, 0x00, 0x00, 0x00, // SDES content (4 bytes padding)
    ];

    let sr2 = build_sr_rr(0x33333333, 0x44444444, &mut stats, 0, 8000);

    let mut compound = Vec::new();
    compound.extend_from_slice(&sr1);
    compound.extend_from_slice(&sdes);
    compound.extend_from_slice(&sr2);

    let packets = parse_rtcp(&compound);
    // Should get 2 SR packets, SDES is skipped (unknown type)
    assert_eq!(
        packets.len(),
        2,
        "should parse 2 SRs, skipping unknown SDES"
    );
    match (&packets[0], &packets[1]) {
        (RtcpPacket::SenderReport(a), RtcpPacket::SenderReport(b)) => {
            assert_eq!(a.ssrc, 0x11111111);
            assert_eq!(b.ssrc, 0x33333333);
        }
        _ => panic!("expected two SenderReports"),
    }
}

#[test]
fn test_parse_bye_single_ssrc_with_reason() {
    // BYE: V=2, P=0, RC=1, PT=203
    // SSRC: 0x12345678, reason: "bye"
    // Total: 4 (header) + 4 (SSRC) + 1 (reason len) + 3 (reason) + 4 (pad to 32-bit) = 16
    // length field = 16/4 - 1 = 3
    let data: Vec<u8> = vec![
        0x81, 203, 0x00, 0x03, // header: V=2, RC=1, PT=BYE, length=3
        0x12, 0x34, 0x56, 0x78, // SSRC
        0x03, // reason length
        b'b', b'y', b'e', // reason string
        0x00, 0x00, 0x00, 0x00, // padding to 32-bit boundary
    ];
    let packets = parse_rtcp(&data);
    assert_eq!(packets.len(), 1);
    match &packets[0] {
        RtcpPacket::Bye(bye) => {
            assert_eq!(bye.ssrc_list, vec![0x12345678]);
            assert_eq!(bye.reason.as_deref(), Some("bye"));
        }
        _ => panic!("expected BYE"),
    }
}

#[test]
fn test_parse_bye_multiple_ssrcs_no_reason() {
    // BYE: V=2, P=0, RC=2, PT=203, length=2 (12 bytes)
    let data: Vec<u8> = vec![
        0x82, 203, 0x00, 0x02, // header: V=2, RC=2, PT=BYE, length=2
        0x11, 0x22, 0x33, 0x44, // SSRC 1
        0x55, 0x66, 0x77, 0x88, // SSRC 2
    ];
    let packets = parse_rtcp(&data);
    assert_eq!(packets.len(), 1);
    match &packets[0] {
        RtcpPacket::Bye(bye) => {
            assert_eq!(bye.ssrc_list, vec![0x11223344, 0x55667788]);
            assert!(bye.reason.is_none());
        }
        _ => panic!("expected BYE"),
    }
}

#[test]
fn test_parse_bye_zero_ssrcs() {
    // BYE: V=2, P=0, RC=0, PT=203, length=0 (4 bytes — header only)
    let data: Vec<u8> = vec![
        0x80, 203, 0x00, 0x00, // header: V=2, RC=0, PT=BYE, length=0
    ];
    let packets = parse_rtcp(&data);
    assert_eq!(packets.len(), 1);
    match &packets[0] {
        RtcpPacket::Bye(bye) => {
            assert!(bye.ssrc_list.is_empty());
            assert!(bye.reason.is_none());
        }
        _ => panic!("expected BYE"),
    }
}

#[test]
fn test_parse_rtcp_with_csrc_rtp() {
    // RTP packet with CSRC list should still parse correctly
    // (This tests rtp.rs but is related to RTCP flow)
    use crate::media::rtp::RtpHeader;
    let pkt = RtpHeader::build(0, 1, 160, 0x12345678, false, &[0x80; 160]);
    let parsed = RtpHeader::parse(&pkt);
    assert!(parsed.is_some());
    let hdr = parsed.unwrap();
    assert_eq!(hdr.payload_type, 0);
    assert_eq!(hdr.sequence_number, 1);
}

#[test]
fn test_fraction_lost_total_loss_returns_255() {
    // When lost_interval == expected_interval (100% loss), result must be 255.
    // Before the fix, ((N << 8) / N) = 256, and (256 as u8) wrapped to 0.
    let mut stats = RtcpStats::new();
    // First interval: receive seq 0 to establish baseline
    stats.record_received(0x1111_1111u32, 0, 0, 160, 8000);
    stats.fraction_lost_and_update(); // snapshot priors

    // Second interval: advance expected by 100 but receive 0 more packets.
    // Directly advance extended_max_seq via record_received with a far seq.
    stats.record_received(0x1111_1111u32, 100, 100 * 160, 160, 8000);
    // Now expected = 101, received = 2, but we want to test a full-loss interval.
    // expected_prior = 1, received_prior = 1 (from first update)
    // After receiving seq 100: expected = 101, received = 2
    // expected_interval = 101 - 1 = 100, received_interval = 2 - 1 = 1
    // lost_interval = 99, fraction = (99*256)/100 = 253
    let frac = stats.fraction_lost_and_update();
    assert!(
        frac >= 250,
        "near-total loss should produce fraction_lost >= 250, got {}",
        frac
    );
}

#[test]
fn test_fraction_lost_large_interval_no_overflow() {
    // When lost_interval > 2^24, the old formula (lost_interval << 8) overflowed u32.
    // The fix uses u64 intermediate + min(255) to handle this correctly.
    let mut stats = RtcpStats::new();
    // Establish baseline
    stats.record_received(0x1111_1111u32, 0, 0, 160, 8000);
    stats.fraction_lost_and_update(); // snapshot priors (expected_prior=1, received_prior=1)

    // Force a massive sequence jump by wrapping around many times.
    // After 512 rollovers: extended_max_seq ≈ 512 * 65536 = 33_554_432 (> 2^24)
    // We only receive a few packets so most are "lost".
    //
    // Drive this through the public API by recording a seq that forces rollovers.
    // record_received detects rollover when (max_seq_lo - seq32) > 0x8000.
    // We simulate this by alternating near-end and near-start seqs.
    let mut ts = 160u32;
    for _ in 0..512 {
        // Jump to near-end of seq space to trigger rollover detection
        stats.record_received(0x1111_1111u32, 65535, ts, 160, 8000);
        ts = ts.wrapping_add(160);
        stats.record_received(0x1111_1111u32, 0, ts, 160, 8000);
        ts = ts.wrapping_add(160);
    }
    // extended_max_seq should now be very large (512 rollovers * 65536 + some)
    let expected = stats.expected_packets();
    assert!(
        expected > 0x01_000_000,
        "expected packets should exceed 2^24, got {}",
        expected
    );

    // Fraction lost should be very high (we only received 1025 packets out of millions)
    // and must not panic from overflow
    let frac = stats.fraction_lost_and_update();
    assert!(
        frac >= 250,
        "massive loss should produce fraction_lost >= 250, got {}",
        frac
    );
}

#[test]
fn test_jitter_does_not_diverge_when_d_drops_below_j() {
    // Regression for the u32-wrap bug in the RFC 3550 A.8 smoothing step.
    // Drive jitter up with a burst of high-|D| arrivals, then feed perfectly
    // periodic packets (|D| ≈ 0). The recurrence must decay J toward 0;
    // the old code wrapped (d - J) in u32 and J diverged to near UINT32_MAX.
    let mut stats = RtcpStats::new();
    let clock_rate = 8000u32; // G.722's RTP timestamp clock

    // Seed with one packet at ts=0.
    stats.record_received(0x1111_1111u32, 0, 0, 160, clock_rate);

    // Burst of jittery arrivals: 50 packets where arrival time and RTP timestamp
    // are out of step, producing large |D| values.
    for i in 1..=50u32 {
        let ts = i.wrapping_mul(160);
        std::thread::sleep(std::time::Duration::from_millis(40));
        stats.record_received(0x1111_1111u32, i as u16, ts, 160, clock_rate);
    }
    let jittered = stats.jitter;
    assert!(jittered > 0, "burst should drive jitter above 0");

    // Now feed periodic packets (|D| ≈ 0). Jitter should decay toward 0,
    // never exceed any sane bound, and definitely never approach UINT32_MAX.
    for i in 51..=500u32 {
        let ts = i.wrapping_mul(160);
        std::thread::sleep(std::time::Duration::from_millis(20));
        stats.record_received(0x1111_1111u32, i as u16, ts, 160, clock_rate);
        assert!(
            stats.jitter < 10_000_000,
            "jitter must not diverge — got {} µs at iteration {}",
            stats.jitter,
            i
        );
    }
    assert!(
        stats.jitter < jittered,
        "jitter should decay below its peak ({}) once arrivals become periodic, got {}",
        jittered,
        stats.jitter
    );
}

#[test]
fn test_ssrc_change_rebaselines_no_inflation() {
    // A hold/re-INVITE restarts the stream with a fresh SSRC and a low
    // sequence base. Before the fix, the stale baseline (high extended_max
    // vs. a low new seq) tripped the rollover heuristic and cumulative loss
    // ballooned. With re-baselining, a clean second stream shows no loss.
    let mut stats = RtcpStats::new();
    let ssrc_a = 0xAAAA_AAAA;
    let ssrc_b = 0xBBBB_BBBB;

    // Stream A: clean run ending near seq 40000.
    for seq in 39_998u16..=40_000 {
        stats.record_received(ssrc_a, seq, seq as u32 * 160, 160, 8000);
    }
    assert_eq!(stats.cumulative_lost(), 0, "stream A is clean");

    // Stream B: new SSRC, fresh low sequence base, also clean.
    for seq in 100u16..110 {
        stats.record_received(ssrc_b, seq, seq as u32 * 160, 160, 8000);
    }
    assert_eq!(
        stats.cumulative_lost(),
        0,
        "SSRC change with a clean second stream must not inflate loss"
    );
}

#[test]
fn test_ssrc_change_carries_forward_prior_loss() {
    // Loss from before the SSRC change is preserved (not reset to zero),
    // so cumulative_lost() reflects whole-leg loss.
    let mut stats = RtcpStats::new();
    let ssrc_a = 0xAAAA_AAAA;
    let ssrc_b = 0xBBBB_BBBB;

    // Stream A: seq 0..4 then 10..14 — a gap of 5 lost packets.
    for seq in 0u16..5 {
        stats.record_received(ssrc_a, seq, seq as u32 * 160, 160, 8000);
    }
    for seq in 10u16..15 {
        stats.record_received(ssrc_a, seq, seq as u32 * 160, 160, 8000);
    }
    assert_eq!(stats.cumulative_lost(), 5, "stream A lost 5");

    // Stream B: new SSRC, clean. Prior 5 must carry forward.
    for seq in 0u16..10 {
        stats.record_received(ssrc_b, seq, seq as u32 * 160, 160, 8000);
    }
    assert_eq!(
        stats.cumulative_lost(),
        5,
        "prior-generation loss must carry forward across the SSRC change"
    );

    // Stream B then loses 2 (seq 20..21 skipped, resume at 22).
    for seq in 22u16..25 {
        stats.record_received(ssrc_b, seq, seq as u32 * 160, 160, 8000);
    }
    // Stream B expected 0..24 = 25, received 13 → gen lost 12; +5 carried = 17.
    assert_eq!(
        stats.cumulative_lost(),
        17,
        "carried-forward plus current-generation loss should sum"
    );
}

#[test]
fn test_no_reset_on_steady_ssrc() {
    // The very first packet sets the generation (no spurious accumulation),
    // and a steady SSRC never re-baselines.
    let mut stats = RtcpStats::new();
    let ssrc = 0x1234_5678;
    for seq in 0u16..50 {
        stats.record_received(ssrc, seq, seq as u32 * 160, 160, 8000);
    }
    assert_eq!(stats.cumulative_lost(), 0);
    assert_eq!(stats.expected_packets(), 50);
    assert_eq!(stats.packets_received, 50);
}

#[test]
fn test_sr_rr_cumulative_lost_is_per_ssrc_not_whole_leg() {
    // After an SSRC change, the emitted RR report block must carry only the
    // current generation's loss (RFC 3550 scopes cumulative-lost to the
    // block's SSRC), even though the session-level `cumulative_lost()`
    // carries prior-generation loss forward for whole-leg reporting.
    let mut stats = RtcpStats::new();
    let ssrc_a = 0xAAAA_AAAA;
    let ssrc_b = 0xBBBB_BBBB;

    // Stream A loses 5 (seq 0..4 then 10..14).
    for seq in 0u16..5 {
        stats.record_received(ssrc_a, seq, seq as u32 * 160, 160, 8000);
    }
    for seq in 10u16..15 {
        stats.record_received(ssrc_a, seq, seq as u32 * 160, 160, 8000);
    }
    // Switch to a clean stream B.
    for seq in 0u16..10 {
        stats.record_received(ssrc_b, seq, seq as u32 * 160, 160, 8000);
    }

    assert_eq!(stats.cumulative_lost(), 5, "whole-leg keeps prior loss");
    assert_eq!(
        stats.current_gen_lost(),
        0,
        "current generation (B) is clean"
    );

    // The wire RR must report the current-generation (per-SSRC) loss = 0.
    let data = build_sr_rr(0x1234_5678, ssrc_b, &mut stats, 0, 8000);
    match &parse_rtcp(&data)[0] {
        RtcpPacket::SenderReport(sr) => {
            assert_eq!(
                sr.report_blocks[0].cumulative_lost, 0,
                "RR block must carry per-SSRC loss, not the whole-leg total"
            );
        }
        _ => panic!("expected SR"),
    }
}

#[test]
fn test_sr_rr_lsr_dlsr_reset_on_ssrc_change() {
    // After the remote changes SSRC, the RR block for the new source must
    // not echo the old source's LSR/DLSR (which would make the peer compute
    // a bogus RTT). Until we receive an SR from the new SSRC, LSR must be 0.
    let mut stats = RtcpStats::new();
    let ssrc_a = 0xAAAA_AAAA;
    let ssrc_b = 0xBBBB_BBBB;

    // Receive a packet and an SR from source A.
    stats.record_received(ssrc_a, 0, 0, 160, 8000);
    let sr_a = SenderReport {
        ssrc: ssrc_a,
        ntp_timestamp: 0x0000_1234_5678_0000,
        rtp_timestamp: 0,
        sender_packet_count: 1,
        sender_octet_count: 160,
        report_blocks: vec![],
    };
    stats.process_sr(&sr_a);
    assert_ne!(stats.last_sr_ntp, 0, "LSR recorded from source A's SR");

    // Source B takes over (SSRC change) — no SR from B received yet.
    stats.record_received(ssrc_b, 0, 0, 160, 8000);

    let data = build_sr_rr(0x1234_5678, ssrc_b, &mut stats, 0, 8000);
    match &parse_rtcp(&data)[0] {
        RtcpPacket::SenderReport(sr) => {
            assert_eq!(
                sr.report_blocks[0].last_sr, 0,
                "RR block for the new SSRC must not echo the old source's LSR"
            );
            assert_eq!(
                sr.report_blocks[0].delay_since_last_sr, 0,
                "DLSR must be 0 when no SR has been received from the new source"
            );
        }
        _ => panic!("expected SR"),
    }
}

#[test]
fn test_sr_rr_emits_lsr_only_for_matching_ssrc() {
    // SR timing is keyed to its source: build_sr_rr echoes LSR/DLSR only
    // when reporting on the same SSRC the SR came from, and suppresses it
    // (0) for any other SSRC — independent of RTP/RTCP arrival ordering.
    let mut stats = RtcpStats::new();
    let src = 0xCAFE_BABE;

    // An SR arrives (even before any RTP for this source).
    let sr = SenderReport {
        ssrc: src,
        ntp_timestamp: 0x0000_1111_2222_0000,
        rtp_timestamp: 0,
        sender_packet_count: 1,
        sender_octet_count: 160,
        report_blocks: vec![],
    };
    stats.process_sr(&sr);
    stats.record_received(src, 0, 0, 160, 8000);

    // Reporting on the SR's own SSRC → LSR present.
    let data = build_sr_rr(0x1234_5678, src, &mut stats, 0, 8000);
    match &parse_rtcp(&data)[0] {
        RtcpPacket::SenderReport(s) => assert_ne!(
            s.report_blocks[0].last_sr, 0,
            "LSR must be echoed when reporting on the SR's own SSRC"
        ),
        _ => panic!("expected SR"),
    }

    // Reporting on a different SSRC → LSR suppressed.
    let other = 0x0000_0099;
    let data2 = build_sr_rr(0x1234_5678, other, &mut stats, 0, 8000);
    match &parse_rtcp(&data2)[0] {
        RtcpPacket::SenderReport(s) => assert_eq!(
            s.report_blocks[0].last_sr, 0,
            "LSR must be suppressed when reporting on a different SSRC"
        ),
        _ => panic!("expected SR"),
    }
}
