use super::*;

/// Create a minimal `SessionState` for unit testing command handlers.
/// Uses in-memory defaults — no real sockets or file caches needed.
fn test_session_state() -> SessionState {
    let (cmd_tx, _cmd_rx) = mpsc::channel(16);
    SessionState {
        session_id: SessionId::new_v4(),
        media_bindings: Arc::new(
            MediaBindings::new(&["127.0.0.1".parse().unwrap()], 50000, 50100).unwrap(),
        ),
        media_dir: None,
        file_cache: Arc::new(
            crate::playback::file_cache::FileCache::new(
                std::env::temp_dir().join("rtpbridge-test-cache"),
            )
            .unwrap(),
        ),
        endpoint_count: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        max_endpoints: 100,
        legacy_ice_renomination: false,
        metrics: Arc::new(crate::metrics::Metrics::new()),
        cmd_tx,
        event_tx: None,
        critical_event_tx: None,
        dropped_events: Arc::new(AtomicU64::new(0)),
        endpoints: HashMap::new(),
        dtmf_state: HashMap::new(),
        sensitive_dtmf_endpoints: HashSet::new(),
        routing: RoutingTable::new(),
        recording_mgr: RecordingManager::new(),
        vad_monitors: HashMap::new(),
        stats_interval: None,
        stats_include_diagnostics: false,
        last_stats_emit: Instant::now(),
        file_rtp_states: HashMap::new(),
        tone_rtp_states: HashMap::new(),
        transcode_cache: HashMap::new(),
        url_sources: HashMap::new(),
        fax_detectors: HashMap::new(),
        analysis_decoders: HashMap::new(),
        media_timeout_emitted: std::collections::HashSet::new(),
        dtmf_injection: None,
        last_timeout_check: Instant::now(),
        shared_playback: Arc::new(crate::playback::shared_playback::SharedPlaybackManager::new()),
        empty_since: None,
        mixers: HashMap::new(),
        playout_buffers: HashMap::new(),
        playout_policy: HashMap::new(),
        mix_grid: None,
        ws_audio_registry: Arc::new(crate::control::ws_audio::WsAudioRegistry::new()),
    }
}

/// Repro harness for the WS→PSTN stutter (call2.pcapng): drive the REAL `drive_grid` with
/// the captured inbound-WS arrival timeline, modeling the media loop's select/sleep wake
/// behavior — wake at `min(grid_instant, next_packet)`, batch-drain everything that has
/// arrived, then one `drive_grid` pass. A correctly grid-paced egress is ~20 ms between
/// frames with no sub-ms bursts, regardless of how bursty the inbound arrivals are.
#[test]
fn synth_grid_repaces_bursty_ws_inbound_from_pcap() {
    // Inter-arrival deltas (ms) of inbound 20 ms WS frames; index ~196 is a 4678.6 ms gap.
    let deltas_ms: &[f64] = &[
        0.4, 40.1, 21.1, 40.7, 40.2, 1.2, 1.2, 41.0, 1.1, 56.8, 27.9, 0.0, 30.4, 42.7, 0.1, 7.8,
        20.1, 29.2, 42.1, 0.0, 12.5, 19.5, 26.4, 42.1, 14.4, 0.1, 20.9, 22.4, 41.2, 19.3, 39.4,
        21.7, 0.0, 40.8, 37.4, 40.4, 1.6, 0.1, 5.2, 23.0, 29.6, 43.8, 0.1, 4.3, 21.6, 31.1, 41.2,
        0.1, 10.8, 21.2, 26.5, 41.1, 15.4, 0.1, 21.0, 22.0, 40.5, 21.5, 5.2, 19.6, 21.4, 41.4,
        29.8, 0.0, 18.7, 20.4, 41.5, 0.0, 20.4, 21.2, 40.9, 22.3, 35.4, 40.3, 1.4, 0.0, 7.5, 21.2,
        29.4, 42.7, 0.0, 10.4, 21.1, 25.8, 36.9, 0.0, 40.7, 0.1, 22.2, 40.4, 20.2, 0.0, 20.8, 19.5,
        40.5, 23.4, 0.0, 18.3, 19.4, 41.9, 40.4, 0.0, 1.5, 20.0, 41.0, 21.3, 31.4, 40.3, 1.3, 0.0,
        11.2, 21.1, 25.8, 36.6, 0.0, 41.7, 0.0, 20.2, 61.4, 0.0, 0.6, 20.6, 19.6, 40.9, 41.1, 0.1,
        5.0, 20.8, 42.2, 0.1, 20.5, 21.0, 20.4, 44.0, 39.0, 0.1, 44.8, 40.9, 21.3, 40.7, 18.1, 0.0,
        19.2, 20.3, 40.6, 0.8, 41.3, 0.2, 20.0, 40.8, 2.9, 17.5, 34.8, 41.5, 8.3, 0.1, 22.2, 28.1,
        40.4, 15.3, 21.8, 0.0, 21.0, 41.0, 40.7, 0.0, 20.6, 40.7, 41.0, 17.8, 41.2, 0.4, 0.1, 3.1,
        21.4, 61.4, 19.9, 0.0, 30.2, 40.5, 41.0, 4678.6, 22.2, 40.2, 15.4, 41.0, 0.0, 11.5, 40.0,
        17.2, 34.7, 41.5, 0.0, 7.3, 21.7, 29.5, 51.3, 0.0, 1.3, 21.3, 25.6, 41.8, 0.0, 15.5, 21.1,
        20.2, 40.8, 21.2, 0.1, 21.6, 20.4, 40.7, 56.7, 0.0, 40.3, 5.3, 42.1, 0.1, 1.3, 20.2, 41.0,
        21.5, 28.5, 41.1, 0.5, 0.0, 13.1, 21.3, 24.0, 40.1, 38.1, 2.1, 19.5, 60.7, 0.0, 29.4, 21.4,
        40.9, 35.3, 0.0, 12.1, 41.6, 0.0, 54.0, 0.1, 0.6, 20.9, 41.3, 45.0, 40.7, 5.3, 0.0, 40.7,
        51.2, 23.8, 0.0, 32.6, 40.4, 29.6, 0.0, 21.3, 20.5,
    ];

    let base = Instant::now();
    let mut arrivals = vec![base];
    let mut acc = 0.0f64;
    for &d in deltas_ms {
        acc += d;
        arrivals.push(base + Duration::from_micros((acc * 1000.0) as u64));
    }

    let src = EndpointId::new_v4();
    let mut buffers: HashMap<EndpointId, PlayoutBuffer> = HashMap::new();
    // 16 kHz wire rate (640-byte / 20 ms frames in the capture).
    buffers.insert(src, PlayoutBuffer::synth(src, 16_000, 1, 0, 0));
    let mut mix_grid: Option<Instant> = None;
    let frame = vec![0u8; 640];

    let mut out: Vec<Instant> = Vec::new();
    let mut now = base;
    let mut idx = 0usize;
    let deadline = *arrivals.last().unwrap() + Duration::from_secs(2);
    let mut guard = 0u64;
    loop {
        guard += 1;
        assert!(guard < 5_000_000, "loop runaway");
        let next_in = arrivals.get(idx).copied();
        let wake = match (mix_grid, next_in) {
            (Some(g), Some(ti)) => g.min(ti),
            (Some(g), None) => g,
            (None, Some(ti)) => ti,
            (None, None) => break,
        };
        if wake > now {
            now = wake;
        }
        // Batch-drain every packet that has arrived by `now` (select + try_recv loop).
        while matches!(arrivals.get(idx), Some(&ta) if ta <= now) {
            if let Some(buf) = buffers.get_mut(&src) {
                buf.push(
                    RoutedRtpPacket {
                        source_endpoint_id: src,
                        payload_type: 127,
                        sequence_number: 0,
                        timestamp: 0,
                        ssrc: 0,
                        marker: false,
                        payload: frame.clone(),
                    },
                    now,
                );
            }
            idx += 1;
        }
        let mut routed = Vec::new();
        drive_grid(&mut mix_grid, &mut buffers, &mut routed, now);
        for _ in &routed {
            out.push(now);
        }
        if idx >= arrivals.len() && mix_grid.is_none() {
            break;
        }
        assert!(now <= deadline, "did not converge");
    }

    // Analyze egress pacing. A 4.7 s input gap is legitimately DTX-collapsed (one long
    // output gap), so classify gaps > 200 ms separately and require the rest be ~20 ms.
    let n = out.len();
    assert!(n > 100, "expected a full egress stream, got {n}");
    let (mut zero_pairs, mut within, mut small, mut big) = (0usize, 0usize, 0usize, 0usize);
    for w in out.windows(2) {
        let d = w[1].duration_since(w[0]).as_secs_f64() * 1000.0;
        if d > 200.0 {
            big += 1;
            continue;
        }
        small += 1;
        if d < 1.0 {
            zero_pairs += 1;
        }
        if (15.0..=25.0).contains(&d) {
            within += 1;
        }
    }
    eprintln!(
        "egress frames={n} smooth(15-25ms)={within}/{small} bursts(<1ms)={zero_pairs} dtx_gaps={big}"
    );
    assert!(
        within as f64 / small as f64 > 0.9,
        "egress should be ~20 ms grid-paced, not arrival-clocked: only {within}/{small} \
         inter-frame gaps fell in 15-25 ms ({zero_pairs} sub-ms bursts)"
    );
    assert!(
        zero_pairs < small / 50,
        "egress is emitting catch-up bursts ({zero_pairs} sub-ms gaps) instead of pacing"
    );
}

#[derive(Clone)]
struct Str0mDatagram {
    source: std::net::SocketAddr,
    destination: std::net::SocketAddr,
    data: Vec<u8>,
}

fn poll_str0m_until_timeout(
    rtc: &mut str0m::Rtc,
    now: Instant,
    transmits: &mut Vec<Str0mDatagram>,
    connected: &mut bool,
) -> Vec<u16> {
    let mut rtp_sequences = Vec::new();
    loop {
        match rtc.poll_output() {
            Ok(str0m::Output::Transmit(t)) => transmits.push(Str0mDatagram {
                source: t.source,
                destination: t.destination,
                data: t.contents.to_vec(),
            }),
            Ok(str0m::Output::Event(event)) => match event {
                str0m::Event::Connected
                | str0m::Event::IceConnectionStateChange(
                    str0m::IceConnectionState::Connected | str0m::IceConnectionState::Completed,
                ) => {
                    *connected = true;
                }
                str0m::Event::RtpPacket(pkt) => {
                    rtp_sequences.push(pkt.header.sequence_number);
                }
                _ => {}
            },
            Ok(str0m::Output::Timeout(_)) => {
                let _ = rtc.handle_input(str0m::Input::Timeout(now));
                break;
            }
            Err(_) => break,
        }
    }
    rtp_sequences
}

fn deliver_str0m_datagrams(
    rtc: &mut str0m::Rtc,
    datagrams: impl IntoIterator<Item = Str0mDatagram>,
    now: Instant,
) {
    for datagram in datagrams {
        if let Ok(receive) = str0m::net::Receive::new(
            str0m::net::Protocol::Udp,
            datagram.source,
            datagram.destination,
            &datagram.data,
        ) {
            let _ = rtc.handle_input(str0m::Input::Receive(now, receive));
        }
    }
}

fn write_str0m_opus_packet(rtc: &mut str0m::Rtc, mid: str0m::media::Mid, seq: u64, now: Instant) {
    let mut api = rtc.direct_api();
    let stream = api
        .stream_tx_by_mid(mid, None)
        .expect("TX stream must exist for sendrecv audio");
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

fn wire_rtp_sequence(data: &[u8]) -> Option<u16> {
    if data.len() < 12 || data[0] >> 6 != 2 {
        return None;
    }
    let pt = data[1] & 0x7f;
    if (64..=95).contains(&pt) {
        return None;
    }
    Some(u16::from_be_bytes([data[2], data[3]]))
}

fn poll_until_rtp_datagrams(
    rtc: &mut str0m::Rtc,
    start: Instant,
    transmits: &mut Vec<Str0mDatagram>,
    connected: &mut bool,
    target_rtp_count: usize,
) {
    for tick in 0..250 {
        let now = start + Duration::from_millis(tick * 10);
        let _ = poll_str0m_until_timeout(rtc, now, transmits, connected);
        if transmits
            .iter()
            .filter(|datagram| wire_rtp_sequence(&datagram.data).is_some())
            .count()
            >= target_rtp_count
        {
            return;
        }
    }
}

/// Contract test for the WebRTC ingress bug fixed in the session loop.
///
/// str0m 0.21 RTP mode stores one pending RTP packet for the next
/// `poll_output()` instead of queueing all packets received since the prior
/// poll. If the bridge feeds two inbound RTP datagrams before polling, only
/// the newest packet is emitted. The media session therefore must drain
/// `poll_output()` immediately after each WebRTC `handle_receive()`.
#[test]
fn str0m_rtp_mode_requires_poll_after_each_inbound_rtp_packet() {
    use str0m::change::{SdpAnswer, SdpOffer};
    use str0m::media::{Direction, MediaKind};
    use str0m::{Candidate, RtcConfig};

    let server_addr: std::net::SocketAddr = "127.0.0.1:40100".parse().unwrap();
    let client_addr: std::net::SocketAddr = "127.0.0.1:40101".parse().unwrap();

    let mut server = RtcConfig::new()
        .set_ice_lite(true)
        .set_rtp_mode(true)
        .build(Instant::now());
    server.add_local_candidate(Candidate::host(server_addr, "udp").unwrap());

    let mut api = server.sdp_api();
    let mid = api.add_media(MediaKind::Audio, Direction::SendRecv, None, None, None);
    let (offer, pending) = api.apply().unwrap();

    let mut client = RtcConfig::new().set_rtp_mode(true).build(Instant::now());
    client.add_local_candidate(Candidate::host(client_addr, "udp").unwrap());

    let answer = client
        .sdp_api()
        .accept_offer(SdpOffer::from_sdp_string(&offer.to_sdp_string()).unwrap())
        .unwrap();
    server
        .sdp_api()
        .accept_answer(
            pending,
            SdpAnswer::from_sdp_string(&answer.to_sdp_string()).unwrap(),
        )
        .unwrap();

    let start = Instant::now();
    let mut s2c = Vec::new();
    let mut c2s = Vec::new();
    let mut connected = false;

    for tick in 0..250 {
        let now = start + Duration::from_millis(tick * 10);
        let _ = poll_str0m_until_timeout(&mut server, now, &mut s2c, &mut connected);
        let _ = poll_str0m_until_timeout(&mut client, now, &mut c2s, &mut connected);
        deliver_str0m_datagrams(&mut client, s2c.drain(..).collect::<Vec<_>>(), now);
        deliver_str0m_datagrams(&mut server, c2s.drain(..).collect::<Vec<_>>(), now);
        if connected && tick > 120 {
            break;
        }
    }
    assert!(connected, "ICE/DTLS should connect before RTP write");

    let now = start + Duration::from_secs(4);
    let mut batch = Vec::new();
    let mut ignored_connected = connected;
    for seq in 10..15 {
        write_str0m_opus_packet(&mut server, mid, seq, now);
    }
    poll_until_rtp_datagrams(&mut server, now, &mut batch, &mut ignored_connected, 2);
    let batch_rtp_sequences: Vec<u16> = batch
        .iter()
        .filter_map(|datagram| wire_rtp_sequence(&datagram.data))
        .collect();
    assert!(
        batch_rtp_sequences.len() >= 2,
        "sender should have emitted multiple RTP datagrams, got {batch_rtp_sequences:?}"
    );
    let expected_latest = *batch_rtp_sequences.last().unwrap();

    deliver_str0m_datagrams(&mut client, batch.clone(), now + Duration::from_millis(20));
    let received_without_drain = poll_str0m_until_timeout(
        &mut client,
        now + Duration::from_millis(20),
        &mut c2s,
        &mut ignored_connected,
    );
    assert_eq!(
        received_without_drain,
        vec![expected_latest],
        "without an intervening poll, str0m RTP mode emits only the latest packet"
    );

    let mut received_with_drain = Vec::new();
    for seq in [20_u64, 21] {
        let packet_time = now + Duration::from_millis(seq);
        let mut one_packet = Vec::new();
        write_str0m_opus_packet(&mut server, mid, seq, packet_time);
        poll_until_rtp_datagrams(
            &mut server,
            packet_time,
            &mut one_packet,
            &mut ignored_connected,
            1,
        );
        assert!(
            one_packet
                .iter()
                .any(|datagram| wire_rtp_sequence(&datagram.data) == Some(seq as u16)),
            "sender should have emitted RTP seq {seq}"
        );
        for datagram in one_packet {
            deliver_str0m_datagrams(&mut client, [datagram], packet_time);
            received_with_drain.extend(poll_str0m_until_timeout(
                &mut client,
                packet_time,
                &mut c2s,
                &mut ignored_connected,
            ));
        }
    }
    assert_eq!(
        received_with_drain,
        vec![20, 21],
        "polling after each inbound RTP datagram preserves every packet"
    );
}

#[test]
fn test_emit_event_none_channel_is_noop() {
    let tx: Option<mpsc::Sender<Event>> = None;
    let dropped = AtomicU64::new(0);
    let metrics = crate::metrics::Metrics::new();
    emit_event(
        &tx,
        "test.event",
        serde_json::json!({"key": "value"}),
        &dropped,
        &metrics,
    );
}

#[tokio::test]
async fn test_emit_event_full_channel_drops_without_panic() {
    let (tx, _rx) = mpsc::channel::<Event>(1);
    let tx = Some(tx);
    let dropped = AtomicU64::new(0);
    let metrics = crate::metrics::Metrics::new();
    emit_event(&tx, "first", serde_json::json!({}), &dropped, &metrics);
    // Should be dropped (channel full) but must NOT panic
    emit_event(&tx, "second", serde_json::json!({}), &dropped, &metrics);
    emit_event(&tx, "third", serde_json::json!({}), &dropped, &metrics);
    assert_eq!(
        dropped.load(Ordering::Relaxed),
        2,
        "two events should have been dropped"
    );
}

// ── SessionState handler tests ──────────────────────────────────

#[test]
fn test_get_info_empty_session() {
    let state = test_session_state();
    let info = state.get_info();
    assert!(info.endpoints.is_empty());
    assert!(info.recordings.is_empty());
    assert!(info.vad_active.is_empty());
}

#[test]
fn test_handle_accept_answer_not_found() {
    let mut state = test_session_state();
    let result = state.handle_accept_answer(EndpointId::new_v4(), "v=0\r\n", None, None);
    assert!(result.is_err());
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("Endpoint not found")
    );
}

#[test]
fn test_handle_accept_offer_not_found() {
    let mut state = test_session_state();
    let result = state.handle_accept_offer(EndpointId::new_v4(), "v=0\r\n");
    assert!(result.is_err());
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("Endpoint not found")
    );
}

#[test]
fn test_handle_update_remote_sdp_not_found() {
    let mut state = test_session_state();
    let result = state.handle_update_remote_sdp(EndpointId::new_v4(), "v=0\r\n");
    assert!(result.is_err());
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("Endpoint not found")
    );
}

#[test]
fn test_handle_vad_start_not_found() {
    let mut state = test_session_state();
    let result = state.handle_vad_start(EndpointId::new_v4(), 500, 0.5);
    assert!(result.is_err());
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("Endpoint not found")
    );
}

#[test]
fn test_handle_vad_stop_not_active() {
    let mut state = test_session_state();
    let result = state.handle_vad_stop(EndpointId::new_v4());
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("VAD not active"));
}

#[test]
fn test_vad_stop_prunes_shared_decoder_when_no_fax() {
    // VAD is the only analyser → stopping it must drop the shared decoder so
    // a later vad.start gets a fresh (non-stale) stateful decoder.
    let mut state = test_session_state();
    let eid = EndpointId::new_v4();
    state
        .vad_monitors
        .insert(eid, VadMonitor::new(16000, 0.5, 1000));
    state.analysis_decoders.insert(
        eid,
        crate::media::codec::make_decoder(AudioCodec::G722).unwrap(),
    );

    state.handle_vad_stop(eid).unwrap();
    assert!(
        !state.analysis_decoders.contains_key(&eid),
        "decoder should be pruned when no analyser remains"
    );
}

#[test]
fn test_vad_stop_keeps_shared_decoder_when_fax_active() {
    // Fax detection still active → the decoder is still being fed, so it
    // must be retained when VAD stops.
    let mut state = test_session_state();
    let eid = EndpointId::new_v4();
    state
        .vad_monitors
        .insert(eid, VadMonitor::new(16000, 0.5, 1000));
    state.fax_detectors.insert(eid, FaxDetector::new(16000));
    state.analysis_decoders.insert(
        eid,
        crate::media::codec::make_decoder(AudioCodec::G722).unwrap(),
    );

    state.handle_vad_stop(eid).unwrap();
    assert!(
        state.analysis_decoders.contains_key(&eid),
        "decoder must be retained while fax detection is still active"
    );

    // Stopping fax too now prunes it.
    state.handle_fax_detect_stop(eid).unwrap();
    assert!(
        !state.analysis_decoders.contains_key(&eid),
        "decoder should be pruned once the last analyser stops"
    );
}

#[test]
fn test_handle_file_seek_not_found() {
    let mut state = test_session_state();
    let result = state.handle_file_seek(EndpointId::new_v4(), 1000);
    assert!(result.is_err());
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("Endpoint not found")
    );
}

#[test]
fn test_handle_file_pause_not_found() {
    let mut state = test_session_state();
    let result = state.handle_file_pause(EndpointId::new_v4());
    assert!(result.is_err());
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("Endpoint not found")
    );
}

#[test]
fn test_handle_file_resume_not_found() {
    let mut state = test_session_state();
    let result = state.handle_file_resume(EndpointId::new_v4());
    assert!(result.is_err());
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("Endpoint not found")
    );
}

#[test]
fn test_handle_ice_restart_not_found() {
    let mut state = test_session_state();
    let result = state.handle_ice_restart(EndpointId::new_v4());
    assert!(result.is_err());
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("Endpoint not found")
    );
}

#[test]
fn test_handle_srtp_rekey_not_found() {
    let mut state = test_session_state();
    let result = state.handle_srtp_rekey(EndpointId::new_v4());
    assert!(result.is_err());
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("Endpoint not found")
    );
}

#[tokio::test]
async fn test_handle_remove_endpoint_not_found() {
    let mut state = test_session_state();
    let result = state.handle_remove_endpoint(EndpointId::new_v4()).await;
    assert!(result.is_err());
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("Endpoint not found")
    );
}

#[tokio::test]
async fn test_cleanup_endpoint_state_removes_all_ancillary() {
    let mut state = test_session_state();
    let eid = EndpointId::new_v4();

    // Populate all ancillary state maps for this endpoint
    state.dtmf_state.insert(
        eid,
        EndpointDtmf {
            detector: DtmfDetector::new(),
            te_pt: Some(101),
        },
    );
    state
        .vad_monitors
        .insert(eid, VadMonitor::new(8000, 0.5, 500));
    state.file_rtp_states.insert(
        eid,
        FileRtpState {
            seq_no: 0,
            timestamp: 0,
            ssrc: 0,
            last_poll: Instant::now(),
        },
    );
    state
        .url_sources
        .insert(eid, "https://example.com/test.wav".to_string());

    // Add a dummy transcode cache entry involving this endpoint
    let other_eid = EndpointId::new_v4();
    state.transcode_cache.insert(
        (eid, other_eid),
        CachedTranscode {
            pipeline: TranscodePipeline::new(AudioCodec::Pcmu, AudioCodec::G722).unwrap(),
            last_used: Instant::now(),
        },
    );

    state.cleanup_endpoint_state(eid).await;

    assert!(!state.dtmf_state.contains_key(&eid));
    assert!(!state.vad_monitors.contains_key(&eid));
    assert!(!state.file_rtp_states.contains_key(&eid));
    assert!(!state.url_sources.contains_key(&eid));
    assert!(
        !state.transcode_cache.contains_key(&(eid, other_eid)),
        "transcode cache entry involving removed endpoint should be cleaned"
    );
}

#[test]
fn test_rebuild_routing_updates_endpoint_count() {
    let mut state = test_session_state();
    assert_eq!(
        state
            .endpoint_count
            .load(std::sync::atomic::Ordering::Relaxed),
        0
    );

    // Insert a file endpoint directly (must be in Playing state to be routable)
    let id = EndpointId::new_v4();
    let mut ep = FileEndpoint::new_buffering(id, 0.0);
    ep.state = EndpointState::Playing;
    state.endpoints.insert(id, Endpoint::File(Box::new(ep)));
    state.rebuild_routing();

    assert_eq!(
        state
            .endpoint_count
            .load(std::sync::atomic::Ordering::Relaxed),
        1
    );
}

#[tokio::test]
async fn test_handle_command_destroy_returns_false() {
    let mut state = test_session_state();
    let (packet_tx, _packet_rx) = mpsc::channel(16);
    let cont = state
        .handle_command(SessionCommand::Destroy, &packet_tx)
        .await;
    assert!(
        !cont,
        "Destroy command should return false to break the loop"
    );
}

#[tokio::test]
async fn test_handle_command_attach_detach() {
    let mut state = test_session_state();
    let (packet_tx, _packet_rx) = mpsc::channel(16);
    let (event_tx, _event_rx) = mpsc::channel(16);

    assert!(state.event_tx.is_none());

    let (critical_tx, _critical_rx) = mpsc::channel(16);
    let cont = state
        .handle_command(
            SessionCommand::Attach {
                event_tx,
                critical_event_tx: critical_tx,
                dropped_events: Arc::new(AtomicU64::new(0)),
            },
            &packet_tx,
        )
        .await;
    assert!(cont);
    assert!(state.event_tx.is_some());

    let cont = state
        .handle_command(SessionCommand::Detach, &packet_tx)
        .await;
    assert!(cont);
    assert!(state.event_tx.is_none());
}

#[tokio::test]
async fn test_handle_command_stats_subscribe_unsubscribe() {
    let mut state = test_session_state();
    let (packet_tx, _packet_rx) = mpsc::channel(16);

    assert!(state.stats_interval.is_none());

    let (reply_tx, reply_rx) = oneshot::channel();
    state
        .handle_command(
            SessionCommand::StatsSubscribe {
                reply: reply_tx,
                interval_ms: 5000,
                include_diagnostics: false,
            },
            &packet_tx,
        )
        .await;
    assert!(reply_rx.await.unwrap().is_ok());
    assert_eq!(state.stats_interval, Some(Duration::from_millis(5000)));
    assert!(!state.stats_include_diagnostics);

    let (reply_tx, reply_rx) = oneshot::channel();
    state
        .handle_command(
            SessionCommand::StatsUnsubscribe { reply: reply_tx },
            &packet_tx,
        )
        .await;
    assert!(reply_rx.await.unwrap().is_ok());
    assert!(state.stats_interval.is_none());
    assert!(!state.stats_include_diagnostics);
}

#[tokio::test]
async fn test_stats_resubscribe_preserves_emit_anchor() {
    let mut state = test_session_state();
    let (packet_tx, _packet_rx) = mpsc::channel(16);

    // First subscribe anchors the emit timeline to ~now.
    let (reply_tx, reply_rx) = oneshot::channel();
    state
        .handle_command(
            SessionCommand::StatsSubscribe {
                reply: reply_tx,
                interval_ms: 5000,
                include_diagnostics: false,
            },
            &packet_tx,
        )
        .await;
    assert!(reply_rx.await.unwrap().is_ok());

    // Pretend a stats event fired 2s ago.
    let anchor = Instant::now() - Duration::from_secs(2);
    state.last_stats_emit = anchor;

    // Re-subscribe with a new interval: the anchor must be preserved so the
    // next fire is `interval - elapsed` from now, not reset to a fresh
    // full interval.
    let (reply_tx, reply_rx) = oneshot::channel();
    state
        .handle_command(
            SessionCommand::StatsSubscribe {
                reply: reply_tx,
                interval_ms: 10000,
                include_diagnostics: true,
            },
            &packet_tx,
        )
        .await;
    assert!(reply_rx.await.unwrap().is_ok());
    assert_eq!(state.stats_interval, Some(Duration::from_millis(10000)));
    assert!(state.stats_include_diagnostics);
    assert_eq!(
        state.last_stats_emit, anchor,
        "re-subscribe must not re-anchor the emit timeline"
    );
}

#[test]
fn test_check_media_timeouts_emits_once() {
    // Verify that the media_timeout_emitted set prevents duplicate emissions
    let mut emitted = std::collections::HashSet::new();

    // Test the emitted set behavior directly
    let eid = EndpointId::new_v4();
    assert!(emitted.insert(eid), "first insert should succeed");
    assert!(
        !emitted.insert(eid),
        "second insert should return false (already present)"
    );
    emitted.remove(&eid);
    assert!(
        emitted.insert(eid),
        "after remove, insert should succeed again"
    );
}

#[tokio::test]
async fn test_cleanup_endpoint_state_removes_analysis_decoder() {
    let mut state = test_session_state();
    let eid = EndpointId::new_v4();

    // Add a shared analysis decoder
    state.analysis_decoders.insert(
        eid,
        crate::media::codec::make_decoder(AudioCodec::Pcmu).unwrap(),
    );
    assert!(state.analysis_decoders.contains_key(&eid));

    state.cleanup_endpoint_state(eid).await;
    assert!(
        !state.analysis_decoders.contains_key(&eid),
        "cleanup should remove the shared analysis decoder"
    );
}

#[tokio::test]
async fn test_create_with_file_passes_cache_params() {
    let mut state = test_session_state();
    let (packet_tx, _packet_rx) = mpsc::channel(16);

    // Try creating a file endpoint with a non-existent local file to verify
    // the params are threaded through (will fail because no media_dir, but that's ok)
    let (reply_tx, reply_rx) = oneshot::channel();
    state
        .handle_command(
            SessionCommand::CreateWithFile {
                reply: reply_tx,
                source: "/nonexistent/test.wav".to_string(),
                start_ms: 0,
                loop_count: None,
                cache_ttl_secs: 600,
                timeout_ms: 15000,
                shared: false,
                headers: None,
                gain_db: 0.0,
            },
            &packet_tx,
        )
        .await;
    let result = reply_rx.await.unwrap();
    // Should fail because media_dir is None for local files
    assert!(result.is_err());
}

// ── Dynamic PT codec resolution tests ───────────────────────────

#[test]
fn test_endpoint_audio_codec_resolves_non_standard_opus_pt() {
    // An RTP endpoint negotiated with Opus at PT 96 (not the default 111).
    // endpoint_audio_codec should still resolve to Opus via codec name,
    // whereas the old AudioCodec::from_pt(96) would return None.

    // Test the resolution functions directly on the SdpCodec name.
    let opus_pt96 = sdp::SdpCodec {
        pt: 96,
        name: "opus",
        clock_rate: 48000,
        channels: Some(2),
        fmtp: None,
    };

    // Old approach: from_pt(96) → None (broken for dynamic PTs)
    assert!(
        AudioCodec::from_pt(96).is_none(),
        "from_pt(96) should return None for non-standard PT"
    );

    // New approach: from_name resolves correctly
    assert_eq!(
        AudioCodec::from_name(opus_pt96.name),
        Some(AudioCodec::Opus),
        "from_name should resolve 'opus' regardless of PT number"
    );
}

#[test]
fn test_endpoint_audio_codec_standard_pts_still_work() {
    // Verify standard PTs resolve through both paths
    assert_eq!(AudioCodec::from_name("PCMU"), Some(AudioCodec::Pcmu));
    assert_eq!(AudioCodec::from_name("G722"), Some(AudioCodec::G722));
    assert_eq!(AudioCodec::from_name("opus"), Some(AudioCodec::Opus));
}

#[tokio::test]
async fn test_endpoint_audio_codec_on_rtp_endpoint_with_dynamic_pt() {
    // End-to-end: create an RTP endpoint from an SDP that uses PT 96 for Opus,
    // then verify endpoint_audio_codec resolves correctly.
    let pool = crate::net::socket_pool::SocketPool::new("127.0.0.1".parse().unwrap(), 52100, 52200)
        .unwrap();
    let pair = pool.allocate_pair().await.unwrap();
    let (tx, _rx) = mpsc::channel(16);

    let sdp = "v=0\r\n\
        o=- 1 1 IN IP4 10.0.0.1\r\n\
        s=-\r\n\
        c=IN IP4 10.0.0.1\r\n\
        t=0 0\r\n\
        m=audio 20000 RTP/AVP 96 101\r\n\
        a=rtpmap:96 opus/48000/2\r\n\
        a=rtpmap:101 telephone-event/8000\r\n\
        a=sendrecv\r\n";

    let (ep, _answer) = RtpEndpoint::from_offer(
        EndpointId::new_v4(),
        EndpointDirection::SendRecv,
        sdp,
        pair,
        "127.0.0.1".parse().unwrap(),
        tx,
    )
    .unwrap();

    // The endpoint should have negotiated Opus at PT 96
    assert_eq!(ep.send_codec.as_ref().unwrap().pt, 96);
    assert_eq!(ep.send_codec.as_ref().unwrap().name, "opus");

    // Wrap in Endpoint and test resolution
    let wrapped = Endpoint::Rtp(Box::new(ep));
    assert_eq!(
        endpoint_audio_codec(&wrapped),
        Some(AudioCodec::Opus),
        "endpoint_audio_codec should resolve Opus even at non-standard PT 96"
    );
    assert_eq!(
        endpoint_send_pt(&wrapped),
        Some(96),
        "endpoint_send_pt should return the negotiated PT 96, not hardcoded 111"
    );
}

// ── DTMF non-blocking injection tests ───────────────────────────

#[tokio::test]
async fn test_dtmf_inject_queues_packets_non_blocking() {
    let mut state = test_session_state();
    let pool = crate::net::socket_pool::SocketPool::new("127.0.0.1".parse().unwrap(), 52200, 52300)
        .unwrap();
    let pair = pool.allocate_pair().await.unwrap();
    let (tx, _rx) = mpsc::channel(16);

    let sdp = "v=0\r\n\
        o=- 1 1 IN IP4 10.0.0.1\r\n\
        s=-\r\n\
        c=IN IP4 10.0.0.1\r\n\
        t=0 0\r\n\
        m=audio 20000 RTP/AVP 0 101\r\n\
        a=rtpmap:0 PCMU/8000\r\n\
        a=rtpmap:101 telephone-event/8000\r\n\
        a=sendrecv\r\n";

    let (ep, _answer) = RtpEndpoint::from_offer(
        EndpointId::new_v4(),
        EndpointDirection::SendRecv,
        sdp,
        pair,
        "127.0.0.1".parse().unwrap(),
        tx,
    )
    .unwrap();

    let eid = ep.id;
    state.endpoints.insert(eid, Endpoint::Rtp(Box::new(ep)));
    state.dtmf_state.insert(
        eid,
        EndpointDtmf {
            detector: DtmfDetector::new(),
            te_pt: Some(101),
        },
    );

    // Inject should return immediately (non-blocking)
    let before = Instant::now();
    let result = state.handle_dtmf_inject(&eid, '5', 200, 10);
    let elapsed = before.elapsed();

    assert!(result.is_ok(), "DTMF inject should succeed");
    assert!(
        elapsed < Duration::from_millis(50),
        "DTMF inject should return immediately, took {:?}",
        elapsed
    );

    // Should have queued packets
    let inj = state.dtmf_injection.as_ref().unwrap();
    assert_eq!(inj.endpoint_id, eid);
    assert!(!inj.packets.is_empty(), "should have queued DTMF packets");
    assert_eq!(inj.next_index, 0, "no packets sent yet");

    // All packets should have PT = 101 (telephone-event)
    for pkt in &inj.packets {
        assert_eq!(pkt.payload_type, 101);
    }
}

#[tokio::test]
async fn test_dtmf_inject_rejects_concurrent() {
    let mut state = test_session_state();
    let pool = crate::net::socket_pool::SocketPool::new("127.0.0.1".parse().unwrap(), 52300, 52400)
        .unwrap();
    let pair = pool.allocate_pair().await.unwrap();
    let (tx, _rx) = mpsc::channel(16);

    let sdp = "v=0\r\n\
        o=- 1 1 IN IP4 10.0.0.1\r\n\
        s=-\r\n\
        c=IN IP4 10.0.0.1\r\n\
        t=0 0\r\n\
        m=audio 20000 RTP/AVP 0 101\r\n\
        a=rtpmap:0 PCMU/8000\r\n\
        a=rtpmap:101 telephone-event/8000\r\n\
        a=sendrecv\r\n";

    let (ep, _) = RtpEndpoint::from_offer(
        EndpointId::new_v4(),
        EndpointDirection::SendRecv,
        sdp,
        pair,
        "127.0.0.1".parse().unwrap(),
        tx,
    )
    .unwrap();

    let eid = ep.id;
    state.endpoints.insert(eid, Endpoint::Rtp(Box::new(ep)));
    state.dtmf_state.insert(
        eid,
        EndpointDtmf {
            detector: DtmfDetector::new(),
            te_pt: Some(101),
        },
    );

    // First injection should succeed
    assert!(state.handle_dtmf_inject(&eid, '1', 100, 10).is_ok());

    // Second injection while first is pending should fail
    let result = state.handle_dtmf_inject(&eid, '2', 100, 10);
    assert!(result.is_err());
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("already in progress")
    );
}

#[test]
fn test_dtmf_inject_file_endpoint_rejected() {
    let mut state = test_session_state();
    let eid = EndpointId::new_v4();
    let ep = FileEndpoint::new_buffering(eid, 0.0);
    state.endpoints.insert(eid, Endpoint::File(Box::new(ep)));

    let result = state.handle_dtmf_inject(&eid, '5', 200, 10);
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("file endpoint"));
}

#[test]
fn test_sensitive_dtmf_mode_requires_an_existing_endpoint() {
    let mut state = test_session_state();
    let result = state.handle_dtmf_set_sensitive(EndpointId::new_v4(), true);
    assert!(result.is_err());
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("Endpoint not found")
    );
}

#[test]
fn test_sensitive_dtmf_packets_are_omitted_from_recordings_only_while_enabled() {
    let endpoint_id = EndpointId::new_v4();
    let packet = RoutedRtpPacket {
        source_endpoint_id: endpoint_id,
        payload_type: 101,
        sequence_number: 1,
        timestamp: 2,
        ssrc: 3,
        marker: true,
        payload: vec![1, 2, 3, 4],
    };
    let dtmf_state = HashMap::from([(
        endpoint_id,
        EndpointDtmf {
            detector: DtmfDetector::new(),
            te_pt: Some(101),
        },
    )]);
    let mut sensitive = HashSet::new();

    assert!(should_record_inbound(&packet, &dtmf_state, &sensitive));
    sensitive.insert(endpoint_id);
    assert!(!should_record_inbound(&packet, &dtmf_state, &sensitive));

    let audio_packet = RoutedRtpPacket {
        payload_type: 0,
        ..packet
    };
    assert!(should_record_inbound(
        &audio_packet,
        &dtmf_state,
        &sensitive
    ));
}

// ── URL file-cache cleanup on destroy ───────────────────────────

#[tokio::test]
async fn test_url_sources_drained_on_cleanup() {
    let mut state = test_session_state();
    let eid1 = EndpointId::new_v4();
    let eid2 = EndpointId::new_v4();

    state
        .url_sources
        .insert(eid1, "https://example.com/a.wav".to_string());
    state
        .url_sources
        .insert(eid2, "https://example.com/b.wav".to_string());

    // Simulate what the session shutdown code does
    for (_eid, url) in state.url_sources.drain() {
        state.file_cache.release(&url).await;
    }

    assert!(
        state.url_sources.is_empty(),
        "url_sources should be empty after drain"
    );
}

// ── handle_inbound_packet RTCP classification ───────────────────

#[tokio::test]
async fn test_inbound_rtcp_classified_by_is_rtcp_flag() {
    let mut endpoints = HashMap::new();
    let pool = crate::net::socket_pool::SocketPool::new("127.0.0.1".parse().unwrap(), 52400, 52500)
        .unwrap();
    let pair = pool.allocate_pair().await.unwrap();
    let (tx, _rx) = mpsc::channel(16);

    let sdp = "v=0\r\n\
        o=- 1 1 IN IP4 10.0.0.1\r\n\
        s=-\r\n\
        c=IN IP4 10.0.0.1\r\n\
        t=0 0\r\n\
        m=audio 20000 RTP/AVP 0\r\n\
        a=rtpmap:0 PCMU/8000\r\n\
        a=sendrecv\r\n";

    let (ep, _) = RtpEndpoint::from_offer(
        EndpointId::new_v4(),
        EndpointDirection::SendRecv,
        sdp,
        pair,
        "127.0.0.1".parse().unwrap(),
        tx,
    )
    .unwrap();

    let eid = ep.id;
    endpoints.insert(eid, Endpoint::Rtp(Box::new(ep)));

    // Build a minimal RTCP SR packet (PT = 200)
    let mut rtcp_data = vec![0x80u8, 200, 0x00, 0x06];
    rtcp_data.extend_from_slice(&[0u8; 24]); // SR body

    let pkt = InboundPacket {
        endpoint_id: eid,
        source: "10.0.0.1:20001".parse().unwrap(),
        data: rtcp_data,
        recv_at: Instant::now(),
        is_rtcp: true,
        local: None,
    };

    let (rtp, rtcp, _bye) =
        handle_inbound_packet(&mut endpoints, &pkt, &crate::metrics::Metrics::new());
    assert!(rtp.is_none(), "RTCP packet should not produce routed RTP");
    assert!(
        rtcp.is_some(),
        "RTCP packet should return bytes for recording tap"
    );
}

#[tokio::test]
async fn test_inbound_rtp_not_misclassified() {
    let mut endpoints = HashMap::new();
    let pool = crate::net::socket_pool::SocketPool::new("127.0.0.1".parse().unwrap(), 52500, 52600)
        .unwrap();
    let pair = pool.allocate_pair().await.unwrap();
    let (tx, _rx) = mpsc::channel(16);

    let sdp = "v=0\r\n\
        o=- 1 1 IN IP4 10.0.0.1\r\n\
        s=-\r\n\
        c=IN IP4 10.0.0.1\r\n\
        t=0 0\r\n\
        m=audio 20000 RTP/AVP 0\r\n\
        a=rtpmap:0 PCMU/8000\r\n\
        a=sendrecv\r\n";

    let (ep, _) = RtpEndpoint::from_offer(
        EndpointId::new_v4(),
        EndpointDirection::SendRecv,
        sdp,
        pair,
        "127.0.0.1".parse().unwrap(),
        tx,
    )
    .unwrap();

    let eid = ep.id;
    endpoints.insert(eid, Endpoint::Rtp(Box::new(ep)));

    // Build a valid RTP PCMU packet (PT=0, V=2)
    let rtp_data = crate::media::rtp::RtpHeader::build(0, 1, 160, 12345, false, &vec![0x80u8; 160]);

    let pkt = InboundPacket {
        endpoint_id: eid,
        source: "10.0.0.1:20000".parse().unwrap(),
        data: rtp_data,
        recv_at: Instant::now(),
        is_rtcp: false,
        local: None,
    };

    let (rtp, rtcp, _bye) =
        handle_inbound_packet(&mut endpoints, &pkt, &crate::metrics::Metrics::new());
    assert!(rtp.is_some(), "RTP packet should produce a routed packet");
    assert!(
        rtcp.is_none(),
        "RTP packet should not produce RTCP recording"
    );
}

/// Minimal plain-RTP offer for a given connection family.
fn rtp_family_offer(is_v6: bool) -> String {
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
         m=audio 20000 RTP/AVP 0 101\r\n\
         a=rtpmap:0 PCMU/8000\r\n\
         a=rtpmap:101 telephone-event/8000\r\n\
         a=sendrecv\r\n"
    )
}

#[tokio::test]
async fn test_create_from_offer_rejects_unbound_address_family() {
    // The default test session binds only IPv4. A plain-RTP offer with an
    // IPv6 c= line must be rejected, not answered with an unreachable IPv4
    // address.
    let mut state = test_session_state();
    let (tx, _rx) = mpsc::channel(16);

    let err = state
        .handle_create_from_offer(
            &tx,
            &rtp_family_offer(true),
            EndpointDirection::SendRecv,
            None,
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("IPv6"), "unexpected error: {err}");
}

#[tokio::test]
async fn test_create_from_offer_dual_stack_answers_matching_family() {
    // Skip if there's no IPv6 loopback to bind a ::1 pool on.
    if tokio::net::UdpSocket::bind("[::1]:0").await.is_err() {
        return;
    }
    let mut state = test_session_state();
    state.media_bindings = Arc::new(
        MediaBindings::new(
            &["127.0.0.1".parse().unwrap(), "::1".parse().unwrap()],
            55200,
            55400,
        )
        .unwrap(),
    );
    let (tx, _rx) = mpsc::channel(16);

    // IPv6 offer → IPv6 answer, allocated from the v6 pool.
    let (_id6, answer6) = state
        .handle_create_from_offer(
            &tx,
            &rtp_family_offer(true),
            EndpointDirection::SendRecv,
            None,
        )
        .await
        .unwrap();
    assert!(
        answer6.contains("c=IN IP6"),
        "IPv6 offer must get an IPv6 answer; answer:\n{answer6}"
    );

    // IPv4 offer → IPv4 answer.
    let (_id4, answer4) = state
        .handle_create_from_offer(
            &tx,
            &rtp_family_offer(false),
            EndpointDirection::SendRecv,
            None,
        )
        .await
        .unwrap();
    assert!(
        answer4.contains("c=IN IP4"),
        "IPv4 offer must get an IPv4 answer; answer:\n{answer4}"
    );
}
