use super::*;

fn make_test_key() -> String {
    // 30 bytes of test key material, base64 encoded
    let key_material = [
        0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B, 0x0C, 0x0D, 0x0E, 0x0F,
        0x10, // 16-byte master key
        0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1A, 0x1B, 0x1C, 0x1D,
        0x1E, // 14-byte master salt
    ];
    // Encode to base64
    crate::session::endpoint_rtp::base64_encode(&key_material)
}

#[test]
fn test_srtp_roundtrip() {
    let key = make_test_key();
    let mut protect_ctx = SrtpContext::from_sdes_key(&key).unwrap();
    let mut unprotect_ctx = SrtpContext::from_sdes_key(&key).unwrap();

    // Build a simple RTP packet
    let rtp = crate::media::rtp::RtpHeader::build(0, 1, 160, 0x12345678, false, &[0xAA; 160]);

    // Protect (encrypt + auth)
    let srtp = protect_ctx.protect(&rtp).unwrap();
    assert_eq!(srtp.len(), rtp.len() + SRTP_AUTH_TAG_LEN);

    // Payload should be different (encrypted)
    assert_ne!(&srtp[12..12 + 160], &rtp[12..12 + 160]);

    // Unprotect (verify + decrypt)
    let decrypted = unprotect_ctx.unprotect(&srtp).unwrap();
    assert_eq!(decrypted, rtp);
}

#[test]
fn test_srtp_auth_failure() {
    let key = make_test_key();
    let mut protect_ctx = SrtpContext::from_sdes_key(&key).unwrap();
    let mut unprotect_ctx = SrtpContext::from_sdes_key(&key).unwrap();

    let rtp = crate::media::rtp::RtpHeader::build(0, 1, 160, 0x12345678, false, &[0xBB; 80]);
    let mut srtp = protect_ctx.protect(&rtp).unwrap();

    // Tamper with a byte
    srtp[20] ^= 0xFF;

    // Should fail auth
    assert!(unprotect_ctx.unprotect(&srtp).is_err());
}

#[test]
fn test_srtp_multiple_packets() {
    let key = make_test_key();
    let mut protect_ctx = SrtpContext::from_sdes_key(&key).unwrap();
    let mut unprotect_ctx = SrtpContext::from_sdes_key(&key).unwrap();

    for seq in 0..10u16 {
        let rtp = crate::media::rtp::RtpHeader::build(
            0,
            seq,
            seq as u32 * 160,
            0xABCD,
            false,
            &[seq as u8; 160],
        );
        let srtp = protect_ctx.protect(&rtp).unwrap();
        let decrypted = unprotect_ctx.unprotect(&srtp).unwrap();
        assert_eq!(decrypted, rtp, "roundtrip failed for seq {}", seq);
    }
}

#[test]
fn test_srtp_replay_rejected() {
    let key = make_test_key();
    let mut protect_ctx = SrtpContext::from_sdes_key(&key).unwrap();
    let mut unprotect_ctx = SrtpContext::from_sdes_key(&key).unwrap();

    let rtp = crate::media::rtp::RtpHeader::build(0, 1, 160, 0x12345678, false, &[0xCC; 160]);
    let srtp = protect_ctx.protect(&rtp).unwrap();

    // First unprotect should succeed
    let decrypted = unprotect_ctx.unprotect(&srtp).unwrap();
    assert_eq!(decrypted, rtp);

    // Replaying the same packet should fail
    let result = unprotect_ctx.unprotect(&srtp);
    assert!(result.is_err(), "replay should be rejected");
    assert!(result.unwrap_err().to_string().contains("replay"));
}

#[test]
fn test_srtp_old_packet_outside_window_rejected() {
    let key = make_test_key();
    let mut protect_ctx = SrtpContext::from_sdes_key(&key).unwrap();
    let mut unprotect_ctx = SrtpContext::from_sdes_key(&key).unwrap();

    // Send packet seq=0
    let rtp0 = crate::media::rtp::RtpHeader::build(0, 0, 0, 0xABCD, false, &[0x00; 80]);
    let srtp0 = protect_ctx.protect(&rtp0).unwrap();

    // Advance well past the 64-packet window
    for seq in 1..=100u16 {
        let rtp = crate::media::rtp::RtpHeader::build(
            0,
            seq,
            seq as u32 * 160,
            0xABCD,
            false,
            &[seq as u8; 80],
        );
        let srtp = protect_ctx.protect(&rtp).unwrap();
        unprotect_ctx.unprotect(&srtp).unwrap();
    }

    // Now try to unprotect seq=0 — should be rejected (outside window)
    let result = unprotect_ctx.unprotect(&srtp0);
    assert!(
        result.is_err(),
        "old packet outside window should be rejected"
    );
}

#[test]
fn test_srtp_reset_sequence_state_accepts_restarted_low_seq() {
    // Reproduces the post-hold "no audio" bug: peer sends RTCP BYE on hold,
    // then resumes with a fresh low sequence number on unhold. Without
    // reset_sequence_state(), the replay window rejects the resumed packets
    // as "too old" and decrypt fails silently for the entire call.
    let key = make_test_key();
    let mut protect_ctx = SrtpContext::from_sdes_key(&key).unwrap();
    let mut unprotect_ctx = SrtpContext::from_sdes_key(&key).unwrap();

    // Phone runs up to seq=200 before going on hold
    for seq in 1..=200u16 {
        let rtp = crate::media::rtp::RtpHeader::build(
            0,
            seq,
            seq as u32 * 160,
            0xABCD,
            false,
            &[seq as u8; 80],
        );
        let srtp = protect_ctx.protect(&rtp).unwrap();
        unprotect_ctx.unprotect(&srtp).unwrap();
    }

    // Phone unholds, restarts its protect context with the SAME SSRC but a
    // fresh low sequence (a new SSRC would be auto-accepted per-stream; the
    // case that still needs a reset is a same-SSRC sequence restart).
    let mut restarted_protect = SrtpContext::from_sdes_key(&key).unwrap();
    let restart_rtp = crate::media::rtp::RtpHeader::build(0, 1, 160, 0xABCD, false, &[0xAA; 80]);
    let restart_srtp = restarted_protect.protect(&restart_rtp).unwrap();

    // Without reset, the low-seq packet on the SAME SSRC must be rejected.
    let pre_reset = unprotect_ctx.unprotect(&restart_srtp);
    assert!(
        pre_reset.is_err(),
        "sanity: same-SSRC low-seq packet on a stale context should be rejected before reset"
    );

    // After reset_sequence_state, the same packet must decrypt successfully
    unprotect_ctx.reset_sequence_state();
    let decrypted = unprotect_ctx
        .unprotect(&restart_srtp)
        .expect("restarted low-seq packet should decrypt after reset");
    assert_eq!(decrypted, restart_rtp);
}

#[test]
fn test_srtp_reset_sequence_state_preserves_keys() {
    // The key derivation should survive a reset — only the replay/sequence
    // tracking is cleared. Verified by checking that the cipher_key/auth_key
    // bytes are unchanged across a reset.
    let key = make_test_key();
    let mut ctx = SrtpContext::from_sdes_key(&key).unwrap();

    // Pump the context so it has non-default state
    let rtp = crate::media::rtp::RtpHeader::build(0, 50, 8000, 0x11223344, false, &[0x55; 80]);
    let srtp = {
        let mut twin = SrtpContext::from_sdes_key(&key).unwrap();
        twin.protect(&rtp).unwrap()
    };
    ctx.unprotect(&srtp).unwrap();

    let cipher_before = ctx.cipher_key;
    let salt_before = ctx.cipher_salt;
    let auth_before = ctx.auth_key;

    ctx.reset_sequence_state();

    assert_eq!(
        ctx.cipher_key, cipher_before,
        "cipher_key must be preserved"
    );
    assert_eq!(
        ctx.cipher_salt, salt_before,
        "cipher_salt must be preserved"
    );
    assert_eq!(ctx.auth_key, auth_before, "auth_key must be preserved");
    assert!(
        ctx.streams.is_empty(),
        "reset must drop all per-SSRC sequence/replay state"
    );
}

#[test]
fn test_srtcp_reset_recv_state_accepts_restarted_low_index() {
    // SRTCP analogue of the SRTP test: peer restarts with a fresh SRTCP
    // index of 0 after RTCP BYE / hold. Without reset_recv_state(), the
    // replay window rejects the restarted low-index packets.
    let key = make_test_key();
    let mut protect_ctx = SrtcpContext::from_sdes_key(&key).unwrap();
    let mut unprotect_ctx = SrtcpContext::from_sdes_key(&key).unwrap();

    let mut stats = crate::media::rtcp::RtcpStats::new();
    let rtcp = crate::media::rtcp::build_sr_rr(0x11111111, 0x22222222, &mut stats, 0, 8000);

    // Run the protect context past the 64-packet replay window
    for _ in 0..200 {
        let srtcp = protect_ctx.protect_rtcp(&rtcp).unwrap();
        unprotect_ctx.unprotect_rtcp(&srtcp).unwrap();
    }

    // Peer restarts SRTCP from index 0
    let mut restarted_protect = SrtcpContext::from_sdes_key(&key).unwrap();
    let restart_srtcp = restarted_protect.protect_rtcp(&rtcp).unwrap();

    let pre_reset = unprotect_ctx.unprotect_rtcp(&restart_srtcp);
    assert!(
        pre_reset.is_err(),
        "sanity: low-index SRTCP on a stale context should be rejected before reset"
    );

    unprotect_ctx.reset_recv_state();
    unprotect_ctx
        .unprotect_rtcp(&restart_srtcp)
        .expect("restarted low-index SRTCP should decrypt after reset");
}

#[test]
fn test_base64_decode() {
    let decoded = base64_decode("AQIDBA==").unwrap();
    assert_eq!(decoded, vec![1, 2, 3, 4]);
}

#[test]
fn test_base64_invalid_chars() {
    let result = base64_decode("AQID!A==");
    assert!(
        result.is_err(),
        "base64_decode should reject invalid characters"
    );
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("Invalid base64"),
        "error message should mention invalid base64: {}",
        err_msg
    );
}

// ---- SRTCP tests ----

#[test]
fn test_srtcp_roundtrip() {
    let key = make_test_key();
    let mut protect_ctx = SrtcpContext::from_sdes_key(&key).unwrap();
    let mut unprotect_ctx = SrtcpContext::from_sdes_key(&key).unwrap();

    // Build a simple RTCP SR (use the rtcp module)
    let mut stats = crate::media::rtcp::RtcpStats::new();
    stats.packets_sent = 50;
    stats.octets_sent = 8000;
    let rtcp = crate::media::rtcp::build_sr_rr(0x12345678, 0xAABBCCDD, &mut stats, 0, 8000);

    let srtcp = protect_ctx.protect_rtcp(&rtcp).unwrap();
    // Should be longer: +4 (E+index) +10 (auth tag)
    assert_eq!(srtcp.len(), rtcp.len() + 4 + SRTP_AUTH_TAG_LEN);

    // First 8 bytes (header+SSRC) should be in the clear
    assert_eq!(&srtcp[..4], &rtcp[..4], "RTCP header should be in clear");
    assert_eq!(&srtcp[4..8], &rtcp[4..8], "SSRC should be in clear");

    // Payload should be different (encrypted)
    if rtcp.len() > 8 {
        assert_ne!(
            &srtcp[8..rtcp.len()],
            &rtcp[8..],
            "payload should be encrypted"
        );
    }

    let decrypted = unprotect_ctx.unprotect_rtcp(&srtcp).unwrap();
    assert_eq!(decrypted, rtcp);
}

#[test]
fn test_srtcp_auth_failure() {
    let key = make_test_key();
    let mut protect_ctx = SrtcpContext::from_sdes_key(&key).unwrap();
    let mut unprotect_ctx = SrtcpContext::from_sdes_key(&key).unwrap();

    let mut stats = crate::media::rtcp::RtcpStats::new();
    let rtcp = crate::media::rtcp::build_sr_rr(0x12345678, 0xAABBCCDD, &mut stats, 0, 8000);
    let mut srtcp = protect_ctx.protect_rtcp(&rtcp).unwrap();

    // Tamper with the encrypted payload
    srtcp[10] ^= 0xFF;

    assert!(unprotect_ctx.unprotect_rtcp(&srtcp).is_err());
}

#[test]
fn test_srtp_sequence_wraparound() {
    let key = make_test_key();
    let mut protect_ctx = SrtpContext::from_sdes_key(&key).unwrap();
    let mut unprotect_ctx = SrtpContext::from_sdes_key(&key).unwrap();

    // Send a realistic stream of packets up through and past the 16-bit
    // sequence number wraparound boundary (65534, 65535, 0, 1, ...).
    // Start well before the boundary to establish steady state.
    let start: u16 = 65500;
    let count: u32 = 100; // crosses from 65500 through 0 up to ~64

    for i in 0..count {
        let seq = start.wrapping_add(i as u16);
        let ts = i.wrapping_mul(160);
        let rtp =
            crate::media::rtp::RtpHeader::build(0, seq, ts, 0x12345678, false, &[seq as u8; 160]);
        let srtp = protect_ctx.protect(&rtp).unwrap();
        let decrypted = unprotect_ctx
            .unprotect(&srtp)
            .unwrap_or_else(|e| panic!("unprotect failed for seq {} (i={}): {}", seq, i, e));
        assert_eq!(decrypted, rtp, "roundtrip failed for seq {} (i={})", seq, i);
    }
}

#[test]
fn test_srtp_out_of_order_within_window() {
    let key = make_test_key();
    let mut protect_ctx = SrtpContext::from_sdes_key(&key).unwrap();
    let mut unprotect_ctx = SrtpContext::from_sdes_key(&key).unwrap();

    // Protect packets 0..=4 in order
    let mut srtp_packets = Vec::new();
    for seq in 0u16..=4 {
        let rtp = crate::media::rtp::RtpHeader::build(
            0,
            seq,
            seq as u32 * 160,
            0xABCD,
            false,
            &[seq as u8; 80],
        );
        let srtp = protect_ctx.protect(&rtp).unwrap();
        srtp_packets.push((seq, rtp, srtp));
    }

    // Unprotect in out-of-order sequence: 0, 2, 4, 1, 3
    let unprotect_order = [0usize, 2, 4, 1, 3];
    for &idx in &unprotect_order {
        let (seq, ref original_rtp, ref srtp) = srtp_packets[idx];
        let decrypted = unprotect_ctx
            .unprotect(srtp)
            .unwrap_or_else(|e| panic!("unprotect failed for seq {} (out-of-order): {}", seq, e));
        assert_eq!(
            &decrypted, original_rtp,
            "roundtrip failed for seq {} (out-of-order)",
            seq
        );
    }
}

#[test]
fn test_srtcp_different_keys_from_srtp() {
    let key = make_test_key();
    let srtp_ctx = SrtpContext::from_sdes_key(&key).unwrap();
    let srtcp_ctx = SrtcpContext::from_sdes_key(&key).unwrap();

    // SRTCP uses labels 0x03/0x04/0x05 vs SRTP 0x00/0x01/0x02
    // so derived keys must differ
    assert_ne!(srtp_ctx.cipher_key, srtcp_ctx.cipher_key);
    assert_ne!(srtp_ctx.auth_key, srtcp_ctx.auth_key);
    assert_ne!(srtp_ctx.cipher_salt, srtcp_ctx.cipher_salt);
}

#[test]
fn test_srtcp_multiple_packets() {
    let key = make_test_key();
    let mut protect_ctx = SrtcpContext::from_sdes_key(&key).unwrap();
    let mut unprotect_ctx = SrtcpContext::from_sdes_key(&key).unwrap();

    for i in 0..5 {
        let mut stats = crate::media::rtcp::RtcpStats::new();
        stats.packets_sent = i * 10;
        let rtcp = crate::media::rtcp::build_sr_rr(0x12345678, 0xAABBCCDD, &mut stats, 0, 8000);
        let srtcp = protect_ctx.protect_rtcp(&rtcp).unwrap();
        let decrypted = unprotect_ctx.unprotect_rtcp(&srtcp).unwrap();
        assert_eq!(decrypted, rtcp, "SRTCP roundtrip failed for packet {}", i);
    }
}

#[test]
fn test_srtcp_replay_rejected() {
    let key = make_test_key();
    let mut protect_ctx = SrtcpContext::from_sdes_key(&key).unwrap();
    let mut unprotect_ctx = SrtcpContext::from_sdes_key(&key).unwrap();

    let mut stats = crate::media::rtcp::RtcpStats::new();
    let rtcp = crate::media::rtcp::build_sr_rr(0x12345678, 0xAABBCCDD, &mut stats, 0, 8000);
    let srtcp = protect_ctx.protect_rtcp(&rtcp).unwrap();

    // First unprotect should succeed
    unprotect_ctx.unprotect_rtcp(&srtcp).unwrap();

    // Replaying the same packet should fail
    let result = unprotect_ctx.unprotect_rtcp(&srtcp);
    assert!(result.is_err(), "SRTCP replay should be rejected");
    assert!(result.unwrap_err().to_string().contains("replay"));
}

#[test]
fn test_srtcp_old_packet_outside_window_rejected() {
    let key = make_test_key();
    let mut protect_ctx = SrtcpContext::from_sdes_key(&key).unwrap();
    let mut unprotect_ctx = SrtcpContext::from_sdes_key(&key).unwrap();

    let mut stats = crate::media::rtcp::RtcpStats::new();

    // Protect 100 RTCP packets
    let mut srtcp_packets = Vec::new();
    for _ in 0..100 {
        stats.packets_sent += 10;
        let rtcp = crate::media::rtcp::build_sr_rr(0x12345678, 0xAABBCCDD, &mut stats, 0, 8000);
        srtcp_packets.push(protect_ctx.protect_rtcp(&rtcp).unwrap());
    }

    // Unprotect all in order
    for srtcp in &srtcp_packets {
        unprotect_ctx.unprotect_rtcp(srtcp).unwrap();
    }

    // Replaying packet 0 should fail (outside 64-packet window)
    let result = unprotect_ctx.unprotect_rtcp(&srtcp_packets[0]);
    assert!(
        result.is_err(),
        "old SRTCP packet outside window should be rejected"
    );
}

#[test]
fn test_srtcp_index_increments() {
    let key = make_test_key();
    let mut protect_ctx = SrtcpContext::from_sdes_key(&key).unwrap();

    let mut stats = crate::media::rtcp::RtcpStats::new();
    let rtcp = crate::media::rtcp::build_sr_rr(0x12345678, 0xAABBCCDD, &mut stats, 0, 8000);

    // Protect 3 RTCP packets and extract the E+index field from each
    let mut indices = Vec::new();
    for _ in 0..3 {
        let srtcp = protect_ctx.protect_rtcp(&rtcp).unwrap();
        // E+index is 4 bytes just before the 10-byte auth tag
        let idx_offset = srtcp.len() - SRTP_AUTH_TAG_LEN - 4;
        let e_and_index = u32::from_be_bytes([
            srtcp[idx_offset],
            srtcp[idx_offset + 1],
            srtcp[idx_offset + 2],
            srtcp[idx_offset + 3],
        ]);
        indices.push(e_and_index);
    }

    // E-flag should be set (0x80000000) and indices should be 0, 1, 2
    assert_eq!(
        indices[0],
        0x80000000 | 0,
        "first packet should have index 0"
    );
    assert_eq!(
        indices[1],
        0x80000000 | 1,
        "second packet should have index 1"
    );
    assert_eq!(
        indices[2],
        0x80000000 | 2,
        "third packet should have index 2"
    );
}

#[test]
fn test_srtp_different_keys_fail() {
    // Create two SrtpContexts with DIFFERENT keys
    let key_material_a = [
        0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B, 0x0C, 0x0D, 0x0E, 0x0F,
        0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1A, 0x1B, 0x1C, 0x1D, 0x1E,
    ];
    let key_material_b = [
        0xA1, 0xA2, 0xA3, 0xA4, 0xA5, 0xA6, 0xA7, 0xA8, 0xA9, 0xAA, 0xAB, 0xAC, 0xAD, 0xAE, 0xAF,
        0xB0, 0xB1, 0xB2, 0xB3, 0xB4, 0xB5, 0xB6, 0xB7, 0xB8, 0xB9, 0xBA, 0xBB, 0xBC, 0xBD, 0xBE,
    ];
    let key_a = crate::session::endpoint_rtp::base64_encode(&key_material_a);
    let key_b = crate::session::endpoint_rtp::base64_encode(&key_material_b);

    let mut ctx_a = SrtpContext::from_sdes_key(&key_a).unwrap();
    let mut ctx_b = SrtpContext::from_sdes_key(&key_b).unwrap();

    // Protect a packet with context A
    let rtp = crate::media::rtp::RtpHeader::build(0, 1, 160, 0x12345678, false, &[0xAA; 160]);
    let srtp = ctx_a.protect(&rtp).unwrap();

    // Try to unprotect with context B — should fail with auth tag mismatch
    let result = ctx_b.unprotect(&srtp);
    assert!(result.is_err(), "unprotect with different key should fail");
    let err_msg = result.err().unwrap().to_string();
    assert!(
        err_msg.contains("auth tag mismatch"),
        "error should mention auth tag mismatch: {err_msg}"
    );
}

#[test]
fn test_srtp_short_key_material() {
    // Base64 of 20 bytes (less than the required 30)
    let short_material = [0x01u8; 20];
    let short_key = crate::session::endpoint_rtp::base64_encode(&short_material);

    let result = SrtpContext::from_sdes_key(&short_key);
    assert!(result.is_err(), "short key material should fail");
    let err_msg = result.err().unwrap().to_string();
    assert!(
        err_msg.contains("too short"),
        "error should mention key material too short: {err_msg}"
    );
}

#[test]
fn test_srtp_exact_30_byte_key() {
    // Exactly 30 bytes of key material (16 master key + 14 master salt)
    let key_material: [u8; 30] = [
        0xDE, 0xAD, 0xBE, 0xEF, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B,
        0x0C, // 16-byte master key
        0x10, 0x20, 0x30, 0x40, 0x50, 0x60, 0x70, 0x80, 0x90, 0xA0, 0xB0, 0xC0, 0xD0,
        0xE0, // 14-byte master salt
    ];
    let key_b64 = crate::session::endpoint_rtp::base64_encode(&key_material);

    // Creating the context should succeed (no panic, no error)
    let ctx = SrtpContext::from_sdes_key(&key_b64);
    assert!(
        ctx.is_ok(),
        "exact 30-byte key material should succeed: {:?}",
        ctx.err()
    );

    let ctx = ctx.unwrap();
    // Derived keys should be non-zero (KDF actually ran)
    assert_ne!(
        ctx.cipher_key, [0u8; 16],
        "cipher key should be derived (non-zero)"
    );
    assert_ne!(
        ctx.cipher_salt, [0u8; 14],
        "cipher salt should be derived (non-zero)"
    );
    assert_ne!(
        ctx.auth_key, [0u8; 20],
        "auth key should be derived (non-zero)"
    );

    // Verify the context can actually protect and unprotect a packet
    let mut protect_ctx = SrtpContext::from_sdes_key(&key_b64).unwrap();
    let mut unprotect_ctx = SrtpContext::from_sdes_key(&key_b64).unwrap();

    let rtp = crate::media::rtp::RtpHeader::build(0, 1, 160, 0xCAFEBABE, false, &[0x55; 160]);
    let srtp = protect_ctx.protect(&rtp).unwrap();
    let decrypted = unprotect_ctx.unprotect(&srtp).unwrap();
    assert_eq!(
        decrypted, rtp,
        "roundtrip with exact 30-byte key should succeed"
    );
}

#[test]
fn test_srtp_key_29_bytes_rejected() {
    // Exactly 1 byte short of the required 30
    let key_material = [0xAA; 29];
    let key_b64 = crate::session::endpoint_rtp::base64_encode(&key_material);
    let result = SrtpContext::from_sdes_key(&key_b64);
    assert!(result.is_err(), "29-byte key material should be rejected");
    let err = result.err().unwrap();
    assert!(err.to_string().contains("too short"));
}

#[test]
fn test_srtp_key_31_bytes_accepted() {
    // 31 bytes is 1 byte MORE than required (30). The extra byte should be
    // silently ignored — the context should initialize and work correctly.
    let key_material = [0xBB; 31];
    let key_b64 = crate::session::endpoint_rtp::base64_encode(&key_material);
    let result = SrtpContext::from_sdes_key(&key_b64);
    assert!(
        result.is_ok(),
        "31-byte key material should be accepted (extra byte ignored)"
    );

    // Verify it can protect/unprotect roundtrip
    let mut protect_ctx = SrtpContext::from_sdes_key(&key_b64).unwrap();
    let mut unprotect_ctx = SrtpContext::from_sdes_key(&key_b64).unwrap();
    let rtp = crate::media::rtp::RtpHeader::build(0, 1, 160, 0x12345678, false, &[0xAA; 160]);
    let srtp = protect_ctx.protect(&rtp).unwrap();
    let decrypted = unprotect_ctx.unprotect(&srtp).unwrap();
    assert_eq!(decrypted, rtp, "roundtrip with 31-byte key should work");
}

#[test]
fn test_srtp_replay_exact_duplicate() {
    let key = make_test_key();
    let mut protect_ctx = SrtpContext::from_sdes_key(&key).unwrap();
    let mut unprotect_ctx = SrtpContext::from_sdes_key(&key).unwrap();

    let rtp = crate::media::rtp::RtpHeader::build(0, 42, 6720, 0xABCD, false, &[0x55; 160]);
    let srtp = protect_ctx.protect(&rtp).unwrap();

    // First unprotect should succeed
    unprotect_ctx.unprotect(&srtp).unwrap();

    // Exact duplicate should be rejected (replay)
    let result = unprotect_ctx.unprotect(&srtp);
    assert!(result.is_err(), "exact duplicate packet should be rejected");
    assert!(
        result.unwrap_err().to_string().contains("replay"),
        "error should mention replay"
    );
}

#[test]
fn test_srtp_replay_outside_window() {
    let key = make_test_key();
    let mut protect_ctx = SrtpContext::from_sdes_key(&key).unwrap();
    let mut unprotect_ctx = SrtpContext::from_sdes_key(&key).unwrap();

    // Protect packets 0..100
    let mut srtp_packets = Vec::new();
    for seq in 0u16..100 {
        let rtp = crate::media::rtp::RtpHeader::build(
            0,
            seq,
            seq as u32 * 160,
            0xABCD,
            false,
            &[seq as u8; 80],
        );
        srtp_packets.push(protect_ctx.protect(&rtp).unwrap());
    }

    // Unprotect packets 0..100 in order
    for srtp in &srtp_packets {
        unprotect_ctx.unprotect(srtp).unwrap();
    }

    // Now try to replay packet 0 — should be outside the 64-packet replay window
    let result = unprotect_ctx.unprotect(&srtp_packets[0]);
    assert!(
        result.is_err(),
        "packet far outside replay window should be rejected"
    );
}

#[test]
fn test_srtp_key_malformed_base64_variants() {
    // Embedded null byte
    let result = SrtpContext::from_sdes_key("AQID\x00BA==");
    assert!(result.is_err(), "base64 with null byte should be rejected");

    // Whitespace in middle
    let result = SrtpContext::from_sdes_key("AQ ID BA==");
    assert!(result.is_err(), "base64 with space should be rejected");

    // Unicode
    let result = SrtpContext::from_sdes_key("AQIDé==");
    assert!(result.is_err(), "base64 with unicode should be rejected");

    // Empty string → too-short key material
    let result = SrtpContext::from_sdes_key("");
    assert!(result.is_err(), "empty string should be rejected");
}

// ── IV computation tests ─────────────────────────────────────────

#[test]
fn test_compute_iv_rfc3711_layout() {
    // Verify RFC 3711 §4.1.1 layout:
    // IV = (k_s * 2^16) XOR (SSRC * 2^64) XOR (i * 2^16)
    //
    // With all-zero salt, SSRC=0, index=0, IV should be all zeros.
    let salt = [0u8; 14];
    let iv = compute_iv(&salt, 0, 0);
    assert_eq!(iv, [0u8; 16], "all-zero inputs should produce all-zero IV");

    // With non-zero salt, zero SSRC and index:
    // IV should equal salt at bytes 0-13, bytes 14-15 = 0
    let salt = [
        0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B, 0x0C, 0x0D, 0x0E,
    ];
    let iv = compute_iv(&salt, 0, 0);
    assert_eq!(
        &iv[0..14],
        &salt,
        "salt-only IV should have salt at bytes 0-13"
    );
    assert_eq!(iv[14], 0, "byte 14 must be zero");
    assert_eq!(iv[15], 0, "byte 15 must be zero");
}

#[test]
fn test_compute_iv_ssrc_position() {
    // SSRC * 2^64 places SSRC at bytes 4-7
    let salt = [0u8; 14];
    let iv = compute_iv(&salt, 0xDEADBEEF, 0);
    assert_eq!(iv[4], 0xDE);
    assert_eq!(iv[5], 0xAD);
    assert_eq!(iv[6], 0xBE);
    assert_eq!(iv[7], 0xEF);
    // Bytes 0-3 and 8-15 should be zero (no salt, no index)
    assert_eq!(&iv[0..4], &[0, 0, 0, 0]);
    assert_eq!(&iv[8..16], &[0, 0, 0, 0, 0, 0, 0, 0]);
}

#[test]
fn test_compute_iv_index_shifted_by_16() {
    // i * 2^16 places the 48-bit index at bytes 8-13 (shifted left by 16)
    let salt = [0u8; 14];
    // Index = ROC=1, SEQ=0 → raw 48-bit index = (1 << 16) | 0 = 0x10000
    let index: u64 = 0x10000;
    let iv = compute_iv(&salt, 0, index);
    // index << 16 = 0x1_0000_0000 → bytes: [0,0,0,1,0,0,0,0]
    // placed at iv[8..16]
    assert_eq!(
        &iv[8..16],
        &[0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00]
    );
    // Bytes 14-15 must be zero
    assert_eq!(iv[14], 0);
    assert_eq!(iv[15], 0);
}

#[test]
fn test_compute_iv_full_combination() {
    // Combine salt, SSRC, and index and verify XOR
    let salt: [u8; 14] = [
        0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
    ];
    let ssrc: u32 = 0x12345678;
    // 48-bit index: ROC=0, SEQ=1 → index = 1
    let index: u64 = 1;

    let iv = compute_iv(&salt, ssrc, index);

    // Expected (manually computed):
    // index=1, shifted: 1u64 << 16 = 0x0000_0000_0001_0000
    //   to_be_bytes() = [0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00]
    //   placed at iv[8..16]
    //
    // Before salt XOR:
    //   iv = [0,0,0,0, 0x12,0x34,0x56,0x78, 0,0,0,0, 0,0x01,0,0]
    // After XOR with salt (all 0xFF at bytes 0-13):
    //   iv[0..4]   = 0xFF
    //   iv[4..8]   = SSRC XOR salt = [0xED, 0xCB, 0xA9, 0x87]
    //   iv[8..14]  = [0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFE]
    //   iv[14..16] = [0, 0] (no salt contribution)
    assert_eq!(iv[0], 0xFF);
    assert_eq!(iv[1], 0xFF);
    assert_eq!(iv[2], 0xFF);
    assert_eq!(iv[3], 0xFF);
    assert_eq!(iv[4], 0xFF ^ 0x12);
    assert_eq!(iv[5], 0xFF ^ 0x34);
    assert_eq!(iv[6], 0xFF ^ 0x56);
    assert_eq!(iv[7], 0xFF ^ 0x78);
    // shifted index [0,0,0,0,0,0x01,0,0] at iv[8..16], XOR with salt[8..14]
    assert_eq!(iv[8], 0xFF); // 0x00 ^ 0xFF
    assert_eq!(iv[9], 0xFF); // 0x00 ^ 0xFF
    assert_eq!(iv[10], 0xFF); // 0x00 ^ 0xFF
    assert_eq!(iv[11], 0xFF); // 0x00 ^ 0xFF
    assert_eq!(iv[12], 0xFF); // 0x00 ^ 0xFF
    assert_eq!(iv[13], 0xFE); // 0x01 ^ 0xFF
    assert_eq!(iv[14], 0x00); // no salt at byte 14
    assert_eq!(iv[15], 0x00);
}

#[test]
fn test_compute_srtcp_iv_layout() {
    // Same layout rules as SRTP IV but with 32-bit SRTCP index
    let salt = [0u8; 14];

    // All zeros
    let iv = compute_srtcp_iv(&salt, 0, 0);
    assert_eq!(iv, [0u8; 16]);

    // Salt only
    let salt = [
        0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B, 0x0C, 0x0D, 0x0E,
    ];
    let iv = compute_srtcp_iv(&salt, 0, 0);
    assert_eq!(&iv[0..14], &salt);
    assert_eq!(iv[14], 0);
    assert_eq!(iv[15], 0);

    // SSRC only
    let salt = [0u8; 14];
    let iv = compute_srtcp_iv(&salt, 0xAABBCCDD, 0);
    assert_eq!(&iv[4..8], &[0xAA, 0xBB, 0xCC, 0xDD]);

    // Index shifted by 16: SRTCP index 1 → (1u64 << 16) = 0x10000
    let iv = compute_srtcp_iv(&salt, 0, 1);
    // [0,0,0,0,0,0x01,0x00,0x00] at iv[8..16]
    assert_eq!(&iv[8..14], &[0x00, 0x00, 0x00, 0x00, 0x00, 0x01]);
    assert_eq!(iv[14], 0);
    assert_eq!(iv[15], 0);
}

#[test]
fn test_srtcp_protect_unprotect_roundtrip() {
    let key = make_test_key();
    let mut protect_ctx = SrtcpContext::from_sdes_key(&key).unwrap();
    let mut unprotect_ctx = SrtcpContext::from_sdes_key(&key).unwrap();

    // Build a minimal RTCP Sender Report by hand:
    // 4-byte header + 4-byte SSRC + 20-byte SR body = 28 bytes
    // V=2, P=0, RC=0 → first byte = 0x80
    // PT=200 (SR)
    // Length=6 (28 bytes / 4 - 1 = 6)
    let mut rtcp = [0u8; 28];
    rtcp[0] = 0x80; // V=2, P=0, RC=0
    rtcp[1] = 200; // PT = Sender Report
    rtcp[2] = 0x00; // length high byte
    rtcp[3] = 0x06; // length low byte (6 32-bit words after header)
    // SSRC
    rtcp[4] = 0x12;
    rtcp[5] = 0x34;
    rtcp[6] = 0x56;
    rtcp[7] = 0x78;
    // Fill SR body (NTP timestamp, RTP timestamp, packet/octet counts)
    // with recognizable non-zero data so we can verify roundtrip
    for i in 8..28 {
        rtcp[i] = i as u8;
    }
    let original = rtcp.to_vec();

    // Protect (encrypt + authenticate)
    let srtcp = protect_ctx.protect_rtcp(&original).unwrap();
    // SRTCP adds 4-byte E+index field and 10-byte auth tag
    assert_eq!(srtcp.len(), original.len() + 4 + SRTP_AUTH_TAG_LEN);

    // Unprotect (verify + decrypt) with a separate context from the same key
    let decrypted = unprotect_ctx.unprotect_rtcp(&srtcp).unwrap();
    assert_eq!(
        decrypted, original,
        "SRTCP roundtrip with hand-built SR should recover original packet"
    );
}

#[test]
fn test_srtp_new_ssrc_accepted_without_reset() {
    // Per-SSRC state: after one source runs to a high sequence, a DIFFERENT
    // SSRC starting at a low sequence is accepted with no reset call — the
    // bug this fixes (the shared window previously rejected it as "too old").
    let key = make_test_key();
    let mut tx = SrtpContext::from_sdes_key(&key).unwrap();
    let mut rx = SrtpContext::from_sdes_key(&key).unwrap();

    for seq in 1..=300u16 {
        let rtp = crate::media::rtp::RtpHeader::build(
            0,
            seq,
            seq as u32 * 160,
            0xAAAA_0001,
            false,
            &[seq as u8; 80],
        );
        let srtp = tx.protect(&rtp).unwrap();
        rx.unprotect(&srtp).unwrap();
    }

    let rtp = crate::media::rtp::RtpHeader::build(0, 1, 160, 0xBBBB_0002, false, &[0x99; 80]);
    let srtp = tx.protect(&rtp).unwrap();
    let decrypted = rx
        .unprotect(&srtp)
        .expect("new SSRC at a low sequence must be accepted without a reset");
    assert_eq!(decrypted, rtp);
}

#[test]
fn test_srtp_same_seq_distinct_ssrc_independent_replay() {
    // The same sequence number on two SSRCs is not a cross-SSRC replay; each
    // SSRC has an independent window. Replaying within one SSRC still fails.
    let key = make_test_key();
    let mut tx = SrtpContext::from_sdes_key(&key).unwrap();
    let mut rx = SrtpContext::from_sdes_key(&key).unwrap();

    let a = crate::media::rtp::RtpHeader::build(0, 7, 1120, 0xA000_0000, false, &[1u8; 80]);
    let b = crate::media::rtp::RtpHeader::build(0, 7, 1120, 0xB000_0000, false, &[2u8; 80]);
    let sa = tx.protect(&a).unwrap();
    let sb = tx.protect(&b).unwrap();

    assert_eq!(rx.unprotect(&sa).unwrap(), a);
    assert_eq!(
        rx.unprotect(&sb).unwrap(),
        b,
        "same seq, other SSRC is not a replay"
    );
    assert!(
        rx.unprotect(&sa)
            .unwrap_err()
            .to_string()
            .contains("replay"),
        "duplicate on the same SSRC must still be a replay"
    );
}

#[test]
fn test_srtp_independent_roc_per_ssrc() {
    // Two SSRCs cross the 16-bit sequence wrap independently; their ROCs must
    // advance separately. Interleaved so neither corrupts the other's ROC.
    let key = make_test_key();
    let mut tx = SrtpContext::from_sdes_key(&key).unwrap();
    let mut rx = SrtpContext::from_sdes_key(&key).unwrap();

    let ssrc_a = 0xA0A0_A0A0; // starts near the top → wraps mid-loop
    let ssrc_b = 0xB0B0_B0B0; // starts low → no wrap
    for i in 0..100u32 {
        let seq_a = 65500u16.wrapping_add(i as u16);
        let seq_b = 1000u16.wrapping_add(i as u16);
        let ra = crate::media::rtp::RtpHeader::build(
            0,
            seq_a,
            i.wrapping_mul(160),
            ssrc_a,
            false,
            &[i as u8; 80],
        );
        let rb = crate::media::rtp::RtpHeader::build(
            0,
            seq_b,
            i.wrapping_mul(160),
            ssrc_b,
            false,
            &[(i as u8) ^ 0xFF; 80],
        );
        let pa = tx.protect(&ra).unwrap();
        let pb = tx.protect(&rb).unwrap();
        assert_eq!(rx.unprotect(&pa).unwrap(), ra, "A failed at i={i}");
        assert_eq!(rx.unprotect(&pb).unwrap(), rb, "B failed at i={i}");
    }
}

#[test]
fn test_srtp_recv_ssrc_cap_rejects_excess() {
    // The receive context bounds distinct SSRCs to MAX_RECV_SSRCS; one more
    // authenticated SSRC is rejected rather than evicting a live stream.
    let key = make_test_key();
    let mut tx = SrtpContext::from_sdes_key(&key).unwrap();
    let mut rx = SrtpContext::from_sdes_key(&key).unwrap();

    for i in 0..MAX_RECV_SSRCS as u32 {
        let rtp =
            crate::media::rtp::RtpHeader::build(0, 0, 0, 0x1000_0000 + i, false, &[i as u8; 80]);
        let srtp = tx.protect(&rtp).unwrap();
        rx.unprotect(&srtp).unwrap();
    }

    let rtp = crate::media::rtp::RtpHeader::build(0, 0, 0, 0x2000_0000, false, &[0u8; 80]);
    let srtp = tx.protect(&rtp).unwrap();
    let err = rx.unprotect(&srtp).unwrap_err().to_string();
    assert!(err.contains("too many"), "expected cap error, got: {err}");

    // An already-tracked SSRC keeps working (not locked out by the cap).
    let rtp = crate::media::rtp::RtpHeader::build(0, 1, 160, 0x1000_0000, false, &[7u8; 80]);
    let srtp = tx.protect(&rtp).unwrap();
    rx.unprotect(&srtp)
        .expect("an already-tracked SSRC must keep working at the cap");
}

#[test]
fn test_srtcp_new_ssrc_accepted_without_reset() {
    // SRTCP analogue: after one source runs its index past the window, a
    // different source at index 0 is accepted without a reset.
    let key = make_test_key();
    let mut rx = SrtcpContext::from_sdes_key(&key).unwrap();
    let mut stats = crate::media::rtcp::RtcpStats::new();

    let mut px = SrtcpContext::from_sdes_key(&key).unwrap();
    let rtcp_x = crate::media::rtcp::build_sr_rr(0x1111_1111, 0x2222_2222, &mut stats, 0, 8000);
    for _ in 0..200 {
        let s = px.protect_rtcp(&rtcp_x).unwrap();
        rx.unprotect_rtcp(&s).unwrap();
    }

    let mut py = SrtcpContext::from_sdes_key(&key).unwrap();
    let rtcp_y = crate::media::rtcp::build_sr_rr(0x3333_3333, 0x4444_4444, &mut stats, 0, 8000);
    let sy = py.protect_rtcp(&rtcp_y).unwrap();
    rx.unprotect_rtcp(&sy)
        .expect("new SRTCP SSRC at index 0 must be accepted without a reset");
}

#[test]
fn test_srtcp_distinct_ssrc_independent_replay() {
    // Same SRTCP index on two SSRCs is independent; replay within one fails.
    let key = make_test_key();
    let mut rx = SrtcpContext::from_sdes_key(&key).unwrap();
    let mut pa = SrtcpContext::from_sdes_key(&key).unwrap();
    let mut pb = SrtcpContext::from_sdes_key(&key).unwrap();
    let mut stats = crate::media::rtcp::RtcpStats::new();
    let rtcp_a = crate::media::rtcp::build_sr_rr(0xAAAA_0000, 0x1, &mut stats, 0, 8000);
    let rtcp_b = crate::media::rtcp::build_sr_rr(0xBBBB_0000, 0x1, &mut stats, 0, 8000);
    let sa = pa.protect_rtcp(&rtcp_a).unwrap();
    let sb = pb.protect_rtcp(&rtcp_b).unwrap();

    rx.unprotect_rtcp(&sa).unwrap();
    rx.unprotect_rtcp(&sb)
        .expect("same index on a different SSRC is independent, not a replay");
    assert!(
        rx.unprotect_rtcp(&sa)
            .unwrap_err()
            .to_string()
            .contains("replay"),
        "SRTCP duplicate on the same SSRC must still be rejected"
    );
}
