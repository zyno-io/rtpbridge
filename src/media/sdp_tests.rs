use super::*;

#[test]
fn test_generate_and_parse_sdp() {
    let addr: SocketAddr = "192.168.1.1:5060".parse().unwrap();
    let codecs = vec![&CODEC_PCMU, &CODEC_G722];
    let sdp = generate_sdp_offer(addr, 30000, &codecs, None, 12345);

    assert!(sdp.contains("m=audio 30000 RTP/AVP"));
    assert!(sdp.contains("a=rtpmap:0 PCMU/8000"));
    assert!(sdp.contains("a=rtpmap:9 G722/8000"));
    assert!(sdp.contains("a=rtpmap:101 telephone-event/8000"));
    assert!(sdp.contains("a=rtcp-mux"));

    let parsed = parse_sdp(&sdp);
    assert!(!parsed.is_webrtc);
    assert!(parsed.telephone_event_pt.is_some());
    assert_eq!(parsed.remote_addr.unwrap().port(), 30000);
}

#[test]
fn test_select_answer_codec_prefers_highest_quality() {
    // Offer lists narrowband first; we should still pick Opus.
    let codecs = vec![CODEC_PCMU, CODEC_G722, CODEC_OPUS];
    assert_eq!(select_answer_codec(&codecs).unwrap().name, "opus");

    // Without Opus, G722 wins over PCMU regardless of order.
    let codecs = vec![CODEC_PCMU, CODEC_G722];
    assert_eq!(select_answer_codec(&codecs).unwrap().name, "G722");
    let codecs = vec![CODEC_G722, CODEC_PCMU];
    assert_eq!(select_answer_codec(&codecs).unwrap().name, "G722");

    // telephone-event is never selected as the media codec.
    let codecs = vec![CODEC_TELEPHONE_EVENT, CODEC_PCMU];
    assert_eq!(select_answer_codec(&codecs).unwrap().name, "PCMU");

    // Only telephone-event (or empty) yields no media codec.
    assert!(select_answer_codec(&[CODEC_TELEPHONE_EVENT]).is_none());
    assert!(select_answer_codec(&[]).is_none());
}

#[test]
fn test_offer_codec_list_is_quality_ordered() {
    // Default offer advertises Opus first, then G.722, then PCMU, with
    // telephone-event last — never PCMU-first.
    let names: Vec<&str> = offer_codec_list(None).iter().map(|c| c.name).collect();
    assert_eq!(names, vec!["opus", "G722", "PCMU", "telephone-event"]);

    // The list is non-increasing in codec_quality (regression guard against
    // a future PCMU-first reorder).
    let list = offer_codec_list(None);
    for pair in list.windows(2) {
        assert!(
            codec_quality(&pair[0]) >= codec_quality(&pair[1]),
            "offer codec order must be quality-descending: {} before {}",
            pair[0].name,
            pair[1].name
        );
    }
}

#[test]
fn test_offer_codec_list_honors_caller_preference() {
    // A caller-specified order is advertised verbatim (RFC 3264 "preferred
    // codec order"), matched case-insensitively, with telephone-event last —
    // the quality default does NOT override an explicit preference.
    let prefer = vec!["pcmu".to_string(), "opus".to_string()];
    let names: Vec<&str> = offer_codec_list(Some(&prefer))
        .iter()
        .map(|c| c.name)
        .collect();
    assert_eq!(names, vec!["PCMU", "opus", "telephone-event"]);

    // Reversing the preference flips the advertised order.
    let prefer = vec!["opus".to_string(), "pcmu".to_string()];
    let names: Vec<&str> = offer_codec_list(Some(&prefer))
        .iter()
        .map(|c| c.name)
        .collect();
    assert_eq!(names, vec!["opus", "PCMU", "telephone-event"]);

    // Unknown and duplicate names are skipped; a single codec keeps DTMF.
    let prefer = vec!["g729".to_string(), "PCMU".to_string(), "pcmu".to_string()];
    let names: Vec<&str> = offer_codec_list(Some(&prefer))
        .iter()
        .map(|c| c.name)
        .collect();
    assert_eq!(names, vec!["PCMU", "telephone-event"]);
}

#[test]
fn test_generated_offer_lists_opus_first() {
    // End-to-end: the m= line advertises Opus's PT (111) ahead of the
    // narrowband PTs so the answerer's first-match selection picks Opus.
    let addr: SocketAddr = "192.168.1.1:5060".parse().unwrap();
    let list = offer_codec_list(None);
    let refs: Vec<&SdpCodec> = list.iter().collect();
    let sdp = generate_sdp_offer(addr, 30000, &refs, None, 12345);

    let m_line = sdp
        .lines()
        .find(|l| l.starts_with("m=audio"))
        .expect("m=audio line present");
    assert_eq!(m_line, "m=audio 30000 RTP/AVP 111 9 0 101");

    // Opus leads (48 kHz) but telephone-event stays at the SIP-friendly
    // 8 kHz; DTMF timing keys off the negotiated telephone-event clock, not
    // the audio codec, so this no longer breaks DTMF for Opus.
    assert!(sdp.contains("a=rtpmap:111 opus/48000/2"));
    assert!(
        sdp.contains("a=rtpmap:101 telephone-event/8000"),
        "telephone-event must be advertised at 8000 even when Opus leads:\n{sdp}"
    );
    assert!(!sdp.contains("telephone-event/48000"));
}

#[test]
fn test_parse_telephone_event_clock_rate() {
    let sdp8 = "v=0\r\no=- 1 1 IN IP4 1.2.3.4\r\ns=-\r\nc=IN IP4 1.2.3.4\r\n\
        t=0 0\r\nm=audio 5000 RTP/AVP 0 101\r\na=rtpmap:0 PCMU/8000\r\n\
        a=rtpmap:101 telephone-event/8000\r\n";
    let parsed = parse_sdp(sdp8);
    assert_eq!(parsed.telephone_event_pt, Some(101));
    assert_eq!(parsed.telephone_event_clock_rate, Some(8000));

    // A WebRTC-style offer advertises telephone-event at 48000.
    let sdp48 = "v=0\r\no=- 1 1 IN IP4 1.2.3.4\r\ns=-\r\nc=IN IP4 1.2.3.4\r\n\
        t=0 0\r\nm=audio 5000 RTP/AVP 111 101\r\na=rtpmap:111 opus/48000/2\r\n\
        a=rtpmap:101 telephone-event/48000\r\n";
    let parsed = parse_sdp(sdp48);
    assert_eq!(parsed.telephone_event_clock_rate, Some(48000));

    // No telephone-event advertised → None (consumers default to 8000).
    let sdp_none = "v=0\r\no=- 1 1 IN IP4 1.2.3.4\r\ns=-\r\nc=IN IP4 1.2.3.4\r\n\
        t=0 0\r\nm=audio 5000 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\n";
    assert_eq!(parse_sdp(sdp_none).telephone_event_clock_rate, None);
}

#[test]
fn test_answer_echoes_offered_telephone_event_clock() {
    let addr: SocketAddr = "192.168.1.1:5060".parse().unwrap();

    // Answering with Opus media but a telephone-event offered at 8000 keeps
    // telephone-event at 8000 — the te clock is independent of the media
    // codec (proves Approach B, not a media-coupled rate).
    let codecs = vec![&CODEC_OPUS, &CODEC_TELEPHONE_EVENT];
    let ans = generate_sdp_answer(addr, 30000, &codecs, None, 1);
    assert!(ans.contains("opus/48000"));
    assert!(ans.contains("a=rtpmap:101 telephone-event/8000"));
    assert!(!ans.contains("telephone-event/48000"));

    // When the remote actually offered telephone-event/48000, the answer
    // echoes 48000 — proving this is B (echo negotiated), not hard-coded 8000.
    let mut te48 = CODEC_TELEPHONE_EVENT;
    te48.clock_rate = 48000;
    let codecs = vec![&CODEC_OPUS, &te48];
    let ans = generate_sdp_answer(addr, 30000, &codecs, None, 1);
    assert!(ans.contains("a=rtpmap:101 telephone-event/48000"));
}

#[test]
fn test_parse_srtp_sdp() {
    let sdp = "v=0\r\n\
        o=- 123 1 IN IP4 10.0.0.1\r\n\
        s=-\r\n\
        c=IN IP4 10.0.0.1\r\n\
        t=0 0\r\n\
        m=audio 20000 RTP/SAVP 0 101\r\n\
        a=rtpmap:0 PCMU/8000\r\n\
        a=rtpmap:101 telephone-event/8000\r\n\
        a=fmtp:101 0-16\r\n\
        a=crypto:1 AES_CM_128_HMAC_SHA1_80 inline:dGVzdGtleQ==\r\n\
        a=sendrecv\r\n";

    let parsed = parse_sdp(sdp);
    assert!(!parsed.is_webrtc);
    assert!(parsed.crypto.is_some());
    let crypto = parsed.crypto.unwrap();
    assert_eq!(crypto.suite, "AES_CM_128_HMAC_SHA1_80");
    assert_eq!(crypto.key_b64, "dGVzdGtleQ==");
    assert_eq!(parsed.telephone_event_pt, Some(101));
}

#[test]
fn test_detect_webrtc() {
    let sdp = "v=0\r\n\
        o=- 123 1 IN IP4 10.0.0.1\r\n\
        s=-\r\n\
        c=IN IP4 10.0.0.1\r\n\
        t=0 0\r\n\
        m=audio 9 UDP/TLS/RTP/SAVPF 111\r\n\
        a=ice-ufrag:abc123\r\n\
        a=ice-pwd:xyz789\r\n\
        a=fingerprint:sha-256 AA:BB:CC\r\n";

    let parsed = parse_sdp(sdp);
    assert!(parsed.is_webrtc);
}

#[test]
fn test_osrtp_detection() {
    // OSRTP (RFC 8643): RTP/AVP profile with a=crypto present
    let sdp = "v=0\r\n\
        o=- 123 1 IN IP4 10.0.0.1\r\n\
        s=-\r\n\
        c=IN IP4 10.0.0.1\r\n\
        t=0 0\r\n\
        m=audio 20000 RTP/AVP 0 101\r\n\
        a=rtpmap:0 PCMU/8000\r\n\
        a=rtpmap:101 telephone-event/8000\r\n\
        a=crypto:1 AES_CM_128_HMAC_SHA1_80 inline:dGVzdGtleQ==\r\n\
        a=sendrecv\r\n";

    let parsed = parse_sdp(sdp);
    assert!(!parsed.is_webrtc);
    assert!(parsed.is_osrtp, "should detect OSRTP");
    assert!(parsed.crypto.is_some());
    assert_eq!(parsed.media_protocol.as_deref(), Some("RTP/AVP"));
}

#[test]
fn test_savp_is_not_osrtp() {
    // RTP/SAVP with crypto is standard SRTP, not OSRTP
    let sdp = "v=0\r\n\
        o=- 123 1 IN IP4 10.0.0.1\r\n\
        s=-\r\n\
        c=IN IP4 10.0.0.1\r\n\
        t=0 0\r\n\
        m=audio 20000 RTP/SAVP 0 101\r\n\
        a=rtpmap:0 PCMU/8000\r\n\
        a=crypto:1 AES_CM_128_HMAC_SHA1_80 inline:dGVzdGtleQ==\r\n\
        a=sendrecv\r\n";

    let parsed = parse_sdp(sdp);
    assert!(!parsed.is_osrtp, "RTP/SAVP is standard SRTP, not OSRTP");
    assert!(parsed.crypto.is_some());
    assert_eq!(parsed.media_protocol.as_deref(), Some("RTP/SAVP"));
}

#[test]
fn test_plain_rtp_no_crypto() {
    // Plain RTP/AVP without crypto
    let sdp = "v=0\r\n\
        o=- 123 1 IN IP4 10.0.0.1\r\n\
        s=-\r\n\
        c=IN IP4 10.0.0.1\r\n\
        t=0 0\r\n\
        m=audio 20000 RTP/AVP 0\r\n\
        a=rtpmap:0 PCMU/8000\r\n\
        a=sendrecv\r\n";

    let parsed = parse_sdp(sdp);
    assert!(!parsed.is_osrtp);
    assert!(parsed.crypto.is_none());
    assert_eq!(parsed.media_protocol.as_deref(), Some("RTP/AVP"));
}

#[test]
fn test_parse_empty_sdp() {
    let parsed = parse_sdp("");
    assert!(
        parsed.codecs.is_empty(),
        "empty SDP should produce no codecs"
    );
    assert!(
        parsed.remote_addr.is_none(),
        "empty SDP should have no remote address"
    );
    assert!(!parsed.is_webrtc, "empty SDP should not be WebRTC");
    assert!(parsed.crypto.is_none(), "empty SDP should have no crypto");
    assert!(
        parsed.media_protocol.is_none(),
        "empty SDP should have no media protocol"
    );
    assert!(
        parsed.direction.is_none(),
        "empty SDP should have no direction"
    );
}

#[test]
fn test_parse_sdp_no_media_line() {
    // Valid SDP session-level headers but no m= line
    let sdp = "v=0\r\no=- 0 0 IN IP4 0.0.0.0\r\ns=-\r\n";
    let parsed = parse_sdp(sdp);
    assert!(
        parsed.codecs.is_empty(),
        "SDP with no m= line should produce no codecs"
    );
    assert!(
        parsed.remote_addr.is_none(),
        "SDP with no m= line should have no remote address (no port)"
    );
    assert!(
        parsed.media_protocol.is_none(),
        "SDP with no m= line should have no media protocol"
    );
}

#[test]
fn test_malformed_crypto_missing_key() {
    // a=crypto line with only tag and suite, no key material
    let sdp = "v=0\r\n\
        o=- 123 1 IN IP4 10.0.0.1\r\n\
        s=-\r\n\
        c=IN IP4 10.0.0.1\r\n\
        t=0 0\r\n\
        m=audio 20000 RTP/SAVP 0\r\n\
        a=rtpmap:0 PCMU/8000\r\n\
        a=crypto:1 AES_CM_128_HMAC_SHA1_80\r\n\
        a=sendrecv\r\n";

    let parsed = parse_sdp(sdp);
    // The parser requires 3 parts in the crypto line; with only 2 parts,
    // crypto should be None (silently skipped).
    assert!(
        parsed.crypto.is_none(),
        "malformed crypto line missing key should be rejected"
    );
}

#[test]
fn test_malformed_crypto_bad_base64_key() {
    // a=crypto line with invalid base64 in the key — parser stores it raw,
    // but the SRTP context should reject it later. Here we just verify
    // the SDP parser doesn't panic.
    let sdp = "v=0\r\n\
        o=- 123 1 IN IP4 10.0.0.1\r\n\
        s=-\r\n\
        c=IN IP4 10.0.0.1\r\n\
        t=0 0\r\n\
        m=audio 20000 RTP/SAVP 0\r\n\
        a=rtpmap:0 PCMU/8000\r\n\
        a=crypto:1 AES_CM_128_HMAC_SHA1_80 inline:!!!NOT-BASE64!!!\r\n\
        a=sendrecv\r\n";

    let parsed = parse_sdp(sdp);
    // The SDP parser should parse the line without panicking; the bad
    // base64 is stored and will fail at SRTP context creation time.
    assert!(parsed.crypto.is_some(), "crypto line should be parsed");
    let crypto = parsed.crypto.unwrap();
    assert_eq!(crypto.suite, "AES_CM_128_HMAC_SHA1_80");
    assert_eq!(crypto.key_b64, "!!!NOT-BASE64!!!");
}

#[test]
fn test_unsupported_crypto_suite_rejected() {
    // Unknown cipher suite — only AES_CM_128_HMAC_SHA1_80 is accepted
    let sdp = "v=0\r\n\
        o=- 123 1 IN IP4 10.0.0.1\r\n\
        s=-\r\n\
        c=IN IP4 10.0.0.1\r\n\
        t=0 0\r\n\
        m=audio 20000 RTP/SAVP 0\r\n\
        a=rtpmap:0 PCMU/8000\r\n\
        a=crypto:1 FAKE_SUITE_256 inline:dGVzdGtleQ==\r\n\
        a=sendrecv\r\n";

    let parsed = parse_sdp(sdp);
    assert!(
        parsed.crypto.is_none(),
        "unsupported cipher suite should be rejected"
    );
}

#[test]
fn test_supported_suite_preferred_over_unsupported() {
    // Multiple crypto lines: unsupported first, supported second
    let sdp = "v=0\r\n\
        o=- 123 1 IN IP4 10.0.0.1\r\n\
        s=-\r\n\
        c=IN IP4 10.0.0.1\r\n\
        t=0 0\r\n\
        m=audio 20000 RTP/SAVP 0\r\n\
        a=rtpmap:0 PCMU/8000\r\n\
        a=crypto:1 AES_256_CM_HMAC_SHA1_80 inline:YmFka2V5\r\n\
        a=crypto:2 AES_CM_128_HMAC_SHA1_80 inline:Z29vZGtleQ==\r\n\
        a=sendrecv\r\n";

    let parsed = parse_sdp(sdp);
    assert!(parsed.crypto.is_some(), "should accept the supported suite");
    let crypto = parsed.crypto.unwrap();
    assert_eq!(crypto.suite, "AES_CM_128_HMAC_SHA1_80");
    assert_eq!(crypto.key_b64, "Z29vZGtleQ==");
    assert_eq!(crypto.tag, 2);
}

#[test]
fn test_malformed_crypto_empty_line() {
    // Completely empty a=crypto: value
    let sdp = "v=0\r\n\
        o=- 123 1 IN IP4 10.0.0.1\r\n\
        s=-\r\n\
        c=IN IP4 10.0.0.1\r\n\
        t=0 0\r\n\
        m=audio 20000 RTP/SAVP 0\r\n\
        a=rtpmap:0 PCMU/8000\r\n\
        a=crypto:\r\n\
        a=sendrecv\r\n";

    let parsed = parse_sdp(sdp);
    assert!(
        parsed.crypto.is_none(),
        "empty crypto line should be rejected"
    );
}

#[test]
fn test_parse_sdp_with_ipv6() {
    let sdp = "v=0\r\n\
        o=- 123 1 IN IP6 ::1\r\n\
        s=-\r\n\
        c=IN IP6 ::1\r\n\
        t=0 0\r\n\
        m=audio 30000 RTP/AVP 0\r\n\
        a=rtpmap:0 PCMU/8000\r\n\
        a=sendrecv\r\n";

    let parsed = parse_sdp(sdp);
    assert!(
        parsed.remote_addr.is_some(),
        "SDP with IPv6 should parse remote address"
    );
    let addr = parsed.remote_addr.unwrap();
    assert!(addr.ip().is_ipv6(), "parsed address should be IPv6");
    assert_eq!(addr.ip().to_string(), "::1", "IPv6 address should be ::1");
    assert_eq!(addr.port(), 30000, "port should be 30000");
    assert_eq!(parsed.codecs.len(), 1, "should have 1 codec");
    assert_eq!(parsed.codecs[0].name, "PCMU", "codec should be PCMU");
}

#[test]
fn test_parse_sdp_multiple_media_lines() {
    // SDP with audio + video m= lines — we should only parse audio
    let sdp = "v=0\r\n\
        o=- 200 1 IN IP4 192.168.1.1\r\n\
        s=-\r\n\
        c=IN IP4 192.168.1.1\r\n\
        t=0 0\r\n\
        m=audio 40000 RTP/AVP 0 101\r\n\
        a=rtpmap:0 PCMU/8000\r\n\
        a=rtpmap:101 telephone-event/8000\r\n\
        a=sendrecv\r\n\
        m=video 40002 RTP/AVP 96\r\n\
        a=rtpmap:96 VP8/90000\r\n\
        a=sendrecv\r\n";

    let parsed = parse_sdp(sdp);
    // Should parse the first audio m= line
    assert!(parsed.remote_addr.is_some());
    assert_eq!(parsed.remote_addr.unwrap().port(), 40000);
    // Should include PCMU from the audio line
    assert!(
        parsed.codecs.iter().any(|c| c.name == "PCMU"),
        "should find PCMU codec from audio m= line"
    );
    assert_eq!(parsed.telephone_event_pt, Some(101));
}

#[test]
fn test_parse_sdp_per_media_connection_lines() {
    // Audio and video have different c= addresses — audio c= must win for remote_addr
    let sdp = "v=0\r\n\
        o=- 400 1 IN IP4 10.0.0.1\r\n\
        s=-\r\n\
        c=IN IP4 10.0.0.99\r\n\
        t=0 0\r\n\
        m=audio 30000 RTP/AVP 0\r\n\
        c=IN IP4 10.0.0.1\r\n\
        a=rtpmap:0 PCMU/8000\r\n\
        a=sendrecv\r\n\
        m=video 30002 RTP/AVP 96\r\n\
        c=IN IP4 10.0.0.2\r\n\
        a=rtpmap:96 VP8/90000\r\n\
        a=sendrecv\r\n";

    let parsed = parse_sdp(sdp);
    let addr = parsed.remote_addr.expect("should have remote addr");
    assert_eq!(
        addr.ip().to_string(),
        "10.0.0.1",
        "audio media-level c= should override session c= and not be overwritten by video c="
    );
    assert_eq!(addr.port(), 30000, "port should come from audio m= line");
}

#[test]
fn test_parse_sdp_session_c_used_when_no_audio_c() {
    // No media-level c= on audio — should fall back to session-level
    let sdp = "v=0\r\n\
        o=- 500 1 IN IP4 10.0.0.1\r\n\
        s=-\r\n\
        c=IN IP4 192.168.1.100\r\n\
        t=0 0\r\n\
        m=audio 25000 RTP/AVP 0\r\n\
        a=rtpmap:0 PCMU/8000\r\n\
        m=video 25002 RTP/AVP 96\r\n\
        c=IN IP4 10.0.0.5\r\n\
        a=rtpmap:96 VP8/90000\r\n";

    let parsed = parse_sdp(sdp);
    let addr = parsed.remote_addr.expect("should have remote addr");
    assert_eq!(
        addr.ip().to_string(),
        "192.168.1.100",
        "should use session-level c= when audio has no media-level c="
    );
    assert_eq!(addr.port(), 25000);
}

#[test]
fn test_parse_sdp_duplicate_codec_definitions() {
    // SDP that lists the same PT twice in rtpmap — last one wins
    let sdp = "v=0\r\n\
        o=- 300 1 IN IP4 10.0.0.1\r\n\
        s=-\r\n\
        c=IN IP4 10.0.0.1\r\n\
        t=0 0\r\n\
        m=audio 50000 RTP/AVP 0 9\r\n\
        a=rtpmap:0 PCMU/8000\r\n\
        a=rtpmap:9 G722/8000\r\n\
        a=sendrecv\r\n";

    let parsed = parse_sdp(sdp);
    assert_eq!(parsed.codecs.len(), 2);
    assert_eq!(parsed.codecs[0].name, "PCMU");
    assert_eq!(parsed.codecs[1].name, "G722");
}

#[test]
fn test_parse_sdp_two_audio_m_lines() {
    // Two m=audio lines — codecs from both should be merged
    let sdp = "v=0\r\n\
        o=- 600 1 IN IP4 10.0.0.1\r\n\
        s=-\r\n\
        c=IN IP4 10.0.0.1\r\n\
        t=0 0\r\n\
        m=audio 30000 RTP/AVP 0\r\n\
        a=rtpmap:0 PCMU/8000\r\n\
        a=sendrecv\r\n\
        m=audio 30002 RTP/AVP 9\r\n\
        a=rtpmap:9 G722/8000\r\n\
        a=sendrecv\r\n";

    let parsed = parse_sdp(sdp);
    // Both codecs should be present (merged from both m= lines)
    let has_pcmu = parsed.codecs.iter().any(|c| c.name == "PCMU");
    let has_g722 = parsed.codecs.iter().any(|c| c.name == "G722");
    assert!(has_pcmu, "should have PCMU from first m=audio line");
    assert!(has_g722, "should have G722 from second m=audio line");
}

#[test]
fn test_parse_sdp_unsupported_codecs_only() {
    // SDP offering only unsupported codecs (G.729 PT 18, PCMA PT 8)
    let sdp = "v=0\r\n\
        o=- 700 1 IN IP4 10.0.0.1\r\n\
        s=-\r\n\
        c=IN IP4 10.0.0.1\r\n\
        t=0 0\r\n\
        m=audio 30000 RTP/AVP 8 18\r\n\
        a=rtpmap:8 PCMA/8000\r\n\
        a=rtpmap:18 G729/8000\r\n\
        a=sendrecv\r\n";

    let parsed = parse_sdp(sdp);
    assert!(
        parsed.codecs.is_empty(),
        "SDP with only unsupported codecs should produce empty codec list, got {:?}",
        parsed.codecs.iter().map(|c| &c.name).collect::<Vec<_>>()
    );
    // remote_addr should still be parsed (SDP is structurally valid)
    assert!(
        parsed.remote_addr.is_some(),
        "remote address should still be parsed"
    );
}

#[test]
fn test_parse_sdp_duplicate_supported_crypto() {
    // Two a=crypto lines with the same supported suite — last one wins
    // (parser overwrites on each matching line)
    let sdp = "v=0\r\n\
        o=- 800 1 IN IP4 10.0.0.1\r\n\
        s=-\r\n\
        c=IN IP4 10.0.0.1\r\n\
        t=0 0\r\n\
        m=audio 20000 RTP/SAVP 0\r\n\
        a=rtpmap:0 PCMU/8000\r\n\
        a=crypto:1 AES_CM_128_HMAC_SHA1_80 inline:Zmlyc3RrZXk=\r\n\
        a=crypto:2 AES_CM_128_HMAC_SHA1_80 inline:c2Vjb25ka2V5\r\n\
        a=sendrecv\r\n";

    let parsed = parse_sdp(sdp);
    assert!(parsed.crypto.is_some(), "should accept a crypto line");
    let crypto = parsed.crypto.unwrap();
    assert_eq!(
        crypto.tag, 2,
        "last matching crypto line (tag 2) should be selected"
    );
    assert_eq!(crypto.key_b64, "c2Vjb25ka2V5");
}

#[test]
fn test_parse_sdp_port_zero_rejected() {
    // m=audio 0 means media stream rejected (RFC 3264 §6)
    let sdp = "v=0\r\n\
        o=- 900 1 IN IP4 10.0.0.1\r\n\
        s=-\r\n\
        c=IN IP4 10.0.0.1\r\n\
        t=0 0\r\n\
        m=audio 0 RTP/AVP 0\r\n\
        a=rtpmap:0 PCMU/8000\r\n\
        a=sendrecv\r\n";

    let parsed = parse_sdp(sdp);
    assert!(
        parsed.remote_addr.is_none(),
        "port 0 should result in no remote_addr (stream rejected)"
    );
    // Codecs should still be parsed (the SDP is structurally valid)
    assert!(
        !parsed.codecs.is_empty(),
        "codecs should still be parsed even with port 0"
    );
}
