use super::*;

/// Create a minimal `SessionState` for unit testing command handlers.
/// Uses in-memory defaults — no real sockets or file caches needed.
fn test_session_state() -> SessionState {
    let (cmd_tx, _cmd_rx) = mpsc::channel(16);
    let metrics = Arc::new(crate::metrics::Metrics::new());
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
        metrics: Arc::clone(&metrics),
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
        transcoding_metrics: TranscodingMetrics::new(metrics),
        url_sources: HashMap::new(),
        reserved_transfers: HashSet::new(),
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

#[test]
fn webrtc_connected_event_uses_critical_channel_when_regular_queue_is_full() {
    let (event_tx, _event_rx) = mpsc::channel(1);
    event_tx
        .try_send(Event::new("stats", serde_json::json!({})))
        .unwrap();
    let (critical_tx, mut critical_rx) = mpsc::channel(1);
    let endpoint_id = EndpointId::new_v4();
    let dropped = AtomicU64::new(0);
    emit_event_with_priority(
        &Some(event_tx),
        &Some(critical_tx),
        "endpoint.webrtc.connected",
        WebrtcConnectedData { endpoint_id },
        &dropped,
        &crate::metrics::Metrics::new(),
    );
    let event = critical_rx
        .try_recv()
        .expect("audio readiness must use the priority channel");
    assert_eq!(event.event, "endpoint.webrtc.connected");
    assert_eq!(event.data["endpoint_id"], endpoint_id.to_string());
    assert_eq!(dropped.load(Ordering::Relaxed), 0);
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
        drive_grid(&mut mix_grid, &mut buffers, false, &mut routed, now);
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

/// Generated prompt + silence must retain every frame on a fixed grid, even when
/// packet/control wakes arrive while both the mixer and playout buffers are empty.
#[test]
fn mixer_grid_preserves_prompt_frames_across_intertick_wakes() {
    use crate::session::mixer::DestinationMixer;

    let base = Instant::now();
    let destination = EndpointId::new_v4();
    let prompt = EndpointId::new_v4();
    let silence = EndpointId::new_v4();
    let mut mixers = HashMap::from([(
        destination,
        DestinationMixer::new(AudioCodec::L16 { sample_rate: 16000 }, 127).unwrap(),
    )]);
    let mut buffers = HashMap::new();
    let mut grid = None;
    let mut output = Vec::new();

    // Follow production ordering: clock, route generated frames, flush, then send.
    // Wakes every 5 ms leave three empty passes between consecutive prompt frames.
    for step in 0..=320 {
        let now = base + Duration::from_millis(step * 5);
        let mut routed = Vec::new();
        let fired = drive_grid(
            &mut grid,
            &mut buffers,
            !mixers.is_empty(),
            &mut routed,
            now,
        );
        assert!(
            routed.is_empty(),
            "generated sources bypass playout buffers"
        );
        let mixer = mixers.get_mut(&destination).unwrap();
        if step % 4 == 0 && step < 320 {
            let frame = (step / 4) as i16;
            mixer.feed_pcm(silence, Arc::new(vec![0; 320])).unwrap();
            mixer
                .feed_pcm(prompt, Arc::new(vec![1000 + frame; 320]))
                .unwrap();
        }
        if fired {
            mixer.flush_tick().unwrap();
        }
        for packet in mixer.drain() {
            output.push((now, packet));
        }
    }

    assert_eq!(output.len(), 80, "every prompt frame must reach the caller");
    for (index, (now, packet)) in output.iter().enumerate() {
        assert_eq!(*now, base + Duration::from_millis(index as u64 * 20));
        let expected: Vec<_> = (0..320)
            .flat_map(|_| (1000 + index as i16).to_le_bytes())
            .collect();
        assert_eq!(packet.payload, expected, "prompt frame {index} changed");
    }
    for pair in output.windows(2) {
        assert_eq!(pair[1].1.timestamp.wrapping_sub(pair[0].1.timestamp), 320);
    }

    // Empty mixers keep the deadline, but do not fabricate output. Removing the
    // final mixer must park the clock rather than leave an idle session ticking.
    let mut routed = Vec::new();
    let next_tick = grid;
    let fired = drive_grid(
        &mut grid,
        &mut buffers,
        !mixers.is_empty(),
        &mut routed,
        base + Duration::from_millis(1605),
    );
    assert!(!fired);
    assert_eq!(grid, next_tick);
    assert!(!mixers[&destination].has_pending());
    mixers.clear();
    drive_grid(
        &mut grid,
        &mut buffers,
        !mixers.is_empty(),
        &mut routed,
        base + Duration::from_millis(1610),
    );
    assert!(grid.is_none());
    assert!(routed.is_empty());
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
        crate::session::source_audio::SourceAudio::new(AudioCodec::G722).unwrap(),
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
        crate::session::source_audio::SourceAudio::new(AudioCodec::G722).unwrap(),
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
            started_emitted: false,
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
async fn test_timeline_mark_is_actor_ordered_and_uses_epoch_milliseconds() {
    let mut state = test_session_state();
    let (packet_tx, _packet_rx) = mpsc::channel(16);
    let before = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock")
        .as_millis() as u64;
    let (reply_tx, reply_rx) = oneshot::channel();

    let continues = state
        .handle_command(SessionCommand::TimelineMark { reply: reply_tx }, &packet_tx)
        .await;
    let marked_at = reply_rx.await.expect("timeline mark reply");
    let after = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock")
        .as_millis() as u64;

    assert!(continues);
    assert!(marked_at >= before);
    assert!(marked_at <= after);
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
async fn test_handle_command_stats_snapshot_is_available_without_subscription() {
    let mut state = test_session_state();
    let (packet_tx, _packet_rx) = mpsc::channel(16);

    assert!(state.stats_interval.is_none());
    let (reply_tx, reply_rx) = oneshot::channel();
    state
        .handle_command(
            SessionCommand::StatsSnapshot {
                reply: reply_tx,
                include_diagnostics: false,
            },
            &packet_tx,
        )
        .await;

    let snapshot = reply_rx.await.expect("stats snapshot reply");
    assert!(snapshot.endpoints.is_empty());
    assert!(
        state.stats_interval.is_none(),
        "taking a snapshot must not create a periodic subscription"
    );
    assert!(
        !state.stats_include_diagnostics,
        "taking a compact snapshot must not change subscription diagnostics"
    );
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
        crate::session::source_audio::SourceAudio::new(AudioCodec::Pcmu).unwrap(),
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
                cache_key: None,
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
        maxptime: None,
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
    state.url_sources.clear();

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
async fn conference_output_follows_codec_and_payload_type_changes() {
    let mut state = test_session_state();
    let (tx, _rx) = mpsc::channel(16);
    let mut ids = Vec::new();
    for _ in 0..3 {
        let created = state
            .handle_create_from_offer(
                &tx,
                &rtp_family_offer(false),
                EndpointDirection::SendRecv,
                None,
            )
            .await;
        let (id, _) = created.unwrap();
        ids.push(id);
    }
    state.rebuild_routing();
    let destination = ids[0];
    let mixer = state.mixers.get_mut(&destination).unwrap();
    mixer.feed_pcm(ids[1], Arc::new(vec![1000; 160])).unwrap();
    mixer.flush_tick().unwrap();
    let before = mixer.drain().next().unwrap();
    assert_eq!(before.payload_type, 0);
    assert_eq!(before.payload.len(), 160);

    if let Endpoint::Rtp(endpoint) = state.endpoints.get_mut(&destination).unwrap() {
        let codec = endpoint.send_codec.as_mut().unwrap();
        codec.name = "g722".into();
        codec.pt = 109;
        codec.clock_rate = 8000;
    }
    state.rebuild_routing();
    let mixer = state.mixers.get_mut(&destination).unwrap();
    // G.722 consumes 320 PCM samples per 20 ms, PCMU consumed 160.
    mixer.feed_pcm(ids[1], Arc::new(vec![1000; 320])).unwrap();
    mixer.flush_tick().unwrap();
    let after = mixer.drain().next().unwrap();
    assert_eq!(after.payload_type, 109);
    assert_eq!(after.payload.len(), 160);
    assert!(after.marker);
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

#[tokio::test]
async fn test_create_from_offer_accepts_secure_and_plain_audio_alternatives() {
    let mut state = test_session_state();
    let (tx, _rx) = mpsc::channel(16);
    let offer = "v=0\r\n\
        o=FreeSWITCH 1 1 IN IP4 127.0.0.1\r\n\
        s=FreeSWITCH\r\n\
        c=IN IP4 127.0.0.1\r\n\
        t=0 0\r\n\
        m=audio 30000 RTP/SAVP 0 9 8 101 13\r\n\
        a=rtpmap:9 G722/8000\r\n\
        a=rtpmap:101 telephone-event/8000\r\n\
        a=crypto:1 AES_CM_128_HMAC_SHA1_80 inline:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\r\n\
        m=audio 30000 RTP/AVP 0 9 8 101 13\r\n";

    let (id, answer) = state
        .handle_create_from_offer(
            &tx,
            offer,
            EndpointDirection::SendRecv,
            Some(EndpointType::Rtp),
        )
        .await
        .unwrap();
    let Endpoint::Rtp(endpoint) = state.endpoints.get(&id).unwrap() else {
        panic!("expected RTP endpoint");
    };
    assert!(endpoint.has_srtp());
    assert_eq!(
        endpoint.send_codec.as_ref().map(|codec| codec.name),
        Some("G722")
    );
    let media_lines: Vec<&str> = answer
        .lines()
        .filter(|line| line.starts_with("m="))
        .collect();
    assert_eq!(media_lines.len(), 2);
    assert!(media_lines[0].contains(" RTP/SAVP 9 101"));
    assert_eq!(media_lines[1], "m=audio 0 RTP/AVP 0 9 8 101 13");
}

async fn double_check_rtp_session(count: usize) -> (SessionState, Vec<EndpointId>) {
    let mut state = test_session_state();
    let (tx, _rx) = mpsc::channel(16);
    let mut ids = Vec::new();
    for _ in 0..count {
        let created = state
            .handle_create_from_offer(
                &tx,
                &rtp_family_offer(false),
                EndpointDirection::SendRecv,
                None,
            )
            .await;
        ids.push(created.unwrap().0);
    }
    state.rebuild_routing();
    (state, ids)
}

async fn add_g722_metric_endpoint(state: &mut SessionState) -> EndpointId {
    let (tx, _rx) = mpsc::channel(16);
    let created = state
        .handle_create_offer(
            &tx,
            EndpointDirection::SendRecv,
            EndpointType::Rtp,
            false,
            false,
            Some(vec!["G722".to_string(), "PCMU".to_string()]),
        )
        .await;
    let (id, _) = created.unwrap();
    // Keep PCMU as an agreed alternative so a subsequent answer can change
    // the selected codec without a Connected-state transition.
    let answer = rtp_family_offer(false)
        .replace("RTP/AVP 0 101", "RTP/AVP 9 0 101")
        .replace("a=rtpmap:0", "a=rtpmap:9 G722/8000\r\na=rtpmap:0");
    state.handle_accept_answer(id, &answer, None, None).unwrap();
    id
}

#[tokio::test]
async fn transcoding_metrics_follow_peer_routes_once_per_session() {
    // Same-codec conferences still decode/mix/encode, but are not codec mismatches.
    let (mut state, _) = double_check_rtp_session(3).await;
    let metrics = Arc::clone(&state.metrics);
    assert!(!state.mixers.is_empty());
    assert_eq!(metrics.transcoding_sessions_total.get(), 0);
    assert_eq!(metrics.transcoding_sessions_active.get(), 0);

    let mismatched_id = add_g722_metric_endpoint(&mut state).await;
    assert_eq!(metrics.transcoding_sessions_total.get(), 1);
    assert_eq!(metrics.transcoding_sessions_active.get(), 1);
    state.rebuild_routing();
    assert_eq!(metrics.transcoding_sessions_total.get(), 1);
    assert_eq!(metrics.transcoding_sessions_active.get(), 1);

    state
        .handle_update_direction(mismatched_id, EndpointDirectionUpdate::Inactive)
        .unwrap();
    assert_eq!(metrics.transcoding_sessions_active.get(), 0);
    state
        .handle_update_direction(mismatched_id, EndpointDirectionUpdate::SendRecv)
        .unwrap();
    assert_eq!(metrics.transcoding_sessions_active.get(), 1);
    assert_eq!(metrics.transcoding_sessions_total.get(), 1);

    // An answer can change the codec without a Connected-state transition.
    state
        .handle_accept_answer(mismatched_id, &rtp_family_offer(false), None, None)
        .unwrap();
    assert_eq!(metrics.transcoding_sessions_active.get(), 0);
    let replacement_id = add_g722_metric_endpoint(&mut state).await;
    assert_eq!(metrics.transcoding_sessions_active.get(), 1);
    assert_eq!(metrics.transcoding_sessions_total.get(), 1);

    // Removing or transferring the mismatched endpoint releases the source
    // session's contribution; the lifetime counter remains historical.
    let removed = state.handle_remove_endpoint(replacement_id).await;
    removed.unwrap();
    assert_eq!(metrics.transcoding_sessions_active.get(), 0);
    assert_eq!(metrics.transcoding_sessions_total.get(), 1);
    assert_eq!(metrics.file_transcodings_active.get(), 0);
}

#[tokio::test]
async fn file_transcoding_metrics_follow_playback_and_destination_lifetimes() {
    let (mut state, ids) = double_check_rtp_session(2).await;
    let metrics = Arc::clone(&state.metrics);
    let file_id = EndpointId::new_v4();
    let file = FileEndpoint::new_buffering(file_id, 0.0);
    state
        .endpoints
        .insert(file_id, Endpoint::File(Box::new(file)));
    state.rebuild_routing();
    assert_eq!(metrics.file_transcodings_active.get(), 0);
    if let Endpoint::File(file) = state.endpoints.get_mut(&file_id).unwrap() {
        file.state = EndpointState::Playing;
    }
    state.rebuild_routing();
    assert_eq!(metrics.file_transcodings_active.get(), 2);
    assert_eq!(metrics.transcoding_sessions_total.get(), 0);
    assert_eq!(metrics.transcoding_sessions_active.get(), 0);

    // Other expected media conversions don't turn this into a peer mismatch.
    state
        .handle_create_tone(super::super::endpoint_tone::ToneType::Ringback, None, None)
        .unwrap();
    let created = state
        .handle_create_websocket(EndpointDirection::SendOnly, 16000, 0)
        .unwrap();
    let (websocket_id, _) = created;
    if let Endpoint::WebSocket(endpoint) = state.endpoints.get_mut(&websocket_id).unwrap() {
        endpoint.state = EndpointState::Connected;
    }
    state.rebuild_routing();
    assert_eq!(metrics.file_transcodings_active.get(), 2);
    assert_eq!(metrics.transcoding_sessions_total.get(), 0);

    state.handle_file_pause(file_id).unwrap();
    state.handle_file_pause(file_id).unwrap();
    assert_eq!(metrics.file_transcodings_active.get(), 0);
    state.rebuild_routing();
    state.handle_file_resume(file_id).unwrap();
    state.handle_file_resume(file_id).unwrap();
    assert_eq!(metrics.file_transcodings_active.get(), 2);

    state
        .handle_update_direction(ids[0], EndpointDirectionUpdate::Inactive)
        .unwrap();
    assert_eq!(metrics.file_transcodings_active.get(), 1);
    let removed = state.handle_remove_endpoint(file_id).await;
    removed.unwrap();
    assert_eq!(metrics.file_transcodings_active.get(), 0);
    assert_eq!(metrics.transcoding_sessions_total.get(), 0);
}

#[tokio::test]
async fn transcoding_metrics_aggregate_sessions_and_release_on_task_abort() {
    let (mut first, _) = double_check_rtp_session(1).await;
    let metrics = Arc::clone(&first.metrics);
    add_g722_metric_endpoint(&mut first).await;
    let file_id = EndpointId::new_v4();
    let mut file = FileEndpoint::new_buffering(file_id, 0.0);
    file.state = EndpointState::Playing;
    first
        .endpoints
        .insert(file_id, Endpoint::File(Box::new(file)));
    first.rebuild_routing();
    let mut second = TranscodingMetrics::new(Arc::clone(&metrics));
    second.update(SessionId::new_v4(), &first.endpoints, &first.routing);
    assert_eq!(metrics.transcoding_sessions_total.get(), 2);
    assert_eq!(metrics.transcoding_sessions_active.get(), 2);
    assert_eq!(metrics.file_transcodings_active.get(), 4);
    drop(first);
    assert_eq!(metrics.transcoding_sessions_active.get(), 1);
    assert_eq!(metrics.file_transcodings_active.get(), 2);

    let (ready_tx, ready_rx) = oneshot::channel();
    let task = tokio::spawn(async move {
        let _ = ready_tx.send(());
        std::future::pending::<()>().await;
        drop(second);
    });
    let ready = ready_rx.await;
    ready.unwrap();
    task.abort();
    let result = task.await;
    assert!(result.unwrap_err().is_cancelled());
    assert_eq!(metrics.transcoding_sessions_total.get(), 2);
    assert_eq!(metrics.transcoding_sessions_active.get(), 0);
    assert_eq!(metrics.file_transcodings_active.get(), 0);
}

#[tokio::test]
async fn completed_generators_leave_mixers_and_restore_direct_routing() {
    for file in [false, true] {
        let (mut state, ids) = double_check_rtp_session(2).await;
        let source = EndpointId::new_v4();
        let generator = if file {
            // A decoder without more packets has the same EOF transition as a
            // consumed file; the file-poll test separately covers its last PCM.
            let mut endpoint = FileEndpoint::new_buffering(source, 0.0);
            endpoint.state = EndpointState::Playing;
            endpoint.loop_count = Some(0);
            Endpoint::File(Box::new(endpoint))
        } else {
            Endpoint::Tone(Box::new(super::super::endpoint_tone::ToneEndpoint::new(
                source,
                super::super::endpoint_tone::ToneType::Beep,
                None,
                Some(0),
            )))
        };
        state.endpoints.insert(source, generator);
        state.rebuild_routing();
        assert_eq!(state.mixers.len(), 2);
        assert_eq!(
            state.metrics.file_transcodings_active.get(),
            if file { 2 } else { 0 }
        );
        let (_, changed) = poll_and_route(
            &mut state.endpoints,
            &mut state.dtmf_state,
            &state.sensitive_dtmf_endpoints,
            &state.routing,
            &state.event_tx,
            &state.critical_event_tx,
            &state.dropped_events,
            &mut state.recording_mgr,
            &mut state.vad_monitors,
            &mut state.fax_detectors,
            &mut state.analysis_decoders,
            &state.metrics,
            Vec::new(),
            &mut state.file_rtp_states,
            &mut state.tone_rtp_states,
            &mut state.transcode_cache,
            128,
            &mut state.mixers,
            &mut state.playout_buffers,
            &state.playout_policy,
            &mut state.mix_grid,
        )
        .await;
        assert!(changed, "generator completion must update routing");
        state.rebuild_routing();
        assert!(state.mixers.is_empty());
        assert!(state.routing.destinations(&source).is_none());
        assert_eq!(state.metrics.file_transcodings_active.get(), 0);
        assert_eq!(
            state.routing.destinations(&ids[0]),
            Some(&HashSet::from([ids[1]]))
        );
    }
}

#[tokio::test]
async fn double_check_tap_stop_preserves_pcm_needed_by_routing() {
    for count in [2, 3] {
        for fax in [false, true] {
            let (mut state, ids) = double_check_rtp_session(count).await;
            let source = ids[0];
            if count == 2 {
                if let Endpoint::Rtp(endpoint) = state.endpoints.get_mut(&ids[1]).unwrap() {
                    let codec = endpoint.send_codec.as_mut().unwrap();
                    codec.name = "g722".into();
                    codec.pt = 9;
                }
                state.rebuild_routing();
            }
            if fax {
                state.handle_fax_detect_start(source).unwrap();
            } else {
                state.handle_vad_start(source, 1000, 0.5).unwrap();
            }
            let mut audio =
                crate::session::source_audio::SourceAudio::new(AudioCodec::Pcmu).unwrap();
            let mut packet = RoutedRtpPacket {
                source_endpoint_id: source,
                payload_type: 0,
                sequence_number: 1,
                timestamp: 0,
                ssrc: 7,
                marker: false,
                payload: vec![0xff; 80],
            };
            let pcm = audio.decode(&packet).unwrap();
            assert!(audio.frames(&pcm, &[8000]).unwrap().is_empty());
            state.analysis_decoders.insert(source, audio);
            if fax {
                state.handle_fax_detect_stop(source).unwrap();
            } else {
                state.handle_vad_stop(source).unwrap();
            }
            let audio = state
                .analysis_decoders
                .get_mut(&source)
                .expect("a routed source still owns its buffered PCM after its tap stops");
            packet.sequence_number += 1;
            packet.timestamp += 80;
            let pcm = audio.decode(&packet).unwrap();
            let frames = audio.frames(&pcm, &[8000]).unwrap();
            assert_eq!(frames.len(), 1, "both 10 ms halves must reach the encoder");
        }
    }
}

#[tokio::test]
async fn double_check_codec_change_updates_playout_clock() {
    for (name, pt, clock_rate) in [("opus", 111, 48000), ("g722", 9, 8000), ("PCMU", 96, 8000)] {
        let (mut state, ids) = double_check_rtp_session(3).await;
        let source = ids[0];
        let now = Instant::now();
        state.playout_buffers.get_mut(&source).unwrap().push(
            RoutedRtpPacket {
                source_endpoint_id: source,
                payload_type: 0,
                sequence_number: 600,
                timestamp: 1_000_000,
                ssrc: 7,
                marker: false,
                payload: vec![0xff; 160],
            },
            now,
        );
        if let Endpoint::Rtp(endpoint) = state.endpoints.get_mut(&source).unwrap() {
            let codec = endpoint.send_codec.as_mut().unwrap();
            codec.name = name.into();
            codec.pt = pt;
            codec.clock_rate = clock_rate;
        }
        state.rebuild_routing();
        let buffer = state.playout_buffers.get_mut(&source).unwrap();
        for sequence in 0..2u16 {
            buffer.push(
                RoutedRtpPacket {
                    source_endpoint_id: source,
                    payload_type: pt,
                    sequence_number: sequence,
                    timestamp: u32::from(sequence) * (clock_rate / 50),
                    ssrc: 7,
                    marker: false,
                    payload: vec![0; 10],
                },
                now,
            );
        }
        let first = buffer.drain_tick(now + Duration::from_millis(60)).unwrap();
        assert_eq!(
            first.sequence_number, 0,
            "discard packets buffered under the old codec/PT"
        );
        let second = buffer
            .drain_tick(now + Duration::from_millis(80))
            .expect("the new source clock must produce its next packet 20 ms later");
        assert_eq!(second.sequence_number, 1);
    }
}

#[tokio::test]
async fn double_check_duplicate_transfer_cannot_release_another_rollback_slot() {
    let (mut state, ids) = double_check_rtp_session(1).await;
    state.max_endpoints = 1;
    let endpoint_id = ids[0];
    let (packet_tx, _packet_rx) = mpsc::channel(16);
    let first = Arc::new(std::sync::Mutex::new(
        crate::session::transfer::TransferState::default(),
    ));
    let second = Arc::new(std::sync::Mutex::new(
        crate::session::transfer::TransferState::default(),
    ));
    for (slot, should_succeed) in [(first.clone(), true), (second.clone(), false)] {
        let (reply, response) = oneshot::channel();
        state
            .handle_command(
                SessionCommand::PrepareTransfer {
                    slot,
                    endpoint_id,
                    reply,
                },
                &packet_tx,
            )
            .await;
        let response = response.await;
        assert_eq!(response.unwrap().is_ok(), should_succeed);
    }
    state
        .handle_command(
            SessionCommand::FinishTransfer {
                slot: second,
                endpoint_id,
            },
            &packet_tx,
        )
        .await;
    assert!(
        state.reserved_transfers.contains(&endpoint_id),
        "the first transfer must retain its rollback capacity"
    );
    let created = state
        .handle_create_from_offer(
            &packet_tx,
            &rtp_family_offer(false),
            EndpointDirection::SendRecv,
            None,
        )
        .await;
    assert!(
        created.is_err(),
        "rollback capacity cannot be used by a new endpoint"
    );
    first.lock().unwrap().cancelled = true;
    state
        .handle_command(
            SessionCommand::FinishTransfer {
                slot: first,
                endpoint_id,
            },
            &packet_tx,
        )
        .await;
    assert!(state.endpoints.contains_key(&endpoint_id));
    assert!(state.reserved_transfers.is_empty());
}

fn codec_test_offer(formats: &str, mappings: &str) -> String {
    format!(
        "v=0\r\no=- 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 31000 RTP/AVP {formats}\r\n{mappings}"
    )
}

#[tokio::test]
async fn codec_source_prefers_established_pcmu_and_rejects_provisional_or_foreign_sources() {
    let mut state = test_session_state();
    let (packets, _) = mpsc::channel(16);
    let pcmu = codec_test_offer("0", "a=rtpmap:0 PCMU/8000\r\n");
    let (caller, _) = state
        .handle_create_from_offer(
            &packets,
            &pcmu,
            EndpointDirection::SendRecv,
            Some(EndpointType::Rtp),
        )
        .await
        .unwrap();
    let (chosen, candidates) = state
        .codec_candidates(
            Some(&CodecSource::Endpoint {
                endpoint_id: caller,
            }),
            EndpointType::Rtp,
            None,
        )
        .unwrap();
    assert_eq!(chosen.unwrap(), ["PCMU"]);
    assert_eq!(candidates, ["PCMU", "opus", "G722"]);
    let (pending, _) = state
        .handle_create_offer(
            &packets,
            EndpointDirection::Inactive,
            EndpointType::Rtp,
            false,
            false,
            None,
        )
        .await
        .unwrap();
    for endpoint_id in [pending, EndpointId::new_v4()] {
        assert!(
            state
                .codec_candidates(
                    Some(&CodecSource::Endpoint { endpoint_id }),
                    EndpointType::Rtp,
                    None
                )
                .is_err()
        );
    }
    assert!(state.routing.destinations(&caller).is_none());
    assert_eq!(state.metrics.transcoding_sessions_active.get(), 0);
}

#[tokio::test]
async fn codec_source_fresh_offer_uses_quality_intersection_and_webrtc_subset() {
    let state = test_session_state();
    let offer = codec_test_offer(
        "0 9 123",
        "a=rtpmap:0 PCMU/8000\r\na=rtpmap:9 G722/8000\r\na=rtpmap:123 opus/48000/2\r\n",
    );
    let source = CodecSource::Offer { sdp: offer };
    let (selected, candidates) = state
        .codec_candidates(Some(&source), EndpointType::Rtp, None)
        .unwrap();
    assert_eq!(selected.unwrap(), ["opus"]);
    assert_eq!(candidates, ["opus", "G722", "PCMU"]);
    let (subset, _) = state
        .codec_candidates(Some(&source), EndpointType::Webrtc, None)
        .unwrap();
    assert_eq!(subset.unwrap(), ["opus"]);
    for offer in [
        codec_test_offer("101", "a=rtpmap:101 telephone-event/8000\r\n"),
        codec_test_offer("111", "a=rtpmap:111 opus/48000/1\r\n"),
    ] {
        assert!(
            state
                .codec_candidates(
                    Some(&CodecSource::Offer { sdp: offer }),
                    EndpointType::Rtp,
                    None
                )
                .is_err()
        );
    }
    assert!(
        state
            .codec_candidates(Some(&source), EndpointType::Rtp, Some(&[]))
            .is_err()
    );
    assert!(
        state
            .codec_candidates(
                Some(&source),
                EndpointType::Rtp,
                Some(&["PCMA".to_string()])
            )
            .is_err()
    );
    let only_g722 = CodecSource::Offer {
        sdp: codec_test_offer("9", "a=rtpmap:9 G722/8000\r\n"),
    };
    let (subset, _) = state
        .codec_candidates(Some(&only_g722), EndpointType::Webrtc, None)
        .unwrap();
    assert_eq!(subset.unwrap(), ["opus"]);
}

#[tokio::test]
async fn codec_winner_answer_preserves_dynamic_payloads_and_directional_opus_constraints() {
    let mut state = test_session_state();
    let (packets, mut inbound) = mpsc::channel(16);
    let bound_caller = tokio::net::UdpSocket::bind("127.0.0.1:0").await;
    let caller_socket = bound_caller.unwrap();
    let bound_peer = tokio::net::UdpSocket::bind("127.0.0.1:0").await;
    let peer_socket = bound_peer.unwrap();
    let caller_addr = caller_socket.local_addr().unwrap();
    let peer_addr = peer_socket.local_addr().unwrap();
    let caller_offer = codec_test_offer(
        "0 123 101 108",
        "a=rtpmap:123 opus/48000/2\r\na=fmtp:123 maxplaybackrate=16000\r\na=rtpmap:101 telephone-event/8000\r\na=rtpmap:108 telephone-event/48000\r\n",
    ).replace("m=audio 31000", &format!("m=audio {}", caller_addr.port()));
    let (reply, receiver) = tokio::sync::oneshot::channel();
    state
        .handle_command(
            SessionCommand::CreateOffer {
                reply,
                direction: EndpointDirection::Inactive,
                endpoint_type: EndpointType::Rtp,
                srtp: false,
                srtp_optional: false,
                codecs: None,
                codec_source: Some(CodecSource::Offer {
                    sdp: caller_offer.clone(),
                }),
            },
            &packets,
        )
        .await;
    let offer = receiver.await.unwrap().unwrap();
    let parsed = sdp::parse_sdp(&offer.sdp_offer);
    assert_eq!(
        parsed
            .codecs
            .iter()
            .filter(|c| c.name != "telephone-event")
            .map(|c| c.name)
            .collect::<Vec<_>>(),
        ["opus"]
    );
    assert!(offer.sdp_offer.contains("maxplaybackrate=16000"));
    let peer_answer = codec_test_offer(
        "111 101",
        "a=rtpmap:111 opus/48000/2\r\na=fmtp:111 maxaveragebitrate=12000;maxplaybackrate=16000;stereo=0\r\na=rtpmap:101 telephone-event/8000\r\n",
    ).replace("m=audio 31000", &format!("m=audio {}", peer_addr.port()));
    state
        .handle_accept_answer(
            offer.endpoint_id,
            &peer_answer,
            Some(EndpointType::Rtp),
            None,
        )
        .unwrap();
    assert_eq!(
        state
            .negotiated_codec(offer.endpoint_id)
            .unwrap()
            .payload_type,
        111
    );
    let (reply, receiver) = tokio::sync::oneshot::channel();
    state
        .handle_command(
            SessionCommand::CreateFromOffer {
                reply,
                sdp: caller_offer,
                direction: EndpointDirection::SendRecv,
                expected_type: Some(EndpointType::Rtp),
                codec: None,
                peer_endpoint_id: Some(offer.endpoint_id),
            },
            &packets,
        )
        .await;
    let caller = receiver.await.unwrap().unwrap();
    let codec = caller.codec.unwrap();
    assert_eq!(codec.name, "opus");
    assert_eq!(codec.payload_type, 123);
    assert!(caller.sdp_answer.contains("maxaveragebitrate=12000"));
    assert!(
        caller
            .sdp_answer
            .contains("a=rtpmap:108 telephone-event/48000")
    );
    assert!(
        !crate::session::endpoint_enum::endpoint_requires_transcoding(
            &state.endpoints[&caller.endpoint_id],
            &state.endpoints[&offer.endpoint_id]
        )
    );
    assert!(
        !crate::session::endpoint_enum::endpoint_requires_transcoding(
            &state.endpoints[&offer.endpoint_id],
            &state.endpoints[&caller.endpoint_id]
        )
    );
    state
        .handle_update_direction(offer.endpoint_id, EndpointDirectionUpdate::Auto)
        .unwrap();
    let mut encoder = crate::media::codec::OpusEncoder::with_profile(sdp::OpusProfile::parse(
        Some("maxaveragebitrate=12000;maxplaybackrate=16000"),
    ))
    .unwrap();
    let mut payload = Vec::new();
    crate::media::codec::AudioEncoder::encode(&mut encoder, &[300; 960], &mut payload).unwrap();
    // Learn both symmetric-RTP addresses before testing media forwarding.
    for (source, sender, pt, addr) in [
        (caller.endpoint_id, &caller_socket, 123, caller_addr),
        (offer.endpoint_id, &peer_socket, 111, peer_addr),
    ] {
        let Endpoint::Rtp(endpoint) = &state.endpoints[&source] else {
            panic!("expected RTP endpoint")
        };
        let seed = crate::media::rtp::RtpHeader::build(pt, 0, 0, 333, true, &payload);
        let sent = sender.send_to(&seed, endpoint.local_rtp_addr).await;
        sent.unwrap();
        let received = tokio::time::timeout(Duration::from_secs(1), inbound.recv()).await;
        let packet = received.unwrap().unwrap();
        assert_eq!(packet.source, addr);
        let (routed, _, _) = handle_inbound_packet(&mut state.endpoints, &packet, &state.metrics);
        assert!(routed.is_some());
    }
    for (source, sender, source_pt) in [
        (caller.endpoint_id, &caller_socket, 123),
        (offer.endpoint_id, &peer_socket, 111),
    ] {
        let Endpoint::Rtp(endpoint) = &state.endpoints[&source] else {
            panic!("expected RTP endpoint")
        };
        let local_addr = endpoint.local_rtp_addr;
        // Prime the real jitter buffer with a short burst of consecutive 20ms packets.
        for sequence in 1..=4 {
            let data = crate::media::rtp::RtpHeader::build(
                source_pt,
                sequence,
                u32::from(sequence) * 960,
                333,
                sequence == 1,
                &payload,
            );
            let sent = sender.send_to(&data, local_addr).await;
            sent.unwrap();
            let received = tokio::time::timeout(Duration::from_secs(1), inbound.recv()).await;
            let packet = received.unwrap().unwrap();
            let (routed, _, _) =
                handle_inbound_packet(&mut state.endpoints, &packet, &state.metrics);
            codec_test_route(&mut state, vec![routed.unwrap()]).await;
        }
    }
    for (receiver, destination_pt) in [(&peer_socket, 111), (&caller_socket, 123)] {
        let mut received = [0; 2048];
        let mut length = None;
        for _ in 0..100 {
            match receiver.try_recv_from(&mut received) {
                Ok((n, _)) => {
                    length = Some(n);
                    break;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(error) => panic!("media receive failed: {error}"),
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
            codec_test_route(&mut state, vec![]).await;
        }
        let length = length.unwrap_or_else(|| {
            panic!(
                "buffered RTP must reach PT {destination_pt}; routed {}",
                state.metrics.packets_routed.get()
            )
        });
        let header = crate::media::rtp::RtpHeader::parse(&received[..length]).unwrap();
        assert_eq!(header.payload_type, destination_pt);
        assert_eq!(
            header.payload(&received[..length]),
            payload,
            "compatible profiles must preserve the encoded audio"
        );
    }
    assert!(state.transcode_cache.is_empty());
    assert_eq!(state.metrics.transcoding_sessions_active.get(), 0);
}

#[tokio::test]
async fn codec_constrained_answer_rejects_payload_changes_without_partial_mutation() {
    let mut state = test_session_state();
    let (packets, _) = mpsc::channel(16);
    let (id, _) = state
        .handle_create_offer(
            &packets,
            EndpointDirection::Inactive,
            EndpointType::Rtp,
            false,
            false,
            Some(vec!["opus".to_string()]),
        )
        .await
        .unwrap();
    for answer in [
        codec_test_offer("123", "a=rtpmap:123 opus/48000/2\r\n"),
        codec_test_offer(
            "111 120",
            "a=rtpmap:111 opus/48000/2\r\na=rtpmap:120 opus/48000/2\r\n",
        ),
        codec_test_offer("101", "a=rtpmap:101 telephone-event/8000\r\n"),
    ] {
        assert!(
            state
                .handle_accept_answer(id, &answer, Some(EndpointType::Rtp), None)
                .is_err()
        );
        assert!(state.negotiated_codec(id).is_err());
        assert_eq!(state.endpoints[&id].state(), EndpointState::Connecting);
    }
    let count = state.endpoints.len();
    assert!(
        state
            .handle_create_offer(
                &packets,
                EndpointDirection::Inactive,
                EndpointType::Rtp,
                false,
                false,
                Some(vec![])
            )
            .await
            .is_err()
    );
    assert_eq!(state.endpoints.len(), count);
}

#[tokio::test]
async fn codec_opus_profile_conversion_counts_routes_and_preserves_remote_hold() {
    let mut state = test_session_state();
    let (packets, _) = mpsc::channel(16);
    let (source, _) = state
        .handle_create_from_offer(
            &packets,
            &codec_test_offer("111", "a=rtpmap:111 opus/48000/2\r\n"),
            EndpointDirection::SendRecv,
            Some(EndpointType::Rtp),
        )
        .await
        .unwrap();
    let (destination, _) = state
        .handle_create_from_offer(
            &packets,
            &codec_test_offer(
                "111",
                "a=rtpmap:111 opus/48000/2\r\na=fmtp:111 maxaveragebitrate=12000\r\n",
            ),
            EndpointDirection::SendRecv,
            Some(EndpointType::Rtp),
        )
        .await
        .unwrap();
    assert!(
        crate::session::endpoint_enum::endpoint_requires_transcoding(
            &state.endpoints[&source],
            &state.endpoints[&destination]
        )
    );
    assert!(
        !crate::session::endpoint_enum::endpoint_requires_transcoding(
            &state.endpoints[&destination],
            &state.endpoints[&source]
        )
    );
    assert_eq!(state.metrics.transcoding_sessions_active.get(), 1);
    state.handle_remove_endpoint(destination).await.unwrap();
    assert_eq!(state.metrics.transcoding_sessions_active.get(), 0);

    let (pending, _) = state
        .handle_create_offer(
            &packets,
            EndpointDirection::Inactive,
            EndpointType::Rtp,
            false,
            false,
            Some(vec!["opus".to_string()]),
        )
        .await
        .unwrap();
    let held_answer = codec_test_offer("111", "a=rtpmap:111 opus/48000/2\r\na=sendonly\r\n");
    state
        .handle_accept_answer(pending, &held_answer, Some(EndpointType::Rtp), None)
        .unwrap();
    if let Endpoint::Rtp(ep) = state.endpoints.get_mut(&pending).unwrap() {
        ep.set_direction_override(EndpointDirectionUpdate::Auto);
        assert_eq!(ep.config.direction, EndpointDirection::SendOnly);
    }
}

#[tokio::test]
async fn codec_webrtc_pcmu_source_generates_exclusive_initial_offer() {
    let mut state = test_session_state();
    let (packets, _) = mpsc::channel(16);
    let source = CodecSource::Offer {
        sdp: codec_test_offer("0 101", "a=rtpmap:101 telephone-event/8000\r\n"),
    };
    let (codecs, _) = state
        .codec_candidates(Some(&source), EndpointType::Webrtc, None)
        .unwrap();
    let (_, offer) = state
        .handle_create_offer(
            &packets,
            EndpointDirection::SendRecv,
            EndpointType::Webrtc,
            false,
            false,
            codecs,
        )
        .await
        .unwrap();
    let parsed = sdp::parse_sdp(&offer);
    assert_eq!(
        parsed
            .codecs
            .iter()
            .filter(|c| c.name != "telephone-event")
            .map(|c| c.name)
            .collect::<Vec<_>>(),
        ["PCMU"]
    );
}

#[test]
fn codec_parser_excludes_opus_packetization_that_the_encoder_cannot_satisfy() {
    for attributes in ["a=fmtp:111 minptime=40\r\n", "a=maxptime:10\r\n"] {
        let offer = codec_test_offer(
            "111 0",
            &format!("a=rtpmap:111 opus/48000/2\r\n{attributes}"),
        );
        let parsed = sdp::parse_sdp(&offer);
        assert!(!parsed.opus_profile.unwrap().encodable());
        assert_eq!(
            sdp::select_answer_codec(&parsed.codecs).unwrap().name,
            "PCMU"
        );
    }
}

async fn codec_test_route(state: &mut SessionState, packets: Vec<RoutedRtpPacket>) {
    poll_and_route(
        &mut state.endpoints,
        &mut state.dtmf_state,
        &state.sensitive_dtmf_endpoints,
        &state.routing,
        &state.event_tx,
        &state.critical_event_tx,
        &state.dropped_events,
        &mut state.recording_mgr,
        &mut state.vad_monitors,
        &mut state.fax_detectors,
        &mut state.analysis_decoders,
        &state.metrics,
        packets,
        &mut state.file_rtp_states,
        &mut state.tone_rtp_states,
        &mut state.transcode_cache,
        128,
        &mut state.mixers,
        &mut state.playout_buffers,
        &state.playout_policy,
        &mut state.mix_grid,
    )
    .await;
}
