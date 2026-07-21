use super::*;

async fn mk_webrtc_ts_endpoint() -> WebRtcEndpoint {
    let id = uuid::Uuid::new_v4();
    let bind_addr: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();
    WebRtcEndpoint::new_with_socket(
        id,
        EndpointConfig {
            direction: EndpointDirection::SendRecv,
        },
        &[bind_addr],
        Arc::new(Metrics::new()),
        false,
    )
    .await
    .expect("new_with_socket should succeed")
}

#[tokio::test]
async fn test_enabled_offer_advertises_legacy_ice_renomination() {
    let id = uuid::Uuid::new_v4();
    let bind_addr: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();
    let (tx, _rx) = mpsc::channel(16);

    let (_endpoint, offer) = WebRtcEndpoint::create_offer_with_legacy_ice_renomination(
        id,
        EndpointDirection::SendRecv,
        &[bind_addr],
        tx,
        Arc::new(Metrics::new()),
        true,
    )
    .await
    .expect("experiment offer should be created");

    assert!(
        offer.contains("a=ice-options:trickle renomination\r\n"),
        "the runtime-enabled endpoint must advertise the patched capability"
    );
}

#[cfg(feature = "legacy-ice-renomination-experiment")]
#[test]
fn test_experiment_fault_drops_only_first_selected_path_after_delay() {
    let endpoint_id = EndpointId::new_v4();
    let local: SocketAddr = "127.0.0.1:40000".parse().unwrap();
    let first_remote: SocketAddr = "192.0.2.10:50000".parse().unwrap();
    let replacement_remote: SocketAddr = "198.51.100.20:50001".parse().unwrap();
    let now = Instant::now();
    let mut fault = RenominationPathFault {
        delay: Duration::from_millis(500),
        first_remote: None,
        activate_at: None,
        activated: false,
        replacement_observed: false,
    };

    fault.observe_selected_pair(endpoint_id, local, first_remote, now);
    assert!(!fault.should_drop(endpoint_id, first_remote, now));
    assert!(!fault.should_drop(
        endpoint_id,
        replacement_remote,
        now + Duration::from_secs(1)
    ));
    assert!(fault.should_drop(endpoint_id, first_remote, now + Duration::from_millis(500)));

    fault.observe_selected_pair(
        endpoint_id,
        local,
        replacement_remote,
        now + Duration::from_secs(1),
    );
    assert!(fault.replacement_observed);
    assert!(!fault.should_drop(
        endpoint_id,
        replacement_remote,
        now + Duration::from_secs(1)
    ));
}

#[tokio::test]
async fn test_default_offer_does_not_advertise_legacy_ice_renomination() {
    let id = uuid::Uuid::new_v4();
    let bind_addr: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();
    let (tx, _rx) = mpsc::channel(16);

    let (_endpoint, offer) = WebRtcEndpoint::create_offer(
        id,
        EndpointDirection::SendRecv,
        &[bind_addr],
        tx,
        Arc::new(Metrics::new()),
    )
    .await
    .expect("default offer should be created");

    assert!(!offer.contains("renomination"));
}

#[tokio::test]
async fn test_outbound_timeline_source_change_preserves_continuity() {
    let mut ep = mk_webrtc_ts_endpoint().await;
    let real_src = EndpointId::new_v4();
    let hold_src = EndpointId::new_v4();

    ep.advance_outbound_timeline(real_src, 5_000, false);
    ep.advance_outbound_timeline(real_src, 5_960, false);
    let before_hold = ep.last_outbound_ts.unwrap();

    let (hold_ts, hold_marker) = ep.advance_outbound_timeline(hold_src, 9_000_000, false);
    assert_eq!(
        hold_ts,
        before_hold + 960,
        "source change must advance one WebRTC audio frame, not jump domains"
    );
    assert!(
        hold_marker,
        "source change must mark a new talk-spurt for the receiver"
    );

    let (hold_next, marker_next) = ep.advance_outbound_timeline(hold_src, 9_000_960, false);
    assert_eq!(
        hold_next,
        hold_ts + 960,
        "steady hold source should advance by source delta"
    );
    assert!(
        !marker_next,
        "steady same-source flow should not force marker"
    );

    let (resume_ts, resume_marker) = ep.advance_outbound_timeline(real_src, 6_920, false);
    assert_eq!(
        resume_ts,
        hold_next + 960,
        "resuming the original source must keep the destination timeline monotonic"
    );
    assert!(resume_marker, "resume source switch should force marker");
}

#[tokio::test]
async fn test_outbound_timeline_clamps_same_source_discontinuity() {
    let mut ep = mk_webrtc_ts_endpoint().await;
    let src = EndpointId::new_v4();

    ep.advance_outbound_timeline(src, 1_000, false);
    ep.advance_outbound_timeline(src, 1_960, false);
    let last_out = ep.last_outbound_ts.unwrap();

    let (ts, marker) = ep.advance_outbound_timeline(src, 1_960 + 100_000, false);
    assert_eq!(
        ts,
        last_out + 960,
        "huge same-source timestamp gaps should collapse to one frame"
    );
    assert!(marker, "same-source discontinuity should force marker");
}

#[tokio::test]
async fn test_outbound_timeline_reset_clears_source_state() {
    let mut ep = mk_webrtc_ts_endpoint().await;
    let src = EndpointId::new_v4();

    ep.advance_outbound_timeline(src, 1_000, false);
    ep.advance_outbound_timeline(src, 1_960, false);
    assert_eq!(ep.learned_step, Some(960));

    ep.reset_outbound_rtp_timeline();

    assert!(ep.last_outbound_ts.is_none());
    assert!(ep.last_source_id.is_none());
    assert!(ep.last_source_ts.is_none());
    assert!(ep.learned_step.is_none());
}

/// The recv task starts promptly after creation, signals liveness, and a
/// healthy task within the grace window is NOT flagged by the liveness sweep.
/// See docs/incident-research/webrtc-recv-task-wedge.md.
#[tokio::test]
async fn test_recv_task_starts_and_is_not_flagged() {
    let id = uuid::Uuid::new_v4();
    let bind_addr: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();
    let (tx, _rx) = mpsc::channel(16);
    let metrics = Arc::new(Metrics::new());

    let mut ep = WebRtcEndpoint::create_offer(
        id,
        EndpointDirection::SendRecv,
        &[bind_addr],
        tx,
        metrics.clone(),
    )
    .await
    .expect("create_offer should succeed")
    .0;

    // The recv task starts asynchronously; it should reach its loop promptly.
    for _ in 0..200 {
        if ep.recv_started.load(Ordering::Relaxed) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(
        ep.recv_started.load(Ordering::Relaxed),
        "recv task should start"
    );
    assert_eq!(metrics.webrtc_recv_task_started.get(), 1);

    // A started task is never flagged, and start_timeout stays zero.
    ep.supervise_recv();
    assert!(!ep.recv_dead_reported);
    assert_eq!(metrics.webrtc_recv_task_start_timeout.get(), 0);
}

/// The liveness sweep flags (and counts, once) a recv task that was spawned
/// but never reached its loop past the grace deadline — the receive-task
/// wedge, including the transfer-restart path. See
/// docs/incident-research/webrtc-recv-task-wedge.md.
#[tokio::test]
async fn test_supervise_recv_detects_never_started() {
    let id = uuid::Uuid::new_v4();
    let bind_addr: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();
    let metrics = Arc::new(Metrics::new());
    let mut ep = WebRtcEndpoint::new_with_socket(
        id,
        EndpointConfig {
            direction: EndpointDirection::SendRecv,
        },
        &[bind_addr],
        metrics.clone(),
        false,
    )
    .await
    .expect("new_with_socket should succeed");

    // Simulate a spawned-but-never-polled recv task: a live (unfinished) task
    // that never sets `recv_started`, with the grace deadline already past.
    ep.recv_task = Some(tokio::spawn(std::future::pending::<()>()));
    ep.recv_started.store(false, Ordering::Relaxed);
    ep.recv_start_deadline = Some(Instant::now() - Duration::from_secs(1));

    ep.supervise_recv();
    assert!(
        ep.recv_dead_reported,
        "a never-started task must be flagged"
    );
    assert_eq!(metrics.webrtc_recv_task_start_timeout.get(), 1);

    // Idempotent: a second sweep does not re-count.
    ep.supervise_recv();
    assert_eq!(metrics.webrtc_recv_task_start_timeout.get(), 1);
}

/// The session liveness sweep flags (once) a recv task that exited while its
/// endpoint is still active — the "task gone, endpoint live" blackhole.
#[tokio::test]
async fn test_supervise_recv_detects_dead_task() {
    let id = uuid::Uuid::new_v4();
    let bind_addr: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();
    let (tx, _rx) = mpsc::channel(16);
    let metrics = Arc::new(Metrics::new());

    let mut ep = WebRtcEndpoint::create_offer(
        id,
        EndpointDirection::SendRecv,
        &[bind_addr],
        tx,
        metrics.clone(),
    )
    .await
    .expect("create_offer should succeed")
    .0;

    // Force the recv task to exit while the endpoint stays in place.
    ep.cancel_token.cancel();
    for _ in 0..200 {
        if ep.recv_task.as_ref().unwrap().is_finished() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    assert!(!ep.recv_dead_reported);
    ep.supervise_recv();
    assert!(ep.recv_dead_reported, "a dead recv task must be reported");
    assert_eq!(metrics.webrtc_recv_task_exited.get(), 1);
    assert_eq!(metrics.webrtc_recv_task_dead.get(), 1);

    // Idempotent: a second sweep does not re-log.
    ep.supervise_recv();
    assert!(ep.recv_dead_reported);
    assert_eq!(metrics.webrtc_recv_task_dead.get(), 1);
}
use str0m::change::{SdpAnswer, SdpOffer};
use str0m::media::{Direction, MediaKind};

/// After a real offer/answer, `negotiated_codec()` reports the actually agreed
/// audio codec (Opus) read from str0m — not the old hardcoded opus/PT-111
/// literal. Exercises the answerer-supplied PT path via str0m's own answer.
#[tokio::test]
async fn test_negotiated_codec_reads_from_str0m() {
    let id = uuid::Uuid::new_v4();
    let bind_addr: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();
    let (tx, _rx) = mpsc::channel(16);
    let metrics = Arc::new(Metrics::new());

    let (mut ep, offer_sdp) =
        WebRtcEndpoint::create_offer(id, EndpointDirection::SendRecv, &[bind_addr], tx, metrics)
            .await
            .expect("create_offer should succeed");

    // Before any answer, nothing is negotiated yet.
    assert!(
        ep.negotiated_codec().is_none(),
        "no codec before the answer is applied"
    );

    // A bare str0m client accepts the offer and produces an answer; applying it
    // populates the media line's remote PTs.
    let mut client = RtcConfig::new().set_rtp_mode(true).build(Instant::now());
    let client_addr: std::net::SocketAddr = "127.0.0.1:40051".parse().unwrap();
    client.add_local_candidate(Candidate::host(client_addr, "udp").unwrap());
    let answer = client
        .sdp_api()
        .accept_offer(SdpOffer::from_sdp_string(&offer_sdp).unwrap())
        .unwrap();

    ep.accept_answer(&answer.to_sdp_string())
        .expect("accept_answer should succeed");

    let nc = ep
        .negotiated_codec()
        .expect("codec negotiated after answer");
    assert_eq!(nc.name, "opus", "str0m negotiates Opus for audio");
    assert_eq!(nc.clock_rate, 48000, "Opus RTP clock is 48 kHz");
    // PT comes from str0m's negotiation, not a hardcoded 111.
    assert!(nc.pt > 0, "a real dynamic PT was negotiated");
}

/// Diagnostic: verify str0m RTP mode media flow between two instances.
/// Server (ICE lite) creates offer → client accepts → ICE → server writes RTP → client receives.
#[test]
fn test_str0m_rtp_mode_media_exchange() {
    let server_addr: std::net::SocketAddr = "127.0.0.1:40000".parse().unwrap();
    let client_addr: std::net::SocketAddr = "127.0.0.1:40001".parse().unwrap();

    // Server: ICE lite + RTP mode (matches rtpbridge config)
    let mut server = RtcConfig::new()
        .set_ice_lite(true)
        .set_rtp_mode(true)
        .build(Instant::now());
    server.add_local_candidate(Candidate::host(server_addr, "udp").unwrap());

    let mut api = server.sdp_api();
    let offer_mid = api.add_media(MediaKind::Audio, Direction::SendRecv, None, None, None);
    let (offer, pending) = api.apply().unwrap();
    let offer_str = offer.to_sdp_string();
    // For the offer creator, MediaAdded doesn't fire — mid is known from add_media
    let server_mid: Option<Mid> = Some(offer_mid);

    // Client: RTP mode, not ICE lite
    let mut client = RtcConfig::new().set_rtp_mode(true).build(Instant::now());
    client.add_local_candidate(Candidate::host(client_addr, "udp").unwrap());

    let answer = client
        .sdp_api()
        .accept_offer(SdpOffer::from_sdp_string(&offer_str).unwrap())
        .unwrap();
    let answer_str = answer.to_sdp_string();

    server
        .sdp_api()
        .accept_answer(pending, SdpAnswer::from_sdp_string(&answer_str).unwrap())
        .unwrap();

    // Drive ICE: exchange STUN packets until connected
    let mut s2c: Vec<Vec<u8>> = Vec::new();
    let mut c2s: Vec<Vec<u8>> = Vec::new();
    let start = Instant::now();
    let mut ice_connected = false;
    let mut client_rtp_count = 0u32;
    let mut wrote_rtp = false;
    let mut write_errors: Vec<String> = Vec::new();

    for i in 0..500 {
        let now = start + std::time::Duration::from_millis(i * 10);

        // Drive server
        loop {
            match server.poll_output() {
                Ok(Output::Transmit(t)) => s2c.push(t.contents.to_vec()),
                Ok(Output::Event(e)) => {
                    if let Event::IceConnectionStateChange(IceConnectionState::Connected)
                    | Event::IceConnectionStateChange(IceConnectionState::Completed)
                    | Event::Connected = &e
                    {
                        ice_connected = true;
                    }
                    // MediaAdded doesn't fire for the offer creator;
                    // server_mid is set from add_media() above.
                }
                Ok(Output::Timeout(_)) => {
                    server.handle_input(Input::Timeout(now)).ok();
                    break;
                }
                Err(_) => break,
            }
        }

        // Drive client
        loop {
            match client.poll_output() {
                Ok(Output::Transmit(t)) => c2s.push(t.contents.to_vec()),
                Ok(Output::Event(e)) => {
                    if let Event::RtpPacket(_) = &e {
                        client_rtp_count += 1;
                    }
                }
                Ok(Output::Timeout(_)) => {
                    client.handle_input(Input::Timeout(now)).ok();
                    break;
                }
                Err(_) => break,
            }
        }

        // Deliver packets s2c
        for data in s2c.drain(..) {
            if let Ok(r) = Receive::new(Protocol::Udp, server_addr, client_addr, &data) {
                client.handle_input(Input::Receive(now, r)).ok();
            }
        }
        // Deliver packets c2s
        for data in c2s.drain(..) {
            if let Ok(r) = Receive::new(Protocol::Udp, client_addr, server_addr, &data) {
                server.handle_input(Input::Receive(now, r)).ok();
            }
        }

        // After ICE connects, write RTP from server to client
        if ice_connected && !wrote_rtp && server_mid.is_some() && i > 100 {
            wrote_rtp = true;
            let mid = server_mid.unwrap();
            for seq in 0..10u64 {
                let mut api = server.direct_api();
                match api.stream_tx_by_mid(mid, None) {
                    Some(stream) => {
                        stream.write_rtp(
                            str0m::rtp::RtpWrite::new(
                                111.into(),
                                seq.into(),
                                (seq as u32) * 960,
                                now,
                                vec![0x80u8; 160],
                            )
                            .marker(seq == 0),
                        );
                    }
                    None => {
                        write_errors.push(format!("seq {seq}: no stream_tx for mid {mid}"));
                    }
                }
            }
        }
    }

    eprintln!(
        "str0m diag: ice_connected={ice_connected}, server_mid={server_mid:?}, \
         wrote_rtp={wrote_rtp}, client_rtp_count={client_rtp_count}, \
         write_errors={write_errors:?}"
    );

    assert!(ice_connected, "ICE should connect");
    assert!(server_mid.is_some(), "server should have audio mid");
    assert!(wrote_rtp, "should have attempted write_rtp");

    if !write_errors.is_empty() {
        eprintln!("write_rtp errors (explains why client got 0 RTP): {write_errors:?}");
    }

    assert!(
        client_rtp_count > 0,
        "client should receive RTP packets written by server via str0m"
    );
}

/// Regression: create_offer with RecvOnly mixing direction must produce
/// sendrecv SDP and allow write_rtp (TX stream must exist for mix delivery).
#[tokio::test]
async fn test_create_offer_recvonly_produces_sendrecv_sdp() {
    let id = uuid::Uuid::new_v4();
    let bind_addr: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();
    let (tx, _rx) = mpsc::channel(16);

    let (ep, offer_sdp) = WebRtcEndpoint::create_offer(
        id,
        EndpointDirection::RecvOnly,
        &[bind_addr],
        tx,
        Arc::new(Metrics::new()),
    )
    .await
    .expect("create_offer should succeed");

    // SDP must be sendrecv — mixing direction is routing-table-only
    assert!(
        offer_sdp.contains("a=sendrecv"),
        "RecvOnly endpoint SDP must contain sendrecv, got:\n{offer_sdp}"
    );
    assert!(
        !offer_sdp.contains("a=recvonly"),
        "RecvOnly endpoint SDP must NOT contain recvonly"
    );

    // The endpoint's mixing direction is still RecvOnly
    assert_eq!(ep.config.direction, EndpointDirection::RecvOnly);

    // audio_mid must be set (TX stream exists)
    assert!(ep.audio_mid.is_some(), "audio_mid must be set");
}

/// Regression: create_offer with SendOnly mixing direction must also
/// produce sendrecv SDP (so the remote peer sends RTP that we can receive,
/// even though routing won't forward it to other endpoints).
#[tokio::test]
async fn test_create_offer_sendonly_produces_sendrecv_sdp() {
    let id = uuid::Uuid::new_v4();
    let bind_addr: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();
    let (tx, _rx) = mpsc::channel(16);

    let (_ep, offer_sdp) = WebRtcEndpoint::create_offer(
        id,
        EndpointDirection::SendOnly,
        &[bind_addr],
        tx,
        Arc::new(Metrics::new()),
    )
    .await
    .expect("create_offer should succeed");

    assert!(
        offer_sdp.contains("a=sendrecv"),
        "SendOnly endpoint SDP must contain sendrecv, got:\n{offer_sdp}"
    );
    assert!(
        !offer_sdp.contains("a=sendonly"),
        "SendOnly endpoint SDP must NOT contain sendonly"
    );
}

/// Regression: full end-to-end test that a RecvOnly mixing endpoint can
/// deliver RTP to the remote peer (spy/listen scenario).
#[test]
fn test_recvonly_endpoint_delivers_rtp_to_client() {
    let server_addr: std::net::SocketAddr = "127.0.0.1:40010".parse().unwrap();
    let client_addr: std::net::SocketAddr = "127.0.0.1:40011".parse().unwrap();

    let mut server = RtcConfig::new()
        .set_ice_lite(true)
        .set_rtp_mode(true)
        .build(Instant::now());
    server.add_local_candidate(Candidate::host(server_addr, "udp").unwrap());

    // After fix: create_offer always uses SendRecv for str0m
    let mut api = server.sdp_api();
    let mid = api.add_media(MediaKind::Audio, Direction::SendRecv, None, None, None);
    let (offer, pending) = api.apply().unwrap();
    let offer_str = offer.to_sdp_string();

    let mut client = RtcConfig::new().set_rtp_mode(true).build(Instant::now());
    client.add_local_candidate(Candidate::host(client_addr, "udp").unwrap());
    let answer = client
        .sdp_api()
        .accept_offer(SdpOffer::from_sdp_string(&offer_str).unwrap())
        .unwrap();
    server
        .sdp_api()
        .accept_answer(
            pending,
            SdpAnswer::from_sdp_string(&answer.to_sdp_string()).unwrap(),
        )
        .unwrap();

    // Drive ICE + deliver RTP from server to client
    let mut s2c: Vec<Vec<u8>> = Vec::new();
    let mut c2s: Vec<Vec<u8>> = Vec::new();
    let start = Instant::now();
    let mut ice_connected = false;
    let mut client_rtp_count = 0u32;
    let mut wrote_rtp = false;

    for i in 0..500 {
        let now = start + std::time::Duration::from_millis(i * 10);

        loop {
            match server.poll_output() {
                Ok(Output::Transmit(t)) => s2c.push(t.contents.to_vec()),
                Ok(Output::Event(e)) => {
                    if matches!(
                        &e,
                        Event::IceConnectionStateChange(IceConnectionState::Connected)
                            | Event::IceConnectionStateChange(IceConnectionState::Completed)
                            | Event::Connected
                    ) {
                        ice_connected = true;
                    }
                }
                Ok(Output::Timeout(_)) => {
                    server.handle_input(Input::Timeout(now)).ok();
                    break;
                }
                Err(_) => break,
            }
        }

        loop {
            match client.poll_output() {
                Ok(Output::Transmit(t)) => c2s.push(t.contents.to_vec()),
                Ok(Output::Event(e)) => {
                    if matches!(&e, Event::RtpPacket(_)) {
                        client_rtp_count += 1;
                    }
                }
                Ok(Output::Timeout(_)) => {
                    client.handle_input(Input::Timeout(now)).ok();
                    break;
                }
                Err(_) => break,
            }
        }

        for data in s2c.drain(..) {
            if let Ok(r) = Receive::new(Protocol::Udp, server_addr, client_addr, &data) {
                client.handle_input(Input::Receive(now, r)).ok();
            }
        }
        for data in c2s.drain(..) {
            if let Ok(r) = Receive::new(Protocol::Udp, client_addr, server_addr, &data) {
                server.handle_input(Input::Receive(now, r)).ok();
            }
        }

        if ice_connected && !wrote_rtp && i > 100 {
            wrote_rtp = true;
            for seq in 0..10u64 {
                let mut api = server.direct_api();
                let stream = api
                    .stream_tx_by_mid(mid, None)
                    .expect("TX stream must exist for recvonly mixing endpoint");
                stream.write_rtp(
                    str0m::rtp::RtpWrite::new(
                        111.into(),
                        seq.into(),
                        (seq as u32) * 960,
                        now,
                        vec![0x80u8; 160],
                    )
                    .marker(seq == 0),
                );
            }
        }
    }

    assert!(ice_connected, "ICE should connect");
    assert!(wrote_rtp, "should have written RTP");
    assert!(
        client_rtp_count > 0,
        "spy phone must receive RTP from the session mix"
    );
}

/// Regression: create_offer must NOT arm the connecting-watchdog. The
/// offer can sit indefinitely without a counter-answer (ring-no-answer,
/// caller hangup), and str0m's ICE agent will independently transition
/// New→Checking on its own timer. Both paths previously armed the
/// watchdog, causing false `webrtc_connecting_stuck` increments on
/// every unanswered call. Verifies the fix on both sites:
///   1. create_offer no longer calls mark_negotiation_started.
///   2. Event::IceConnectionStateChange(Checking) skips arming while
///      pending_offer.is_some().
#[tokio::test]
async fn test_create_offer_does_not_arm_watchdog_before_answer() {
    let id = uuid::Uuid::new_v4();
    let bind_addr: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();
    let (tx, _rx) = mpsc::channel(16);

    let (mut ep, _offer_sdp) = WebRtcEndpoint::create_offer(
        id,
        EndpointDirection::SendRecv,
        &[bind_addr],
        tx,
        Arc::new(Metrics::new()),
    )
    .await
    .expect("create_offer should succeed");

    assert!(
        ep.connecting_since.is_none(),
        "watchdog must not be armed at create_offer time"
    );
    assert!(
        ep.pending_offer.is_some(),
        "pending_offer must be Some while waiting for the answer"
    );

    // Drive str0m well past the point where its ICE agent would emit
    // Checking on its own (handle_timeout-driven New→Checking transition).
    // With pending_offer.is_some(), the Checking arm site must skip.
    let start = Instant::now();
    for i in 0..200 {
        let now = start + std::time::Duration::from_millis(i * 100);
        let _ = ep.handle_timeout(now);
        let _ = ep.poll_output();
    }

    assert!(
        ep.connecting_since.is_none(),
        "watchdog must NOT be armed by str0m's pre-answer Checking transition \
         while pending_offer is still Some (regression for unanswered-call \
         false positive)"
    );
}

#[tokio::test]
async fn test_ice_restart_rejected_while_offer_pending() {
    let id = uuid::Uuid::new_v4();
    let bind_addr: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();
    let (tx, _rx) = mpsc::channel(16);

    // create_offer leaves an unanswered pending offer.
    let (mut ep, _offer) = WebRtcEndpoint::create_offer(
        id,
        EndpointDirection::SendRecv,
        &[bind_addr],
        tx,
        Arc::new(Metrics::new()),
    )
    .await
    .expect("create_offer should succeed");
    assert!(ep.pending_offer.is_some());

    // A second outstanding offer would discard str0m's pending offer and
    // let a later answer apply against the wrong one — must be refused.
    let err = ep
        .ice_restart()
        .expect_err("ice_restart must be rejected while an offer is pending");
    assert!(
        err.to_string().contains("already pending"),
        "unexpected error: {err}"
    );
    assert!(
        ep.pending_offer.is_some(),
        "the rejected ice_restart must NOT have disturbed the existing pending offer"
    );
}

#[tokio::test]
async fn test_accept_answer_malformed_preserves_pending_offer() {
    let id = uuid::Uuid::new_v4();
    let bind_addr: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();
    let (tx, _rx) = mpsc::channel(16);

    let (mut ep, _offer) = WebRtcEndpoint::create_offer(
        id,
        EndpointDirection::SendRecv,
        &[bind_addr],
        tx,
        Arc::new(Metrics::new()),
    )
    .await
    .expect("create_offer should succeed");
    assert!(ep.pending_offer.is_some());

    // A malformed answer must be rejected BEFORE the pending offer is taken,
    // so a later well-formed retry can still complete the negotiation.
    let err = ep
        .accept_answer("this is not valid sdp")
        .expect_err("malformed answer must be rejected");
    assert!(
        err.to_string().contains("parse SDP answer"),
        "unexpected error: {err}"
    );
    assert!(
        ep.pending_offer.is_some(),
        "a malformed answer must NOT consume the pending offer"
    );
}

#[tokio::test]
async fn test_ice_restart_increments_and_returns_offer_generation() {
    let id = uuid::Uuid::new_v4();
    let bind_addr: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();
    let (tx, _rx) = mpsc::channel(16);

    let (mut ep, _offer) = WebRtcEndpoint::create_offer(
        id,
        EndpointDirection::SendRecv,
        &[bind_addr],
        tx,
        Arc::new(Metrics::new()),
    )
    .await
    .expect("create_offer should succeed");
    assert_eq!(ep.offer_generation, 0, "the initial offer is generation 0");

    // Simulate the initial answer clearing the pending offer.
    ep.pending_offer = None;
    let (_o1, g1) = ep.ice_restart().expect("first ice_restart");
    assert_eq!(g1, 1, "first ICE restart is generation 1");
    assert_eq!(ep.offer_generation, 1);

    // Simulate that restart being answered, then restart again.
    ep.pending_offer = None;
    let (_o2, g2) = ep.ice_restart().expect("second ice_restart");
    assert_eq!(g2, 2, "generation is monotonic across restarts");
}

#[test]
fn test_remote_dtls_fingerprint_from_sdp() {
    let sdp = "\
        v=0\r\n\
        o=- 1 1 IN IP4 127.0.0.1\r\n\
        s=-\r\n\
        t=0 0\r\n\
        a=fingerprint:sha-256 00:01:02:03:04:05:06:07:08:09:0A:0B:0C:0D:0E:0F:10:11:12:13:14:15:16:17:18:19:1A:1B:1C:1D:1E:1F\r\n";

    let fingerprint = remote_dtls_fingerprint_from_sdp(sdp).expect("fingerprint should parse");
    assert_eq!(
        fingerprint.to_string(),
        "sha-256 00:01:02:03:04:05:06:07:08:09:0A:0B:0C:0D:0E:0F:10:11:12:13:14:15:16:17:18:19:1A:1B:1C:1D:1E:1F"
    );

    let missing = remote_dtls_fingerprint_from_sdp("v=0\r\n").unwrap_err();
    assert!(
        missing
            .to_string()
            .contains("missing remote DTLS fingerprint"),
        "missing fingerprint should be explicit: {missing}"
    );
}

#[tokio::test]
async fn test_accept_answer_rejects_changed_remote_dtls_fingerprint() {
    let id = uuid::Uuid::new_v4();
    let bind_addr: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();
    let (tx, _rx) = mpsc::channel(16);

    let (mut ep, offer_sdp) = WebRtcEndpoint::create_offer(
        id,
        EndpointDirection::SendRecv,
        &[bind_addr],
        tx,
        Arc::new(Metrics::new()),
    )
    .await
    .expect("create_offer should succeed");

    let first_addr: std::net::SocketAddr = "127.0.0.1:41051".parse().unwrap();
    let mut first_peer = RtcConfig::new().set_rtp_mode(true).build(Instant::now());
    first_peer.add_local_candidate(Candidate::host(first_addr, "udp").unwrap());
    let first_answer = first_peer
        .sdp_api()
        .accept_offer(SdpOffer::from_sdp_string(&offer_sdp).unwrap())
        .unwrap()
        .to_sdp_string();
    let first_fingerprint =
        remote_dtls_fingerprint_from_sdp(&first_answer).expect("first fingerprint should parse");

    ep.accept_answer(&first_answer)
        .expect("initial answer should be accepted");
    assert_eq!(
        ep.remote_dtls_fingerprint.as_ref(),
        Some(&first_fingerprint)
    );

    let (restart_offer, _generation) = ep.ice_restart().expect("ice restart should start");

    let second_addr: std::net::SocketAddr = "127.0.0.1:41052".parse().unwrap();
    let mut fresh_peer = RtcConfig::new().set_rtp_mode(true).build(Instant::now());
    fresh_peer.add_local_candidate(Candidate::host(second_addr, "udp").unwrap());
    let fresh_answer = fresh_peer
        .sdp_api()
        .accept_offer(SdpOffer::from_sdp_string(&restart_offer).unwrap())
        .unwrap()
        .to_sdp_string();
    let fresh_fingerprint =
        remote_dtls_fingerprint_from_sdp(&fresh_answer).expect("fresh fingerprint should parse");
    assert_ne!(
        first_fingerprint, fresh_fingerprint,
        "a fresh peer connection should advertise a new DTLS identity"
    );

    let err = ep.accept_answer(&fresh_answer).unwrap_err();
    assert!(
        err.to_string()
            .contains("remote DTLS fingerprint changed on existing WebRTC endpoint"),
        "changed fingerprint should be rejected clearly: {err}"
    );
    assert!(
        ep.pending_offer.is_some(),
        "rejection must happen before consuming the pending restart offer"
    );
}

#[test]
fn ice_state_str_maps_all_variants() {
    assert_eq!(ice_state_str(IceConnectionState::New), "new");
    assert_eq!(ice_state_str(IceConnectionState::Checking), "checking");
    assert_eq!(ice_state_str(IceConnectionState::Connected), "connected");
    assert_eq!(ice_state_str(IceConnectionState::Completed), "completed");
    assert_eq!(
        ice_state_str(IceConnectionState::Disconnected),
        "disconnected"
    );
}

/// A fresh endpoint has no ICE state and a zeroed wire-level counter; once a
/// state is stored, the `Endpoint` accessors surface it (lowercased) for
/// the WebRTC variant.
#[tokio::test]
async fn ice_state_and_raw_recv_surface_through_endpoint_enum() {
    use crate::session::endpoint_enum::Endpoint;

    let id = uuid::Uuid::new_v4();
    let bind_addr: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();
    let (tx, _rx) = mpsc::channel(16);
    let metrics = Arc::new(Metrics::new());

    let mut ep =
        WebRtcEndpoint::create_offer(id, EndpointDirection::SendRecv, &[bind_addr], tx, metrics)
            .await
            .expect("create_offer should succeed")
            .0;

    assert!(
        ep.ice_connection_state.is_none(),
        "no ICE transition has happened yet"
    );
    // Simulate str0m reporting ICE consent loss.
    ep.ice_connection_state = Some(IceConnectionState::Disconnected);

    let wrapped = Endpoint::WebRtc(Box::new(ep));
    assert_eq!(wrapped.ice_state(), Some("disconnected"));
    // The wire-level counters exist for WebRTC and start at zero.
    assert_eq!(wrapped.raw_recv_packets(), Some(0));
    assert_eq!(wrapped.raw_recv_bytes(), Some(0));
}

#[tokio::test]
async fn test_create_offer_dual_stack_binds_both_families() {
    // Probe IPv6 loopback; skip on environments without IPv6 rather than
    // failing spuriously.
    if UdpSocket::bind("[::1]:0").await.is_err() {
        return;
    }
    let v4: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();
    let v6: std::net::SocketAddr = "[::1]:0".parse().unwrap();
    let (tx, _rx) = mpsc::channel(16);
    let metrics = Arc::new(Metrics::new());

    let (ep, offer) = WebRtcEndpoint::create_offer(
        EndpointId::new_v4(),
        EndpointDirection::SendRecv,
        &[v4, v6],
        tx,
        metrics,
    )
    .await
    .expect("dual-stack create_offer should succeed");

    // One socket per family, in the order supplied; one ICE host candidate
    // is registered per socket (add_host_candidates), so str0m offers both.
    assert_eq!(ep.sockets.len(), 2, "should bind one socket per family");
    assert!(ep.sockets[0].0.is_ipv4(), "first (primary) socket is IPv4");
    assert!(ep.sockets[1].0.is_ipv6(), "second socket is IPv6");
    assert_eq!(
        ep.local_addr, ep.sockets[0].0,
        "primary local_addr is the first bound socket"
    );
    assert_ne!(ep.sockets[0].0.port(), 0, "v4 socket got an OS port");
    assert_ne!(ep.sockets[1].0.port(), 0, "v6 socket got an OS port");
    assert!(
        offer.contains("m=audio"),
        "offer must contain an audio m-line"
    );
}

#[tokio::test]
async fn test_create_offer_single_family_binds_one_socket() {
    let v4: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();
    let (tx, _rx) = mpsc::channel(16);
    let metrics = Arc::new(Metrics::new());

    let (ep, _offer) = WebRtcEndpoint::create_offer(
        EndpointId::new_v4(),
        EndpointDirection::SendRecv,
        &[v4],
        tx,
        metrics,
    )
    .await
    .expect("single-family create_offer should succeed");

    assert_eq!(
        ep.sockets.len(),
        1,
        "single family binds exactly one socket"
    );
    assert_eq!(ep.local_addr, ep.sockets[0].0);
}

#[tokio::test]
async fn test_recording_addrs_picks_nominated_family() {
    if UdpSocket::bind("[::1]:0").await.is_err() {
        return; // no IPv6 loopback
    }
    let v4: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();
    let v6: std::net::SocketAddr = "[::1]:0".parse().unwrap();
    let (tx, _rx) = mpsc::channel(16);
    let metrics = Arc::new(Metrics::new());

    let (mut ep, _offer) = WebRtcEndpoint::create_offer(
        EndpointId::new_v4(),
        EndpointDirection::SendRecv,
        &[v4, v6],
        tx,
        metrics,
    )
    .await
    .expect("dual-stack create_offer should succeed");

    // No peer nominated yet → synthetic fallback (None, None).
    assert_eq!(ep.recording_addrs(), (None, None));

    // Nominated IPv6 peer → local must be the IPv6 socket, NOT the v4 primary.
    let peer6: std::net::SocketAddr = "[::1]:40000".parse().unwrap();
    ep.remote_addr = Some(peer6);
    let (local, remote) = ep.recording_addrs();
    assert_eq!(remote, Some(peer6));
    assert!(
        local.is_some_and(|l| l.is_ipv6()),
        "local must match the nominated IPv6 family, got {local:?}"
    );

    // Nominated IPv4 peer → local must be the IPv4 socket.
    let peer4: std::net::SocketAddr = "127.0.0.1:40000".parse().unwrap();
    ep.remote_addr = Some(peer4);
    let (local, _remote) = ep.recording_addrs();
    assert!(
        local.is_some_and(|l| l.is_ipv4()),
        "local must match the nominated IPv4 family, got {local:?}"
    );
}
