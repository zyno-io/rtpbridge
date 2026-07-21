use super::*;

// ── is_rtcp_mux_packet tests ────────────────────────────────────

#[test]
fn test_rtcp_mux_detects_sr() {
    // Sender Report: PT = 200 (0xC8)
    let pkt = [0x80, 200u8, 0x00, 0x06];
    assert!(RtpEndpoint::is_rtcp_mux_packet(&pkt));
}

#[test]
fn test_rtcp_mux_detects_rr() {
    // Receiver Report: PT = 201 (0xC9)
    let pkt = [0x80, 201u8, 0x00, 0x01];
    assert!(RtpEndpoint::is_rtcp_mux_packet(&pkt));
}

#[test]
fn test_rtcp_mux_detects_sdes() {
    let pkt = [0x80, 202u8, 0x00, 0x02];
    assert!(RtpEndpoint::is_rtcp_mux_packet(&pkt));
}

#[test]
fn test_rtcp_mux_detects_bye() {
    let pkt = [0x80, 203u8, 0x00, 0x01];
    assert!(RtpEndpoint::is_rtcp_mux_packet(&pkt));
}

#[test]
fn test_rtcp_mux_detects_app() {
    let pkt = [0x80, 204u8, 0x00, 0x03];
    assert!(RtpEndpoint::is_rtcp_mux_packet(&pkt));
}

#[test]
fn test_rtcp_mux_rejects_extended_types() {
    // Extended RTCP types (205-213) are excluded from demux to avoid collision
    // with RTP packets that have marker bit set + PT 77-85.
    // These are handled by the RTCP parser after initial classification.
    for pt in 205..=213u8 {
        let pkt = [0x80, pt, 0x00, 0x01];
        assert!(
            !RtpEndpoint::is_rtcp_mux_packet(&pkt),
            "PT {pt} should NOT be detected as RTCP in demux (extended range excluded)"
        );
    }
}

#[test]
fn test_rtcp_mux_rejects_rtp_pcmu() {
    // RTP PCMU: PT = 0, with marker bit clear → byte 1 = 0x00
    let pkt = [0x80, 0x00, 0x00, 0x01];
    assert!(!RtpEndpoint::is_rtcp_mux_packet(&pkt));
}

#[test]
fn test_rtcp_mux_rejects_rtp_pcmu_with_marker() {
    // RTP PCMU with marker bit set: byte 1 = 0x80 | 0 = 0x80 (128)
    let pkt = [0x80, 0x80, 0x00, 0x01];
    assert!(!RtpEndpoint::is_rtcp_mux_packet(&pkt));
}

#[test]
fn test_rtcp_mux_rejects_rtp_opus_111() {
    // RTP Opus PT=111: byte 1 = 111 (0x6F), no marker
    let pkt = [0x80, 111, 0x00, 0x01];
    assert!(!RtpEndpoint::is_rtcp_mux_packet(&pkt));
}

#[test]
fn test_rtcp_mux_rejects_rtp_opus_111_marker() {
    // RTP Opus PT=111 with marker: byte 1 = 0x80 | 111 = 0xEF (239)
    let pkt = [0x80, 0xEF, 0x00, 0x01];
    assert!(!RtpEndpoint::is_rtcp_mux_packet(&pkt));
}

#[test]
fn test_rtcp_mux_rejects_rtp_g722() {
    // RTP G.722: PT = 9
    let pkt = [0x80, 9, 0x00, 0x01];
    assert!(!RtpEndpoint::is_rtcp_mux_packet(&pkt));
}

#[test]
fn test_rtcp_mux_rejects_too_short() {
    assert!(!RtpEndpoint::is_rtcp_mux_packet(&[]));
    assert!(!RtpEndpoint::is_rtcp_mux_packet(&[0x80]));
}

#[test]
fn test_rtcp_mux_boundary_values() {
    // 199 should NOT match (just below RTCP range)
    assert!(!RtpEndpoint::is_rtcp_mux_packet(&[0x80, 199]));
    // 200 should match (first RTCP PT — SR)
    assert!(RtpEndpoint::is_rtcp_mux_packet(&[0x80, 200]));
    // 204 should match (last safe RTCP PT — APP)
    assert!(RtpEndpoint::is_rtcp_mux_packet(&[0x80, 204]));
    // 205 should NOT match (excluded to avoid marker-bit collision)
    assert!(!RtpEndpoint::is_rtcp_mux_packet(&[0x80, 205]));
}

// ── rtcp_mux address negotiation tests ──────────────────────────

fn make_sdp_with_mux(port: u16, rtcp_mux: bool) -> String {
    let mut sdp = format!(
        "v=0\r\n\
         o=- 123 1 IN IP4 10.0.0.1\r\n\
         s=-\r\n\
         c=IN IP4 10.0.0.1\r\n\
         t=0 0\r\n\
         m=audio {port} RTP/AVP 0 101\r\n\
         a=rtpmap:0 PCMU/8000\r\n\
         a=rtpmap:101 telephone-event/8000\r\n\
         a=sendrecv\r\n"
    );
    if rtcp_mux {
        sdp.push_str("a=rtcp-mux\r\n");
    }
    sdp
}

#[tokio::test]
async fn test_from_offer_with_rtcp_mux_sets_same_port() {
    let pool = crate::net::socket_pool::SocketPool::new("127.0.0.1".parse().unwrap(), 51000, 51100)
        .unwrap();
    let pair = pool.allocate_pair().await.unwrap();
    let (tx, _rx) = tokio::sync::mpsc::channel(16);

    let sdp = make_sdp_with_mux(20000, true);
    let (ep, _answer) = RtpEndpoint::from_offer(
        EndpointId::new_v4(),
        EndpointDirection::SendRecv,
        &sdp,
        pair,
        "127.0.0.1".parse().unwrap(),
        tx,
    )
    .unwrap();

    assert!(ep.rtcp_mux);
    assert_eq!(
        ep.remote_rtp_addr.unwrap().port(),
        ep.remote_rtcp_addr.unwrap().port(),
        "with rtcp-mux, RTCP addr should equal RTP addr"
    );
    assert_eq!(ep.remote_rtcp_addr.unwrap().port(), 20000);
}

#[tokio::test]
async fn test_from_offer_without_rtcp_mux_uses_port_plus_one() {
    let pool = crate::net::socket_pool::SocketPool::new("127.0.0.1".parse().unwrap(), 51100, 51200)
        .unwrap();
    let pair = pool.allocate_pair().await.unwrap();
    let (tx, _rx) = tokio::sync::mpsc::channel(16);

    let sdp = make_sdp_with_mux(20000, false);
    let (ep, _answer) = RtpEndpoint::from_offer(
        EndpointId::new_v4(),
        EndpointDirection::SendRecv,
        &sdp,
        pair,
        "127.0.0.1".parse().unwrap(),
        tx,
    )
    .unwrap();

    assert!(!ep.rtcp_mux);
    assert_eq!(ep.remote_rtp_addr.unwrap().port(), 20000);
    assert_eq!(
        ep.remote_rtcp_addr.unwrap().port(),
        20001,
        "without rtcp-mux, RTCP port should be RTP port + 1"
    );
}

#[tokio::test]
async fn test_from_offer_answer_advertises_selected_codec_first() {
    // Offer lists PCMU first, then G722, then Opus. We select Opus as the
    // highest-quality codec AND must advertise it first in the answer, so the
    // offerer transmits Opus — matching our send_codec and recv_clock_rate.
    // If the answer kept the offer order (PCMU first), the peer would send
    // PCMU while we encode Opus / clock at 48 kHz, breaking audio.
    let pool = crate::net::socket_pool::SocketPool::new("127.0.0.1".parse().unwrap(), 53000, 53100)
        .unwrap();
    let pair = pool.allocate_pair().await.unwrap();
    let (tx, _rx) = tokio::sync::mpsc::channel(16);

    let sdp = "v=0\r\n\
        o=- 1 1 IN IP4 127.0.0.1\r\n\
        s=-\r\n\
        c=IN IP4 127.0.0.1\r\n\
        t=0 0\r\n\
        m=audio 20000 RTP/AVP 0 9 111 101\r\n\
        a=rtpmap:0 PCMU/8000\r\n\
        a=rtpmap:9 G722/8000\r\n\
        a=rtpmap:111 opus/48000/2\r\n\
        a=rtpmap:101 telephone-event/8000\r\n\
        a=sendrecv\r\n";

    let (ep, answer) = RtpEndpoint::from_offer(
        EndpointId::new_v4(),
        EndpointDirection::SendRecv,
        sdp,
        pair,
        "127.0.0.1".parse().unwrap(),
        tx,
    )
    .unwrap();

    assert_eq!(
        ep.send_codec.as_ref().map(|c| c.name),
        Some("opus"),
        "should select the highest-quality offered codec (Opus)"
    );
    assert_eq!(
        ep.recv_clock_rate, 48000,
        "recv_clock_rate must track the selected codec (Opus)"
    );

    let m_line = answer
        .lines()
        .find(|l| l.starts_with("m=audio"))
        .expect("answer must contain an m=audio line");
    let pts: Vec<&str> = m_line.split_whitespace().skip(3).collect();
    assert_eq!(
        pts.first().copied(),
        Some("111"),
        "answer must advertise Opus (PT 111) first, not the offerer's first-listed PCMU; got: {m_line}"
    );

    // We decode all inbound media as send_codec, so the answer must NOT
    // advertise the other offered audio PTs (0/PCMU, 9/G722) — only the
    // selected codec plus telephone-event.
    assert_eq!(
        pts,
        vec!["111", "101"],
        "answer must advertise only the selected codec + telephone-event; got: {m_line}"
    );

    // telephone-event must keep the offered 8 kHz clock, not be rewritten to
    // 48 kHz just because Opus was selected (RFC 3264 — don't redefine the PT).
    assert!(
        answer.contains("a=rtpmap:101 telephone-event/8000"),
        "telephone-event must keep its offered 8000 clock rate; answer:\n{answer}"
    );
    assert!(
        !answer.contains("telephone-event/48000"),
        "telephone-event must not be rewritten to 48000; answer:\n{answer}"
    );
}

#[tokio::test]
async fn test_from_offer_selects_matching_telephone_event_when_multiple_are_offered() {
    let pool = crate::net::socket_pool::SocketPool::new("127.0.0.1".parse().unwrap(), 41300, 41400)
        .unwrap();
    let pair = pool.allocate_pair().await.unwrap();
    let (tx, _rx) = tokio::sync::mpsc::channel(16);
    let offer = "v=0\r\n\
                 o=Telnyx 1 2 IN IP4 127.0.0.1\r\n\
                 s=Telnyx\r\n\
                 c=IN IP4 127.0.0.1\r\n\
                 t=0 0\r\n\
                 m=audio 20000 RTP/AVP 0 9 101 105\r\n\
                 a=rtpmap:0 PCMU/8000\r\n\
                 a=rtpmap:9 G722/8000\r\n\
                 a=rtpmap:101 telephone-event/8000\r\n\
                 a=fmtp:101 0-15\r\n\
                 a=rtpmap:105 telephone-event/16000\r\n\
                 a=fmtp:105 0-15\r\n\
                 a=sendrecv\r\n";

    let (ep, answer) = RtpEndpoint::from_offer(
        EndpointId::new_v4(),
        EndpointDirection::SendRecv,
        offer,
        pair,
        "127.0.0.1".parse().unwrap(),
        tx,
    )
    .unwrap();

    assert_eq!(ep.send_codec.as_ref().map(|codec| codec.name), Some("G722"));
    assert_eq!(ep.telephone_event_pt, Some(101));
    assert_eq!(ep.telephone_event_clock_rate, 8000);
    assert!(
        answer.contains("m=audio 41300 RTP/AVP 9 101"),
        "answer:\n{answer}"
    );
    assert!(answer.contains("a=rtpmap:101 telephone-event/8000"));
    assert!(
        !answer.contains("telephone-event/16000"),
        "answer:\n{answer}"
    );
}

/// Build a minimal plain-RTP offer for the given connection family.
fn family_offer(is_v6: bool, port: u16) -> String {
    let (ver, addr) = if is_v6 {
        ("IP6", "::1")
    } else {
        ("IP4", "127.0.0.1")
    };
    format!(
        "v=0\r\n\
         o=- 1 1 IN {ver} {addr}\r\n\
         s=-\r\n\
         c=IN {ver} {addr}\r\n\
         t=0 0\r\n\
         m=audio {port} RTP/AVP 0 101\r\n\
         a=rtpmap:0 PCMU/8000\r\n\
         a=rtpmap:101 telephone-event/8000\r\n\
         a=sendrecv\r\n"
    )
}

#[tokio::test]
async fn test_update_remote_sdp_rejects_address_family_flip() {
    // A v4-bound RTP endpoint must reject a re-negotiation that flips the
    // remote to IPv6: the socket is bound to one family and we don't migrate,
    // so accepting it would leave us unable to reach the peer. The guard must
    // also run before any state mutation (no half-applied SDP).
    let pool = crate::net::socket_pool::SocketPool::new("127.0.0.1".parse().unwrap(), 41500, 41600)
        .unwrap();
    let pair = pool.allocate_pair().await.unwrap();
    let (tx, _rx) = tokio::sync::mpsc::channel(16);

    let (mut ep, _answer) = RtpEndpoint::from_offer(
        EndpointId::new_v4(),
        EndpointDirection::SendRecv,
        &family_offer(false, 20000),
        pair,
        "127.0.0.1".parse().unwrap(),
        tx,
    )
    .unwrap();
    assert!(ep.remote_rtp_addr.unwrap().is_ipv4());

    let err = ep
        .update_remote_sdp(&family_offer(true, 20000))
        .unwrap_err();
    assert!(
        err.to_string().contains("address family"),
        "unexpected error: {err}"
    );
    // State unchanged — the rejected v6 SDP must not have been applied.
    assert!(
        ep.remote_rtp_addr.unwrap().is_ipv4(),
        "remote address must still be the original IPv4 after a rejected flip"
    );
}

#[tokio::test]
async fn test_update_remote_sdp_allows_same_family_renegotiation() {
    // Same-family re-negotiation (e.g. a port change) must still work — the
    // guard only blocks v4↔v6 flips.
    let pool = crate::net::socket_pool::SocketPool::new("127.0.0.1".parse().unwrap(), 41600, 41700)
        .unwrap();
    let pair = pool.allocate_pair().await.unwrap();
    let (tx, _rx) = tokio::sync::mpsc::channel(16);

    let (mut ep, _answer) = RtpEndpoint::from_offer(
        EndpointId::new_v4(),
        EndpointDirection::SendRecv,
        &family_offer(false, 20000),
        pair,
        "127.0.0.1".parse().unwrap(),
        tx,
    )
    .unwrap();

    ep.update_remote_sdp(&family_offer(false, 21000)).unwrap();
    assert_eq!(
        ep.remote_rtp_addr.unwrap().port(),
        21000,
        "same-family re-INVITE should update the remote port"
    );
}

#[tokio::test]
async fn test_accept_answer_rejects_address_family_flip() {
    // The same guard protects accept_answer: a v4-bound offerer must reject a
    // v6 answer rather than half-apply it.
    let pool = crate::net::socket_pool::SocketPool::new("127.0.0.1".parse().unwrap(), 41700, 41800)
        .unwrap();
    let pair = pool.allocate_pair().await.unwrap();
    let (tx, _rx) = tokio::sync::mpsc::channel(16);

    let (mut ep, _offer) = RtpEndpoint::create_offer(
        EndpointId::new_v4(),
        EndpointDirection::SendRecv,
        pair,
        "127.0.0.1".parse().unwrap(),
        &[sdp::CODEC_PCMU],
        RtpMediaSecurity::PlainRtp,
        tx,
    )
    .unwrap();

    let err = ep.accept_answer(&family_offer(true, 20000)).unwrap_err();
    assert!(
        err.to_string().contains("address family"),
        "unexpected error: {err}"
    );
}

#[tokio::test]
async fn test_from_offer_applies_initial_remote_direction_in_auto_mode() {
    let pool = crate::net::socket_pool::SocketPool::new("127.0.0.1".parse().unwrap(), 41000, 41100)
        .unwrap();
    let pair = pool.allocate_pair().await.unwrap();
    let (tx, _rx) = tokio::sync::mpsc::channel(16);

    let sdp = make_sdp_with_mux(20000, true).replace("a=sendrecv", "a=recvonly");
    let (ep, _answer) = RtpEndpoint::from_offer(
        EndpointId::new_v4(),
        EndpointDirection::SendRecv,
        &sdp,
        pair,
        "127.0.0.1".parse().unwrap(),
        tx,
    )
    .unwrap();

    assert_eq!(
        ep.config.direction,
        EndpointDirection::RecvOnly,
        "remote a=recvonly parses directly to RecvOnly (peer-perspective)"
    );
}

#[tokio::test]
async fn test_accept_answer_with_rtcp_mux_updates_addr() {
    let pool = crate::net::socket_pool::SocketPool::new("127.0.0.1".parse().unwrap(), 51200, 51300)
        .unwrap();
    let pair = pool.allocate_pair().await.unwrap();
    let mut ep = RtpEndpoint::new(EndpointId::new_v4(), EndpointDirection::SendRecv, pair);

    let answer = make_sdp_with_mux(30000, true);
    ep.accept_answer(&answer).unwrap();

    assert!(ep.rtcp_mux);
    assert_eq!(ep.remote_rtp_addr.unwrap().port(), 30000);
    assert_eq!(
        ep.remote_rtcp_addr.unwrap().port(),
        30000,
        "accept_answer with rtcp-mux should set RTCP addr = RTP addr"
    );
}

#[tokio::test]
async fn test_bump_outbound_ssrc_resets_srtp_tx_roc() {
    // Regression for post-hold one-way audio. On unhold the outbound SSRC is
    // rotated via bump_outbound_ssrc(); the peer re-initialises its per-SSRC
    // ROC at 0 for the new SSRC, so our SRTP TX ROC must reset too. Without
    // the reset, a stale highest_seq in the upper half (> 0x8000) plus the
    // post-bump low sequence number spuriously bumps TX ROC to 1. The auth
    // tag covers the ROC, so a fresh receiver (ROC 0) rejects every packet —
    // the just-unheld peer hears nothing while still being heard.
    let pool = crate::net::socket_pool::SocketPool::new("127.0.0.1".parse().unwrap(), 52100, 52200)
        .unwrap();
    let pair = pool.allocate_pair().await.unwrap();
    let mut ep = RtpEndpoint::new(EndpointId::new_v4(), EndpointDirection::SendRecv, pair);

    let key_material: [u8; 30] = [
        0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B, 0x0C, 0x0D, 0x0E, 0x0F,
        0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1A, 0x1B, 0x1C, 0x1D, 0x1E,
    ];
    let key = base64_encode(&key_material);
    ep.srtp_tx = Some(SrtpContext::from_sdes_key(&key).unwrap());

    // Drive the TX context's highest_seq into the upper half (> 0x8000) — the
    // state a hold's worth of outbound (hold music) leaves behind.
    let high =
        crate::media::rtp::RtpHeader::build(0, 0xFFF0, 1600, 0x1111_1111, false, &[0x11; 160]);
    ep.srtp_tx.as_mut().unwrap().protect(&high).unwrap();

    // Unhold: rotate the outbound SSRC.
    ep.bump_outbound_ssrc();

    // Next outbound packet rides the new SSRC with a fresh low sequence.
    let low = crate::media::rtp::RtpHeader::build(0, 0x0001, 160, ep.our_ssrc, false, &[0xAA; 160]);
    let encrypted = ep.srtp_tx.as_mut().unwrap().protect(&low).unwrap();

    // A brand-new receiver (ROC 0, as the peer derives for the new SSRC) must
    // authenticate + decrypt. Fails if TX ROC was left/bumped to 1.
    let mut fresh_rx = SrtpContext::from_sdes_key(&key).unwrap();
    let decrypted = fresh_rx
        .unprotect(&encrypted)
        .expect("fresh receiver must decrypt post-bump packet (TX ROC must reset to 0)");
    assert_eq!(decrypted, low);
}

#[tokio::test]
async fn test_accept_answer_without_rtcp_mux_uses_port_plus_one() {
    let pool = crate::net::socket_pool::SocketPool::new("127.0.0.1".parse().unwrap(), 51300, 51400)
        .unwrap();
    let pair = pool.allocate_pair().await.unwrap();
    let mut ep = RtpEndpoint::new(EndpointId::new_v4(), EndpointDirection::SendRecv, pair);

    let answer = make_sdp_with_mux(30000, false);
    ep.accept_answer(&answer).unwrap();

    assert!(!ep.rtcp_mux);
    assert_eq!(ep.remote_rtcp_addr.unwrap().port(), 30001);
}

#[tokio::test]
async fn test_accept_answer_applies_initial_remote_direction_in_auto_mode() {
    let pool = crate::net::socket_pool::SocketPool::new("127.0.0.1".parse().unwrap(), 41100, 41200)
        .unwrap();
    let pair = pool.allocate_pair().await.unwrap();
    let (tx, _rx) = tokio::sync::mpsc::channel(16);
    let codecs = vec![
        crate::media::sdp::CODEC_PCMU,
        crate::media::sdp::CODEC_TELEPHONE_EVENT,
    ];

    let (mut ep, _offer) = RtpEndpoint::create_offer(
        EndpointId::new_v4(),
        EndpointDirection::SendRecv,
        pair,
        "127.0.0.1".parse().unwrap(),
        &codecs,
        RtpMediaSecurity::PlainRtp,
        tx,
    )
    .unwrap();

    let answer = make_sdp_with_mux(30000, true).replace("a=sendrecv", "a=sendonly");
    ep.accept_answer(&answer).unwrap();

    assert_eq!(
        ep.config.direction,
        EndpointDirection::SendOnly,
        "remote a=sendonly parses directly to SendOnly (peer-perspective)"
    );
}

#[tokio::test]
async fn test_update_remote_sdp_updates_addr_with_rtcp_mux() {
    let pool = crate::net::socket_pool::SocketPool::new("127.0.0.1".parse().unwrap(), 51400, 51500)
        .unwrap();
    let pair = pool.allocate_pair().await.unwrap();
    let mut ep = RtpEndpoint::new(EndpointId::new_v4(), EndpointDirection::SendRecv, pair);

    // First set an initial address via accept_answer
    let initial = make_sdp_with_mux(30000, true);
    ep.accept_answer(&initial).unwrap();
    assert_eq!(ep.remote_rtp_addr.unwrap().port(), 30000);

    // Re-INVITE with a different port — update_remote_sdp should pick it up
    let reinvite = make_sdp_with_mux(40000, true);
    ep.update_remote_sdp(&reinvite).unwrap();

    assert_eq!(ep.remote_rtp_addr.unwrap().port(), 40000);
    assert_eq!(ep.remote_rtcp_addr.unwrap().port(), 40000);
}

#[tokio::test]
async fn test_update_remote_sdp_preserves_codecs() {
    let pool = crate::net::socket_pool::SocketPool::new("127.0.0.1".parse().unwrap(), 51500, 51600)
        .unwrap();
    let pair = pool.allocate_pair().await.unwrap();
    let mut ep = RtpEndpoint::new(EndpointId::new_v4(), EndpointDirection::SendRecv, pair);

    // Seed the offered codec set so accept_answer's intersect-with-offer logic
    // actually populates self.codecs (it would otherwise reject everything).
    ep.codecs = vec![
        crate::media::sdp::CODEC_PCMU,
        crate::media::sdp::CODEC_TELEPHONE_EVENT,
    ];

    // Initial answer: PCMU only
    let initial = make_sdp_with_mux(30000, true);
    ep.accept_answer(&initial).unwrap();
    let codecs_before = ep.codecs.clone();
    let send_codec_before = ep.send_codec.clone();
    assert!(
        !codecs_before.is_empty(),
        "initial accept_answer should populate codecs"
    );

    // Re-INVITE with a different codec list (PCMA added, different PT)
    let reinvite = "\
        v=0\r\n\
        o=- 123 2 IN IP4 10.0.0.1\r\n\
        s=-\r\n\
        c=IN IP4 10.0.0.1\r\n\
        t=0 0\r\n\
        m=audio 40000 RTP/AVP 8 0 101\r\n\
        a=rtpmap:8 PCMA/8000\r\n\
        a=rtpmap:0 PCMU/8000\r\n\
        a=rtpmap:101 telephone-event/8000\r\n\
        a=sendrecv\r\n\
        a=rtcp-mux\r\n";

    ep.update_remote_sdp(reinvite).unwrap();

    // Address updated
    assert_eq!(ep.remote_rtp_addr.unwrap().port(), 40000);
    // Codecs unchanged — this is the whole point of update_remote_sdp vs accept_answer
    assert_eq!(
        ep.codecs, codecs_before,
        "update_remote_sdp must NOT modify codec list"
    );
    assert_eq!(
        ep.send_codec, send_codec_before,
        "update_remote_sdp must NOT modify send_codec"
    );
}

#[tokio::test]
async fn test_update_remote_sdp_resets_addr_lock() {
    let pool = crate::net::socket_pool::SocketPool::new("127.0.0.1".parse().unwrap(), 55100, 55200)
        .unwrap();
    let pair = pool.allocate_pair().await.unwrap();
    let mut ep = RtpEndpoint::new(EndpointId::new_v4(), EndpointDirection::SendRecv, pair);

    // Initial answer and manually lock the address
    let initial = make_sdp_with_mux(30000, true);
    ep.accept_answer(&initial).unwrap();
    ep.addr_locked = true;

    let reinvite = make_sdp_with_mux(40000, true);
    ep.update_remote_sdp(&reinvite).unwrap();

    assert!(
        !ep.addr_locked,
        "update_remote_sdp should reset addr lock so new NAT bindings can be learned"
    );
}

// Build a minimal valid PCMU RTP packet for symmetric-RTP latch tests.
fn make_rtp_packet(ssrc: u32) -> Vec<u8> {
    crate::media::rtp::RtpHeader::build(0, 1, 160, ssrc, false, &[0u8; 160])
}

/// Fix #1 (unit): as the offerer we may ring longer than the learning window
/// before the answer arrives. `accept_answer` must re-anchor the window to
/// answer time, not endpoint creation — otherwise it is already closed when
/// media starts and we lock to the (private, NAT'd) SDP address forever.
#[tokio::test]
async fn test_accept_answer_reanchors_stale_learning_window() {
    let pool = crate::net::socket_pool::SocketPool::new("127.0.0.1".parse().unwrap(), 51710, 51810)
        .unwrap();
    let pair = pool.allocate_pair().await.unwrap();
    let mut ep = RtpEndpoint::new(EndpointId::new_v4(), EndpointDirection::SendRecv, pair);

    // Simulate a long ring: created well before the answer, past the window.
    ep.created_at = Instant::now() - Duration::from_secs(ep.addr_learn_window_secs + 10);
    assert!(ep.created_at.elapsed() > Duration::from_secs(ep.addr_learn_window_secs));

    ep.accept_answer(&make_sdp_with_mux(30000, true)).unwrap();

    assert!(
        !ep.addr_locked,
        "accept_answer must leave the address unlocked"
    );
    assert!(
        ep.created_at.elapsed() < Duration::from_secs(ep.addr_learn_window_secs),
        "accept_answer must re-anchor the learning window to answer time"
    );
}

/// Fix #1 (end-to-end): offerer rings past the window, the answer advertises
/// a private address, then media arrives from the public post-NAT source
/// within the re-anchored window. We must latch the public source.
#[tokio::test]
async fn test_offerer_latches_public_source_after_long_ring() {
    let pool = crate::net::socket_pool::SocketPool::new("127.0.0.1".parse().unwrap(), 51810, 51910)
        .unwrap();
    let pair = pool.allocate_pair().await.unwrap();
    let mut ep = RtpEndpoint::new(EndpointId::new_v4(), EndpointDirection::SendRecv, pair);

    // Long ring before the answer.
    ep.created_at = Instant::now() - Duration::from_secs(ep.addr_learn_window_secs + 10);

    // Answer advertises a private (NAT'd) address: 10.0.0.1:30000.
    ep.accept_answer(&make_sdp_with_mux(30000, true)).unwrap();
    assert_eq!(ep.remote_rtp_addr.unwrap().ip().to_string(), "10.0.0.1");

    // First media actually arrives from the public post-NAT source.
    let public_src: SocketAddr = "203.0.113.7:50000".parse().unwrap();
    let _ = ep.handle_rtp(&make_rtp_packet(0x1234_5678), public_src);

    assert_eq!(
        ep.remote_rtp_addr,
        Some(public_src),
        "must latch the public source the media came from, not the SDP address"
    );
    assert_eq!(
        ep.remote_rtcp_addr,
        Some(public_src),
        "rtcp-mux: RTCP address follows the latched RTP source"
    );
}

/// Fix #2: even if media only starts *after* the (re-anchored) window has
/// elapsed — e.g. answered, then a long pause before cut-through — the first
/// authenticated packet must still latch its source. Without it the
/// window-expiry branch locks the stale SDP address. Uses a non-mux answer to
/// also exercise the RTCP port+1 path.
#[tokio::test]
async fn test_first_packet_latches_even_after_window_elapsed() {
    let pool = crate::net::socket_pool::SocketPool::new("127.0.0.1".parse().unwrap(), 51910, 52010)
        .unwrap();
    let pair = pool.allocate_pair().await.unwrap();
    let mut ep = RtpEndpoint::new(EndpointId::new_v4(), EndpointDirection::SendRecv, pair);

    ep.accept_answer(&make_sdp_with_mux(30000, false)).unwrap();

    // Window already elapsed by the time the first packet arrives.
    ep.created_at = Instant::now() - Duration::from_secs(ep.addr_learn_window_secs + 10);
    assert!(!ep.addr_locked);

    let public_src: SocketAddr = "203.0.113.9:40000".parse().unwrap();
    let _ = ep.handle_rtp(&make_rtp_packet(0x1234_5678), public_src);

    assert_eq!(
        ep.remote_rtp_addr,
        Some(public_src),
        "first packet must latch even though the learning window had elapsed"
    );
    assert_eq!(
        ep.remote_rtcp_addr.unwrap(),
        SocketAddr::new(public_src.ip(), public_src.port() + 1),
        "non-mux: RTCP address is the latched RTP source port + 1"
    );
}

/// Fix #1 (rekey/re-answer interaction — Codex finding #2): on an established
/// leg `remote_ssrc` is already set, so fix #2's first-packet latch can't
/// help. A re-answer (e.g. SRTP rekey) overwrites `remote_rtp_addr` from SDP
/// (back to the private address). `accept_answer` reopening the window is what
/// lets the next packet re-latch the live public source.
#[tokio::test]
async fn test_reanswer_reopens_window_to_relatch_established_leg() {
    let pool = crate::net::socket_pool::SocketPool::new("127.0.0.1".parse().unwrap(), 52010, 52110)
        .unwrap();
    let pair = pool.allocate_pair().await.unwrap();
    let mut ep = RtpEndpoint::new(EndpointId::new_v4(), EndpointDirection::SendRecv, pair);

    // Established leg, already latched to the public source.
    ep.accept_answer(&make_sdp_with_mux(30000, true)).unwrap();
    let public_src: SocketAddr = "203.0.113.7:50000".parse().unwrap();
    let _ = ep.handle_rtp(&make_rtp_packet(0x1234_5678), public_src);
    assert_eq!(ep.remote_rtp_addr, Some(public_src));
    assert!(ep.remote_ssrc.is_some(), "leg is established");

    // Time passes on the call (well past the window).
    ep.created_at = Instant::now() - Duration::from_secs(ep.addr_learn_window_secs + 10);

    // Re-answer overwrites the address back to the private SDP value.
    ep.accept_answer(&make_sdp_with_mux(30000, true)).unwrap();
    assert_eq!(ep.remote_rtp_addr.unwrap().ip().to_string(), "10.0.0.1");

    // Continued media from the same public source must re-latch.
    let _ = ep.handle_rtp(&make_rtp_packet(0x1234_5678), public_src);
    assert_eq!(
        ep.remote_rtp_addr,
        Some(public_src),
        "re-answer must reopen the window so the established leg re-latches its source"
    );
}

#[tokio::test]
async fn test_update_remote_sdp_applies_direction_in_auto_mode() {
    let pool = crate::net::socket_pool::SocketPool::new("127.0.0.1".parse().unwrap(), 41200, 41300)
        .unwrap();
    let pair = pool.allocate_pair().await.unwrap();
    let mut ep = RtpEndpoint::new(EndpointId::new_v4(), EndpointDirection::SendRecv, pair);

    let reinvite_sendonly = "\
        v=0\r\n\
        o=- 123 2 IN IP4 10.0.0.1\r\n\
        s=-\r\n\
        c=IN IP4 10.0.0.1\r\n\
        t=0 0\r\n\
        m=audio 40000 RTP/AVP 0 101\r\n\
        a=rtpmap:0 PCMU/8000\r\n\
        a=rtpmap:101 telephone-event/8000\r\n\
        a=sendonly\r\n\
        a=rtcp-mux\r\n";

    ep.update_remote_sdp(reinvite_sendonly).unwrap();
    assert_eq!(
        ep.config.direction,
        EndpointDirection::SendOnly,
        "remote a=sendonly parses directly to SendOnly in auto mode (peer-perspective)"
    );
}

#[tokio::test]
async fn test_update_remote_sdp_manual_override_takes_priority_until_auto() {
    let pool = crate::net::socket_pool::SocketPool::new("127.0.0.1".parse().unwrap(), 41300, 41400)
        .unwrap();
    let pair = pool.allocate_pair().await.unwrap();
    let mut ep = RtpEndpoint::new(EndpointId::new_v4(), EndpointDirection::SendRecv, pair);

    // Override to RecvOnly so it differs from the direction the SDP below
    // parses to (a=sendonly -> SendOnly), making the override-wins check real.
    ep.set_direction_override(EndpointDirectionUpdate::RecvOnly);
    assert_eq!(ep.config.direction, EndpointDirection::RecvOnly);

    let reinvite_sendonly = "\
        v=0\r\n\
        o=- 123 2 IN IP4 10.0.0.1\r\n\
        s=-\r\n\
        c=IN IP4 10.0.0.1\r\n\
        t=0 0\r\n\
        m=audio 40000 RTP/AVP 0 101\r\n\
        a=rtpmap:0 PCMU/8000\r\n\
        a=rtpmap:101 telephone-event/8000\r\n\
        a=sendonly\r\n\
        a=rtcp-mux\r\n";

    ep.update_remote_sdp(reinvite_sendonly).unwrap();
    assert_eq!(
        ep.config.direction,
        EndpointDirection::RecvOnly,
        "manual override must win over remote SDP direction"
    );

    ep.set_direction_override(EndpointDirectionUpdate::Auto);
    assert_eq!(
        ep.config.direction,
        EndpointDirection::SendOnly,
        "switching back to auto should apply last remote SDP direction (a=sendonly -> SendOnly)"
    );
}

#[tokio::test]
async fn test_update_remote_sdp_maps_inactive_direction() {
    let pool = crate::net::socket_pool::SocketPool::new("127.0.0.1".parse().unwrap(), 41400, 41500)
        .unwrap();
    let pair = pool.allocate_pair().await.unwrap();
    let mut ep = RtpEndpoint::new(EndpointId::new_v4(), EndpointDirection::SendRecv, pair);

    let reinvite_inactive = "\
        v=0\r\n\
        o=- 123 2 IN IP4 10.0.0.1\r\n\
        s=-\r\n\
        c=IN IP4 10.0.0.1\r\n\
        t=0 0\r\n\
        m=audio 40000 RTP/AVP 0 101\r\n\
        a=rtpmap:0 PCMU/8000\r\n\
        a=rtpmap:101 telephone-event/8000\r\n\
        a=inactive\r\n\
        a=rtcp-mux\r\n";

    ep.update_remote_sdp(reinvite_inactive).unwrap();
    assert_eq!(ep.config.direction, EndpointDirection::Inactive);
}

/// Regression: when a bridged peer (e.g. Grandstream GXP21xx) sends a
/// hold/unhold re-INVITE, its offer may list codecs in a different order
/// than the original offer. The 200 OK answer we generate must keep the
/// originally-negotiated `send_codec` listed first — otherwise the phone
/// will switch its outbound codec to whatever PT we list first while the
/// rtpbridge endpoint keeps sending the originally-negotiated codec,
/// producing one-way audio after un-hold.
#[tokio::test]
async fn test_update_remote_sdp_answer_lists_send_codec_first() {
    let pool = crate::net::socket_pool::SocketPool::new("127.0.0.1".parse().unwrap(), 51900, 52000)
        .unwrap();
    let pair = pool.allocate_pair().await.unwrap();
    let (tx, _rx) = tokio::sync::mpsc::channel(16);

    // Original offer lists G722 first — endpoint negotiates G722 as send_codec.
    let original_offer = "\
        v=0\r\n\
        o=- 123 1 IN IP4 10.0.0.1\r\n\
        s=-\r\n\
        c=IN IP4 10.0.0.1\r\n\
        t=0 0\r\n\
        m=audio 30000 RTP/AVP 9 0 101\r\n\
        a=rtpmap:9 G722/8000\r\n\
        a=rtpmap:0 PCMU/8000\r\n\
        a=rtpmap:101 telephone-event/8000\r\n\
        a=sendrecv\r\n";
    let (mut ep, _initial_answer) = RtpEndpoint::from_offer(
        EndpointId::new_v4(),
        EndpointDirection::SendRecv,
        original_offer,
        pair,
        "127.0.0.1".parse().unwrap(),
        tx,
    )
    .unwrap();
    assert_eq!(
        ep.send_codec.as_ref().map(|c| c.name),
        Some("G722"),
        "send_codec should be G722 from initial offer"
    );

    // Re-INVITE re-orders the codec list: PCMU first now.
    let reinvite = "\
        v=0\r\n\
        o=- 123 2 IN IP4 10.0.0.1\r\n\
        s=-\r\n\
        c=IN IP4 10.0.0.1\r\n\
        t=0 0\r\n\
        m=audio 40000 RTP/AVP 0 9 101\r\n\
        a=rtpmap:0 PCMU/8000\r\n\
        a=rtpmap:9 G722/8000\r\n\
        a=rtpmap:101 telephone-event/8000\r\n\
        a=sendrecv\r\n";

    let answer = ep.update_remote_sdp(reinvite).unwrap();

    // Our answer's m= line must list G722's PT (9) first.
    let m_line = answer
        .lines()
        .find(|l| l.starts_with("m=audio"))
        .expect("answer should have m=audio line");
    let pts: Vec<&str> = m_line.split_whitespace().skip(3).collect();
    assert_eq!(
        pts.first().copied(),
        Some("9"),
        "answer's first PT must be G722 (9), not the offerer's preferred PCMU. \
         Full m= line: {m_line}"
    );

    // send_codec must remain unchanged (this is the key invariant of update_remote_sdp).
    assert_eq!(
        ep.send_codec.as_ref().map(|c| c.name),
        Some("G722"),
        "send_codec must remain G722 across re-INVITE"
    );
}

/// Helper: SDP with SRTP crypto line.
fn make_savp_sdp(port: u16, key_b64: &str) -> String {
    format!(
        "v=0\r\n\
         o=- 123 1 IN IP4 10.0.0.1\r\n\
         s=-\r\n\
         c=IN IP4 10.0.0.1\r\n\
         t=0 0\r\n\
         m=audio {port} RTP/SAVP 0 101\r\n\
         a=rtpmap:0 PCMU/8000\r\n\
         a=rtpmap:101 telephone-event/8000\r\n\
         a=sendrecv\r\n\
         a=rtcp-mux\r\n\
         a=crypto:1 AES_CM_128_HMAC_SHA1_80 inline:{key_b64}\r\n"
    )
}

#[tokio::test]
async fn test_accept_answer_rejects_plain_rtp_downgrade_from_srtp_offer() {
    let pool = crate::net::socket_pool::SocketPool::new("127.0.0.1".parse().unwrap(), 51600, 51700)
        .unwrap();
    let pair = pool.allocate_pair().await.unwrap();
    let (mut ep, _) = RtpEndpoint::create_offer(
        EndpointId::new_v4(),
        EndpointDirection::SendRecv,
        pair,
        "127.0.0.1".parse().unwrap(),
        &[
            crate::media::sdp::CODEC_PCMU,
            crate::media::sdp::CODEC_TELEPHONE_EVENT,
        ],
        RtpMediaSecurity::Srtp,
        tokio::sync::mpsc::channel(1).0,
    )
    .unwrap();

    let plain_answer = "v=0\r\n\
        o=- 254709 865470 IN IP4 208.69.81.90\r\n\
        s=-\r\n\
        c=IN IP4 208.69.81.90\r\n\
        t=0 0\r\n\
        m=audio 45502 RTP/AVP 0 101\r\n\
        a=rtpmap:101 telephone-event/8000\r\n\
        a=fmtp:101 0-15\r\n\
        a=ptime:20\r\n";

    let err = ep.accept_answer(plain_answer).unwrap_err();
    assert!(
        err.to_string().contains("offered SRTP, answered RTP"),
        "unexpected error: {err}"
    );
    assert_eq!(ep.state, EndpointState::Connecting);
    assert!(ep.remote_rtp_addr.is_none());
}

#[tokio::test]
async fn test_accept_answer_rejects_unoffered_srtp() {
    let pool = crate::net::socket_pool::SocketPool::new("127.0.0.1".parse().unwrap(), 55200, 55300)
        .unwrap();
    let pair = pool.allocate_pair().await.unwrap();
    let (mut ep, _) = RtpEndpoint::create_offer(
        EndpointId::new_v4(),
        EndpointDirection::SendRecv,
        pair,
        "127.0.0.1".parse().unwrap(),
        &[crate::media::sdp::CODEC_PCMU],
        RtpMediaSecurity::PlainRtp,
        tokio::sync::mpsc::channel(1).0,
    )
    .unwrap();

    let err = ep
        .accept_answer(&make_savp_sdp(
            45502,
            "E4peZWnTvtquGbT3QN3ZJOM8i0Q2zNLc55bTN2VW",
        ))
        .unwrap_err();
    assert!(
        err.to_string().contains("offered RTP, answered SRTP"),
        "unexpected error: {err}"
    );
    assert_eq!(ep.state, EndpointState::Connecting);
    assert!(ep.remote_rtp_addr.is_none());
}

#[tokio::test]
async fn test_accept_answer_allows_plain_rtp_for_osrtp_offer() {
    let pool = crate::net::socket_pool::SocketPool::new("127.0.0.1".parse().unwrap(), 55300, 55400)
        .unwrap();
    let pair = pool.allocate_pair().await.unwrap();
    let (mut ep, offer) = RtpEndpoint::create_offer(
        EndpointId::new_v4(),
        EndpointDirection::SendRecv,
        pair,
        "127.0.0.1".parse().unwrap(),
        &[
            crate::media::sdp::CODEC_PCMU,
            crate::media::sdp::CODEC_TELEPHONE_EVENT,
        ],
        RtpMediaSecurity::OptionalSrtp,
        tokio::sync::mpsc::channel(1).0,
    )
    .unwrap();

    assert!(offer.contains("m=audio ") && offer.contains(" RTP/AVP 0 101"));
    assert!(offer.contains("a=crypto:1 AES_CM_128_HMAC_SHA1_80 inline:"));

    let plain_answer = "v=0\r\n\
        o=- 254709 865470 IN IP4 208.69.81.90\r\n\
        s=-\r\n\
        c=IN IP4 208.69.81.90\r\n\
        t=0 0\r\n\
        m=audio 45502 RTP/AVP 0 101\r\n\
        a=rtpmap:101 telephone-event/8000\r\n\
        a=fmtp:101 0-15\r\n\
        a=ptime:20\r\n";

    ep.accept_answer(plain_answer).unwrap();
    assert_eq!(ep.state, EndpointState::Connected);
    assert!(ep.srtp_tx.is_none());
    assert!(ep.srtcp_tx.is_none());
    assert!(!ep.has_srtp());
}

/// A hold/unhold re-INVITE that carries the same SRTP crypto line as the initial
/// answer must NOT trigger a rekey: resetting the SRTP RX context discards the
/// running rollover counter and produces garbled/static audio for ~5s while the
/// dual-context transition waits out. Phones like the Grandstream GXP2130 reuse
/// the same key across re-INVITEs, so this is the common path.
#[tokio::test]
async fn test_update_remote_sdp_same_srtp_key_skips_rekey() {
    let pool = crate::net::socket_pool::SocketPool::new("127.0.0.1".parse().unwrap(), 51700, 51800)
        .unwrap();
    let pair = pool.allocate_pair().await.unwrap();
    let (mut ep, _) = RtpEndpoint::create_offer(
        EndpointId::new_v4(),
        EndpointDirection::SendRecv,
        pair,
        "127.0.0.1".parse().unwrap(),
        &[
            crate::media::sdp::CODEC_PCMU,
            crate::media::sdp::CODEC_TELEPHONE_EVENT,
        ],
        RtpMediaSecurity::Srtp,
        tokio::sync::mpsc::channel(1).0,
    )
    .unwrap();

    let key = "E4peZWnTvtquGbT3QN3ZJOM8i0Q2zNLc55bTN2VW";
    ep.accept_answer(&make_savp_sdp(30000, key)).unwrap();
    assert!(
        ep.srtp_rx.is_some(),
        "initial accept_answer should install SRTP RX"
    );
    assert!(ep.srtp_rx_new.is_none(), "no rekey yet");

    // Simulate that we had already learned a remote SSRC during the call
    ep.remote_ssrc = Some(0xDEAD_BEEF);

    // Re-INVITE with the SAME crypto key
    ep.update_remote_sdp(&make_savp_sdp(40000, key)).unwrap();

    assert!(ep.srtp_rx.is_some(), "SRTP RX context must be preserved");
    assert!(
        ep.srtp_rx_new.is_none(),
        "same-key re-INVITE must NOT start a dual-context transition (would wipe ROC)"
    );
    assert!(
        ep.rekey_switchover.is_none(),
        "no rekey switchover should be scheduled for a same-key re-INVITE"
    );
    assert_eq!(
        ep.remote_rtp_addr.unwrap().port(),
        40000,
        "address still updates"
    );

    // SSRC must be forgotten so it's relearned from the next inbound packet —
    // phones often switch SSRC across hold/unhold (always after RTCP BYE).
    assert!(
        ep.remote_ssrc.is_none(),
        "remote_ssrc must be cleared so it's relearned from the next packet"
    );
}

/// When the re-INVITE carries a genuinely different crypto key, update_remote_sdp
/// must kick off the 5-second dual-context transition so in-flight packets on the
/// old key decrypt until the remote switches over.
#[tokio::test]
async fn test_update_remote_sdp_different_srtp_key_triggers_rekey() {
    let pool = crate::net::socket_pool::SocketPool::new("127.0.0.1".parse().unwrap(), 51800, 51900)
        .unwrap();
    let pair = pool.allocate_pair().await.unwrap();
    let (mut ep, _) = RtpEndpoint::create_offer(
        EndpointId::new_v4(),
        EndpointDirection::SendRecv,
        pair,
        "127.0.0.1".parse().unwrap(),
        &[
            crate::media::sdp::CODEC_PCMU,
            crate::media::sdp::CODEC_TELEPHONE_EVENT,
        ],
        RtpMediaSecurity::Srtp,
        tokio::sync::mpsc::channel(1).0,
    )
    .unwrap();

    let key1 = "E4peZWnTvtquGbT3QN3ZJOM8i0Q2zNLc55bTN2VW";
    let key2 = "yyuehWOy8nibRn+adLgDP5fiaZ61fEw7RuxBimMr";

    ep.accept_answer(&make_savp_sdp(30000, key1)).unwrap();
    assert_eq!(ep.srtp_rx_key_b64.as_deref(), Some(key1));

    ep.update_remote_sdp(&make_savp_sdp(30000, key2)).unwrap();

    assert!(
        ep.srtp_rx_new.is_some(),
        "different-key re-INVITE must start a dual-context transition"
    );
    assert!(
        ep.rekey_switchover.is_some(),
        "rekey switchover deadline must be set"
    );
    assert_eq!(
        ep.srtp_rx_key_b64.as_deref(),
        Some(key2),
        "key tracker must be updated to the new key"
    );
}

/// Allocate a real socket pair and return a fresh PCMU endpoint suitable
/// for testing the timestamp-continuity logic in isolation.
async fn mk_ts_endpoint(start: u16, end: u16) -> RtpEndpoint {
    let pool =
        crate::net::socket_pool::SocketPool::new("127.0.0.1".parse().unwrap(), start, end).unwrap();
    let pair = pool.allocate_pair().await.unwrap();
    let mut ep = RtpEndpoint::new(EndpointId::new_v4(), EndpointDirection::SendRecv, pair);
    ep.send_codec = Some(crate::media::sdp::CODEC_PCMU); // 8000Hz → 160 step
    ep
}

#[tokio::test]
async fn test_ts_first_packet_seeds_from_source() {
    let mut ep = mk_ts_endpoint(52000, 52100).await;
    let src = EndpointId::new_v4();

    let (ts, marker) = ep.advance_outbound_timeline(src, 12_345, false);
    assert_eq!(ts, 12_345, "first packet should seed from source TS");
    assert!(!marker, "first packet's marker passes through (false here)");
    assert_eq!(ep.last_outbound_ts, Some(12_345));
    assert_eq!(ep.last_source_id, Some(src));
    assert_eq!(ep.last_source_ts, Some(12_345));
}

/// Regression: a single injected DTMF digit (one RFC 4733 event) must reach
/// the wire as ONE event — one marker, one constant timestamp — even when
/// normal audio is interleaved on the same endpoint. Previously each DTMF
/// packet was run through `advance_outbound_timeline`, so every audio↔DTMF
/// switch forced a marker + timestamp bump and the far end saw the digit
/// many times.
#[tokio::test]
async fn test_dtmf_event_holds_one_ts_across_interleaved_audio() {
    let mut ep = mk_ts_endpoint(52900, 53000).await;
    ep.telephone_event_pt = Some(101);
    let audio_src = EndpointId::new_v4();

    // Audio flowing to this endpoint establishes the timeline.
    ep.advance_outbound_timeline(audio_src, 1000, false);
    ep.advance_outbound_timeline(audio_src, 1160, false);
    let audio_anchor = ep.last_outbound_ts.unwrap();

    // DTMF event begins (marker packet). The injected RTP timestamp is
    // a random value unrelated to our audio timeline.
    let injected_ts = 0xDEAD_BEEF;
    let dtmf_ts0 = ep.dtmf_outbound_ts(true, injected_ts);
    assert_eq!(
        dtmf_ts0,
        audio_anchor.wrapping_add(160),
        "DTMF anchors to the next audio frame, not the injected TS"
    );

    // One audio packet is interleaved between DTMF packets (the real bug
    // trigger). It advances the audio timeline but must not disturb DTMF.
    ep.advance_outbound_timeline(audio_src, 1320, false);

    // Continuation + redundant end packets (marker=false) reuse the anchor.
    for _ in 0..4 {
        let ts = ep.dtmf_outbound_ts(false, injected_ts);
        assert_eq!(ts, dtmf_ts0, "all packets of one event share one TS");
    }

    // A second injected digit (new marker) re-anchors to the now-advanced
    // audio timeline, so it's distinct from the first event.
    let dtmf_ts1 = ep.dtmf_outbound_ts(true, injected_ts);
    assert_ne!(dtmf_ts1, dtmf_ts0, "a new event gets a fresh anchor");
}

/// Regression: two DTMF events back-to-back with NO interleaved audio must
/// still get distinct timestamps. The marker anchor advances
/// `last_outbound_ts`, so the second event re-anchors past the first instead
/// of reusing the same value (which a receiver would dedup as a redundant
/// copy of the first event).
#[tokio::test]
async fn test_dtmf_back_to_back_during_silence_distinct_ts() {
    let mut ep = mk_ts_endpoint(53000, 53100).await;
    ep.telephone_event_pt = Some(101);
    let audio_src = EndpointId::new_v4();

    // Establish an audio timeline, then go silent (no more audio packets).
    ep.advance_outbound_timeline(audio_src, 1000, false);
    let silent_anchor = ep.last_outbound_ts.unwrap();

    // First digit: one marker packet + two redundant end copies.
    let d0 = ep.dtmf_outbound_ts(true, 0xAAAA);
    assert_eq!(d0, silent_anchor.wrapping_add(160));
    assert_eq!(ep.dtmf_outbound_ts(false, 0xAAAA), d0);
    assert_eq!(ep.dtmf_outbound_ts(false, 0xAAAA), d0);

    // Second digit immediately after, still no audio in between.
    let d1 = ep.dtmf_outbound_ts(true, 0xBBBB);
    assert_eq!(d1, d0.wrapping_add(160), "second event advances past first");
    assert_ne!(d1, d0, "back-to-back silent digits must not share a TS");
}

/// Regression: when the first-ever outbound packet is DTMF, it seeds the
/// outbound timeline (`last_outbound_ts`) so the following audio packet
/// advances from the DTMF anchor with a marker, rather than taking the
/// first-packet arm and jumping to the source timestamp under the same SSRC.
#[tokio::test]
async fn test_dtmf_first_seeds_timeline_for_following_audio() {
    let mut ep = mk_ts_endpoint(53100, 53200).await;
    ep.telephone_event_pt = Some(101);
    let audio_src = EndpointId::new_v4();

    // No audio yet — DTMF is the first thing out. Anchor falls back to the
    // injected timestamp and seeds the timeline.
    let injected_ts = 0xC0FF_EE00;
    let d0 = ep.dtmf_outbound_ts(true, injected_ts);
    assert_eq!(
        d0, injected_ts,
        "first-ever DTMF seeds from the injected TS"
    );
    assert_eq!(
        ep.last_outbound_ts,
        Some(injected_ts),
        "DTMF must seed the outbound timeline"
    );

    // Audio now starts. It must advance from the DTMF anchor (source change
    // → one-frame bump + marker), not jump to the source's own timestamp.
    let (ts, marker) = ep.advance_outbound_timeline(audio_src, 5_000, false);
    assert_eq!(
        ts,
        injected_ts.wrapping_add(160),
        "audio advances from the DTMF-seeded anchor, not the source TS"
    );
    assert!(marker, "first audio after DTMF re-anchors with a marker");
}

/// DTMF timestamps advance on the telephone-event clock, not the audio codec
/// clock. With Opus media (48000) but telephone-event negotiated at 8000, the
/// per-event bump must be one 20ms frame at 8000 (160), not 48000 (960) —
/// otherwise digits sent to a SIP phone land on the wrong timeline.
#[tokio::test]
async fn test_dtmf_outbound_ts_uses_te_clock_not_media_clock() {
    let mut ep = mk_ts_endpoint(53300, 53400).await;
    ep.telephone_event_pt = Some(101);
    ep.send_codec = Some(crate::media::sdp::CODEC_OPUS); // media @ 48000
    ep.telephone_event_clock_rate = 8000; // DTMF negotiated @ 8000
    let audio_src = EndpointId::new_v4();

    // Two Opus-spaced audio packets (960 apart) would teach a media-clock
    // bump; the DTMF bump must still be 160.
    ep.advance_outbound_timeline(audio_src, 1000, false);
    ep.advance_outbound_timeline(audio_src, 1960, false);
    let anchor = ep.last_outbound_ts.unwrap();

    let dtmf_ts = ep.dtmf_outbound_ts(true, 0x1234);
    assert_eq!(
        dtmf_ts,
        anchor.wrapping_add(160),
        "DTMF bump must use the te clock (8000/50=160), not media (48000/50=960)"
    );
}

#[tokio::test]
async fn test_ts_same_source_uses_source_delta() {
    let mut ep = mk_ts_endpoint(52100, 52200).await;
    let src = EndpointId::new_v4();

    let (ts1, _) = ep.advance_outbound_timeline(src, 1000, false);
    let (ts2, m2) = ep.advance_outbound_timeline(src, 1160, false);
    let (ts3, m3) = ep.advance_outbound_timeline(src, 1320, false);

    assert_eq!(ts1, 1000, "seed");
    assert_eq!(ts2, 1160, "advance by source delta of 160");
    assert_eq!(ts3, 1320, "advance by source delta of 160");
    assert!(!m2 && !m3, "no marker override on same-source steady flow");
    assert_eq!(
        ep.learned_step,
        Some(160),
        "learned step adopts the source pacing"
    );
}

#[tokio::test]
async fn test_ts_source_change_bumps_one_frame_and_marks() {
    let mut ep = mk_ts_endpoint(52200, 52300).await;
    let src_a = EndpointId::new_v4();
    let src_b = EndpointId::new_v4();

    // Establish a baseline on source A.
    ep.advance_outbound_timeline(src_a, 1000, false);
    ep.advance_outbound_timeline(src_a, 1160, false);
    let last_outbound = ep.last_outbound_ts.unwrap();

    // Source B's timestamps live in a totally different domain.
    let (ts, marker) = ep.advance_outbound_timeline(src_b, 9_999_000, false);

    assert_eq!(
        ts,
        last_outbound.wrapping_add(160),
        "source change must bump by one frame, not jump to source B's TS"
    );
    assert!(
        marker,
        "source change must set marker so the receiver re-anchors"
    );
    assert_eq!(ep.last_source_id, Some(src_b));
    assert_eq!(ep.last_source_ts, Some(9_999_000));
}

#[tokio::test]
async fn test_ts_mixer_passthrough_transition_preserves_continuity() {
    // Reproduces the bug: mixer (source=nil) feeds during hold-music, then
    // we transition back to passthrough from a real source after un-hold.
    // Without destination-owned timeline, the wire timestamp domain jumps.
    let mut ep = mk_ts_endpoint(52300, 52400).await;
    let real_src = EndpointId::new_v4();
    let mixer = EndpointId::nil();

    // Real source pre-hold.
    ep.advance_outbound_timeline(real_src, 5_000, false);
    ep.advance_outbound_timeline(real_src, 5_160, false);

    // Hold: mixer takes over with its own monotonic clock.
    let (mix1, m1) = ep.advance_outbound_timeline(mixer, 9_000_000, false);
    let (mix2, m2) = ep.advance_outbound_timeline(mixer, 9_000_160, false);
    assert_eq!(
        mix1,
        5_160 + 160,
        "mixer first packet bumps by one frame off real source"
    );
    assert!(m1, "transition into mixer sets marker");
    assert_eq!(
        mix2,
        mix1 + 160,
        "subsequent mixer packets follow source delta"
    );
    assert!(!m2, "steady mixer flow does not set marker");

    // Un-hold: real source resumes — and its TS may be wildly different.
    let (resume, m3) = ep.advance_outbound_timeline(real_src, 5_320, false);
    assert_eq!(
        resume,
        mix2 + 160,
        "mixer→passthrough must continue the destination's timeline"
    );
    assert!(
        m3,
        "mixer→passthrough sets marker so jitter buffer re-anchors"
    );
}

#[tokio::test]
async fn test_ts_duplicate_holds_outbound() {
    let mut ep = mk_ts_endpoint(52400, 52500).await;
    let src = EndpointId::new_v4();

    ep.advance_outbound_timeline(src, 1000, false);
    let (ts2, _) = ep.advance_outbound_timeline(src, 1160, false);
    // Same source-TS again — duplicate / retransmit.
    let (ts3, m3) = ep.advance_outbound_timeline(src, 1160, false);

    assert_eq!(
        ts3, ts2,
        "duplicate source TS must not advance the wire timeline"
    );
    assert!(!m3, "duplicate is not a new talk-spurt");
}

#[tokio::test]
async fn test_ts_huge_same_source_jump_clamps_and_marks() {
    let mut ep = mk_ts_endpoint(52500, 52600).await;
    let src = EndpointId::new_v4();

    ep.advance_outbound_timeline(src, 1000, false);
    ep.advance_outbound_timeline(src, 1160, false);
    let last_out = ep.last_outbound_ts.unwrap();

    // Source TS jumps by ~10s (50_000 samples at 8kHz) — way beyond
    // 10×160. Should collapse to one frame and set marker.
    let (ts, marker) = ep.advance_outbound_timeline(src, 1160 + 50_000, false);
    assert_eq!(
        ts,
        last_out + 160,
        "huge in-source jump clamps to one frame"
    );
    assert!(marker, "in-source discontinuity sets marker");
}

#[tokio::test]
async fn test_ts_learned_step_used_for_source_change() {
    let mut ep = mk_ts_endpoint(52600, 52700).await;
    let src_a = EndpointId::new_v4();
    let src_b = EndpointId::new_v4();

    // Source A paces at 320 (40ms ptime). Two packets establish the learned step.
    ep.advance_outbound_timeline(src_a, 1000, false);
    ep.advance_outbound_timeline(src_a, 1320, false);
    assert_eq!(ep.learned_step, Some(320), "learned step adopts 40ms ptime");
    let last_out = ep.last_outbound_ts.unwrap();

    // Source change should now bump by the learned step (320), not the
    // codec's nominal 20ms step (160).
    let (ts, _) = ep.advance_outbound_timeline(src_b, 7777, false);
    assert_eq!(
        ts,
        last_out + 320,
        "source-change bump should use learned step when available"
    );
}

#[tokio::test]
async fn test_ts_no_send_codec_falls_back_to_pcmu_step() {
    let pool = crate::net::socket_pool::SocketPool::new("127.0.0.1".parse().unwrap(), 52700, 52800)
        .unwrap();
    let pair = pool.allocate_pair().await.unwrap();
    let mut ep = RtpEndpoint::new(EndpointId::new_v4(), EndpointDirection::SendRecv, pair);
    // Intentionally leave send_codec as None.
    let src_a = EndpointId::new_v4();
    let src_b = EndpointId::new_v4();
    ep.advance_outbound_timeline(src_a, 1000, false);
    let (ts, _) = ep.advance_outbound_timeline(src_b, 50_000, false);
    assert_eq!(ts, 1000 + 160, "fallback step is 160 (8kHz/50)");
}

#[tokio::test]
async fn test_bump_outbound_ssrc_rotates_state() {
    let pool = crate::net::socket_pool::SocketPool::new("127.0.0.1".parse().unwrap(), 52800, 52900)
        .unwrap();
    let pair = pool.allocate_pair().await.unwrap();
    let mut ep = RtpEndpoint::new(EndpointId::new_v4(), EndpointDirection::SendRecv, pair);

    // Establish a prior outbound timeline as if we'd been sending.
    let src = EndpointId::new_v4();
    ep.advance_outbound_timeline(src, 12_345, false);
    let prev_ssrc = ep.our_ssrc;
    let prev_seq = ep.seq_no;
    assert!(ep.last_outbound_ts.is_some());
    assert!(ep.last_source_id.is_some());

    ep.bump_outbound_ssrc();

    assert_ne!(ep.our_ssrc, prev_ssrc, "SSRC must rotate");
    assert_ne!(
        ep.seq_no, prev_seq,
        "seq_no must restart from a fresh random base"
    );
    assert!(
        ep.last_outbound_ts.is_none(),
        "outbound timeline anchor must be cleared so the next packet \
         seeds from the new random base with a marker bit"
    );
    assert!(ep.last_source_id.is_none());
    assert!(ep.last_source_ts.is_none());
}

#[tokio::test]
async fn test_direction_is_sending_classification() {
    // is_sending() == "rtpbridge transmits to the peer" == the peer is
    // willing to receive (peer-perspective: SendRecv or RecvOnly).
    assert!(EndpointDirection::SendRecv.is_sending());
    assert!(EndpointDirection::RecvOnly.is_sending());
    assert!(!EndpointDirection::SendOnly.is_sending());
    assert!(!EndpointDirection::Inactive.is_sending());
}

/// The wire-level counter counts EVERY datagram the RTP and RTCP sockets
/// receive — including non-RTP junk — independently of the media-plane
/// counter, which only counts validated RTP. This is the core property the
/// remote-network-failure signal relies on.
#[tokio::test]
async fn raw_recv_counts_datagrams_on_both_sockets() {
    let pool = crate::net::socket_pool::SocketPool::new("127.0.0.1".parse().unwrap(), 53400, 53500)
        .unwrap();
    let pair = pool.allocate_pair().await.unwrap();
    let mut ep = RtpEndpoint::new(EndpointId::new_v4(), EndpointDirection::SendRecv, pair);
    let rtp_addr = ep.local_rtp_addr;
    let rtcp_addr = ep.rtcp_socket.local_addr().unwrap();
    let raw = Arc::clone(&ep.raw_recv);

    // Keep the receiver alive so the recv tasks' blocking sends complete.
    let (tx, mut rx) = tokio::sync::mpsc::channel(16);
    ep.start_recv_tasks(tx);

    // Two non-RTP datagrams: one to the RTP socket, one to the RTCP socket.
    // Neither parses as RTP media, but both arrived on the wire.
    let sender = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let junk = [0xFFu8; 8];
    sender.send_to(&junk, rtp_addr).await.unwrap();
    sender.send_to(&junk, rtcp_addr).await.unwrap();

    // record() runs before the recv task forwards each datagram, so once we
    // have drained both from the channel we know both were counted.
    for _ in 0..2 {
        tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("recv task should forward the datagram")
            .expect("session channel stays open");
    }

    assert_eq!(raw.packets(), 2, "both datagrams counted at the wire");
    assert_eq!(raw.bytes(), 16);
    // The media-plane counter is untouched: nothing was parsed as RTP.
    assert_eq!(ep.stats.inbound_packets, 0);
}

/// Junk fed through the media path returns no packet and never bumps the
/// media counter — confirming `stats.inbound_*` is media-only, the
/// complement of the wire-level `raw_recv` counter.
#[tokio::test]
async fn handle_rtp_rejects_junk_without_counting_media() {
    let pool = crate::net::socket_pool::SocketPool::new("127.0.0.1".parse().unwrap(), 53500, 53600)
        .unwrap();
    let pair = pool.allocate_pair().await.unwrap();
    let mut ep = RtpEndpoint::new(EndpointId::new_v4(), EndpointDirection::SendRecv, pair);
    let source = ep.local_rtp_addr;

    let before = ep.stats.inbound_packets;
    // First byte 0xFF => RTP version 3 (invalid); header parse fails.
    let result = ep.handle_rtp(&[0xFFu8; 16], source);
    assert!(result.is_none(), "junk must not parse as RTP");
    assert_eq!(
        ep.stats.inbound_packets, before,
        "media counter only counts validated RTP"
    );
}
