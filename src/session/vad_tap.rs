use std::collections::HashMap;
use std::sync::atomic::AtomicU64;

use tokio::sync::mpsc;
use tracing::warn;

use super::endpoint::RoutedRtpPacket;
use super::endpoint_enum::{Endpoint, endpoint_audio_codec};
use crate::control::protocol::*;
use crate::media::codec::AudioCodec;
use crate::media::vad::VadMonitor;

/// Start VAD monitoring on an endpoint.
pub fn vad_start(
    endpoints: &HashMap<EndpointId, Endpoint>,
    vad_monitors: &mut HashMap<EndpointId, VadMonitor>,
    endpoint_id: EndpointId,
    silence_interval_ms: u32,
    speech_threshold: f32,
) -> anyhow::Result<()> {
    let ep = endpoints
        .get(&endpoint_id)
        .ok_or_else(|| anyhow::anyhow!("Endpoint not found"))?;
    let sample_rate = endpoint_audio_codec(ep)
        .map(|c| c.sample_rate())
        .unwrap_or(8000);
    vad_monitors.insert(
        endpoint_id,
        VadMonitor::new(sample_rate, speech_threshold, silence_interval_ms),
    );
    Ok(())
}

/// Stop VAD monitoring on an endpoint.
pub fn vad_stop(
    vad_monitors: &mut HashMap<EndpointId, VadMonitor>,
    vad_decoders: &mut HashMap<EndpointId, Box<dyn crate::media::codec::AudioDecoder>>,
    endpoint_id: EndpointId,
) -> anyhow::Result<()> {
    vad_decoders.remove(&endpoint_id);
    vad_monitors
        .remove(&endpoint_id)
        .map(|_| ())
        .ok_or_else(|| anyhow::anyhow!("VAD not active for endpoint"))
}

/// Feed decoded PCM to VAD monitors for all routed audio packets.
pub fn process_vad(
    packets: &[RoutedRtpPacket],
    endpoints: &HashMap<EndpointId, Endpoint>,
    vad_monitors: &mut HashMap<EndpointId, VadMonitor>,
    vad_decoders: &mut HashMap<EndpointId, Box<dyn crate::media::codec::AudioDecoder>>,
    event_tx: &Option<mpsc::Sender<Event>>,
    dropped_events: &AtomicU64,
    metrics: &crate::metrics::Metrics,
) {
    for pkt in packets {
        if !vad_monitors.contains_key(&pkt.source_endpoint_id) {
            continue;
        }

        let vad_codec = endpoints
            .get(&pkt.source_endpoint_id)
            .and_then(endpoint_audio_codec);

        let pcm: Option<Vec<i16>> = match vad_codec {
            Some(AudioCodec::Pcmu) => {
                let decoder = xlaw::PcmXLawDecoder::new_ulaw();
                Some(pkt.payload.iter().map(|&b| decoder.decode(b)).collect())
            }
            Some(codec) => {
                use std::collections::hash_map::Entry;
                let dec = match vad_decoders.entry(pkt.source_endpoint_id) {
                    Entry::Occupied(e) => Some(e.into_mut()),
                    Entry::Vacant(e) => match crate::media::codec::make_decoder(codec) {
                        Ok(d) => Some(e.insert(d)),
                        Err(err) => {
                            warn!(endpoint_id = %pkt.source_endpoint_id, %err, "VAD decoder creation failed");
                            super::media_session::emit_event(
                                event_tx,
                                "vad.error",
                                serde_json::json!({
                                    "endpoint_id": pkt.source_endpoint_id.to_string(),
                                    "error": format!("VAD decoder creation failed: {err}")
                                }),
                                dropped_events,
                                metrics,
                            );
                            None
                        }
                    },
                };
                if let Some(dec) = dec {
                    let mut pcm_buf = Vec::new();
                    match dec.decode(&pkt.payload, &mut pcm_buf) {
                        Ok(()) => Some(pcm_buf),
                        Err(_) => None,
                    }
                } else {
                    None
                }
            }
            None => None,
        };

        if let Some(pcm) = pcm
            && let Some(vad) = vad_monitors.get_mut(&pkt.source_endpoint_id)
        {
            for vad_event in vad.process(&pcm) {
                match vad_event {
                    crate::media::vad::VadEvent::SpeechStarted => {
                        super::media_session::emit_event(
                            event_tx,
                            "vad.speech_started",
                            VadSpeechStartedData {
                                endpoint_id: pkt.source_endpoint_id,
                            },
                            dropped_events,
                            metrics,
                        );
                    }
                    crate::media::vad::VadEvent::Silence { duration_ms } => {
                        super::media_session::emit_event(
                            event_tx,
                            "vad.silence",
                            VadSilenceData {
                                endpoint_id: pkt.source_endpoint_id,
                                silence_duration_ms: duration_ms,
                            },
                            dropped_events,
                            metrics,
                        );
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::protocol::EndpointDirection;
    use crate::media::sdp;
    use crate::net::socket_pool::SocketPool;
    use crate::session::endpoint_rtp::RtpEndpoint;
    use tokio::sync::mpsc;

    fn test_dropped() -> AtomicU64 {
        AtomicU64::new(0)
    }

    fn test_metrics() -> crate::metrics::Metrics {
        crate::metrics::Metrics::new()
    }

    /// Helper: allocate a SocketPair for test endpoints.
    async fn test_socket_pair() -> crate::net::socket_pool::SocketPair {
        let pool = SocketPool::new("127.0.0.1".parse().unwrap(), 56000, 56100).unwrap();
        pool.allocate_pair().await.unwrap()
    }

    /// Helper: create an RTP endpoint with a given send_codec.
    async fn make_rtp_endpoint(codec: Option<sdp::SdpCodec>) -> (EndpointId, Endpoint) {
        let id = EndpointId::new_v4();
        let pair = test_socket_pair().await;
        let mut ep = RtpEndpoint::new(id, EndpointDirection::SendRecv, pair);
        ep.send_codec = codec;
        (id, Endpoint::Rtp(Box::new(ep)))
    }

    /// Helper: create a PCMU RTP packet payload (silence = 0xFF in mu-law).
    fn pcmu_silence_payload(num_bytes: usize) -> Vec<u8> {
        vec![0xFFu8; num_bytes]
    }

    // ── vad_start tests ─────────────────────────────────────────────────

    #[tokio::test]
    async fn vad_start_with_valid_endpoint() {
        let (id, ep) = make_rtp_endpoint(Some(sdp::CODEC_PCMU.clone())).await;
        let mut endpoints = HashMap::new();
        endpoints.insert(id, ep);
        let mut vad_monitors = HashMap::new();

        let result = vad_start(&endpoints, &mut vad_monitors, id, 1000, 0.5);
        assert!(
            result.is_ok(),
            "vad_start should succeed for existing endpoint"
        );
        assert!(vad_monitors.contains_key(&id), "monitor should be inserted");
    }

    #[tokio::test]
    async fn vad_start_missing_endpoint_returns_error() {
        let endpoints: HashMap<EndpointId, Endpoint> = HashMap::new();
        let mut vad_monitors = HashMap::new();
        let fake_id = EndpointId::new_v4();

        let result = vad_start(&endpoints, &mut vad_monitors, fake_id, 1000, 0.5);
        assert!(
            result.is_err(),
            "vad_start should fail for missing endpoint"
        );
        assert!(
            result.unwrap_err().to_string().contains("not found"),
            "error should mention endpoint not found"
        );
    }

    #[tokio::test]
    async fn vad_start_uses_correct_sample_rate_for_pcmu() {
        // PCMU -> 8000 Hz
        let (id, ep) = make_rtp_endpoint(Some(sdp::CODEC_PCMU.clone())).await;
        let mut endpoints = HashMap::new();
        endpoints.insert(id, ep);
        let mut vad_monitors = HashMap::new();

        vad_start(&endpoints, &mut vad_monitors, id, 500, 0.3).unwrap();
        // The monitor was created successfully; we can't inspect sample_rate directly,
        // but we verify it doesn't panic when processing 8kHz-rate data.
        let monitor = vad_monitors.get_mut(&id).unwrap();
        let pcm = vec![0i16; 160]; // 20ms of 8kHz silence
        let _events = monitor.process(&pcm);
    }

    #[tokio::test]
    async fn vad_start_uses_default_8000_when_no_codec() {
        // Endpoint with no send_codec -> defaults to 8000
        let (id, ep) = make_rtp_endpoint(None).await;
        let mut endpoints = HashMap::new();
        endpoints.insert(id, ep);
        let mut vad_monitors = HashMap::new();

        let result = vad_start(&endpoints, &mut vad_monitors, id, 1000, 0.5);
        assert!(result.is_ok());
        assert!(vad_monitors.contains_key(&id));
    }

    #[tokio::test]
    async fn vad_start_replaces_existing_monitor() {
        let (id, ep) = make_rtp_endpoint(Some(sdp::CODEC_PCMU.clone())).await;
        let mut endpoints = HashMap::new();
        endpoints.insert(id, ep);
        let mut vad_monitors = HashMap::new();

        // Start twice — second call should replace the monitor
        vad_start(&endpoints, &mut vad_monitors, id, 1000, 0.5).unwrap();
        vad_start(&endpoints, &mut vad_monitors, id, 500, 0.8).unwrap();
        assert_eq!(
            vad_monitors.len(),
            1,
            "should still have exactly one monitor"
        );
    }

    // ── vad_stop tests ──────────────────────────────────────────────────

    #[tokio::test]
    async fn vad_stop_removes_monitor_and_decoder() {
        let id = EndpointId::new_v4();
        let mut vad_monitors = HashMap::new();
        vad_monitors.insert(id, VadMonitor::new(8000, 0.5, 1000));

        let mut vad_decoders: HashMap<EndpointId, Box<dyn crate::media::codec::AudioDecoder>> =
            HashMap::new();
        vad_decoders.insert(
            id,
            crate::media::codec::make_decoder(AudioCodec::Pcmu).unwrap(),
        );

        let result = vad_stop(&mut vad_monitors, &mut vad_decoders, id);
        assert!(result.is_ok());
        assert!(!vad_monitors.contains_key(&id), "monitor should be removed");
        assert!(!vad_decoders.contains_key(&id), "decoder should be removed");
    }

    #[tokio::test]
    async fn vad_stop_when_not_active_returns_error() {
        let id = EndpointId::new_v4();
        let mut vad_monitors = HashMap::new();
        let mut vad_decoders: HashMap<EndpointId, Box<dyn crate::media::codec::AudioDecoder>> =
            HashMap::new();

        let result = vad_stop(&mut vad_monitors, &mut vad_decoders, id);
        assert!(result.is_err());
        assert!(
            result.unwrap_err().to_string().contains("not active"),
            "error should mention VAD not active"
        );
    }

    #[tokio::test]
    async fn vad_stop_without_decoder_still_succeeds() {
        // VAD can be active without a decoder (PCMU uses inline decoding, not cached decoders)
        let id = EndpointId::new_v4();
        let mut vad_monitors = HashMap::new();
        vad_monitors.insert(id, VadMonitor::new(8000, 0.5, 1000));

        let mut vad_decoders: HashMap<EndpointId, Box<dyn crate::media::codec::AudioDecoder>> =
            HashMap::new();

        let result = vad_stop(&mut vad_monitors, &mut vad_decoders, id);
        assert!(
            result.is_ok(),
            "vad_stop should succeed even without a decoder entry"
        );
    }

    // ── process_vad tests ───────────────────────────────────────────────

    #[tokio::test]
    async fn process_vad_skips_packets_without_monitors() {
        let (id, ep) = make_rtp_endpoint(Some(sdp::CODEC_PCMU.clone())).await;
        let mut endpoints = HashMap::new();
        endpoints.insert(id, ep);

        let mut vad_monitors = HashMap::new();
        let mut vad_decoders: HashMap<EndpointId, Box<dyn crate::media::codec::AudioDecoder>> =
            HashMap::new();
        let event_tx: Option<mpsc::Sender<Event>> = None;

        let packets = vec![RoutedRtpPacket {
            source_endpoint_id: id,
            payload_type: 0,
            sequence_number: 1,
            timestamp: 160,
            ssrc: 1234,
            marker: false,
            payload: pcmu_silence_payload(160),
        }];

        // No monitors -> should not panic, just skip
        process_vad(
            &packets,
            &endpoints,
            &mut vad_monitors,
            &mut vad_decoders,
            &event_tx,
            &test_dropped(),
            &test_metrics(),
        );
        assert!(
            vad_decoders.is_empty(),
            "no decoders should be created without monitors"
        );
    }

    #[tokio::test]
    async fn process_vad_decodes_pcmu_and_feeds_monitor() {
        let (id, ep) = make_rtp_endpoint(Some(sdp::CODEC_PCMU.clone())).await;
        let mut endpoints = HashMap::new();
        endpoints.insert(id, ep);

        let mut vad_monitors = HashMap::new();
        vad_monitors.insert(id, VadMonitor::new(8000, 0.5, 1000));

        let mut vad_decoders: HashMap<EndpointId, Box<dyn crate::media::codec::AudioDecoder>> =
            HashMap::new();
        let event_tx: Option<mpsc::Sender<Event>> = None;

        // Send enough PCMU silence packets to fill at least one VAD frame (256 samples at 16kHz).
        // At 8kHz, 160 samples get resampled to ~320 at 16kHz which is > 256.
        let packets: Vec<RoutedRtpPacket> = (0..5)
            .map(|i| RoutedRtpPacket {
                source_endpoint_id: id,
                payload_type: 0,
                sequence_number: i,
                timestamp: i as u32 * 160,
                ssrc: 1234,
                marker: i == 0,
                payload: pcmu_silence_payload(160),
            })
            .collect();

        // Should not panic; PCMU uses inline decoding (no entry in vad_decoders)
        process_vad(
            &packets,
            &endpoints,
            &mut vad_monitors,
            &mut vad_decoders,
            &event_tx,
            &test_dropped(),
            &test_metrics(),
        );

        // PCMU uses the inline xlaw decoder path, so vad_decoders stays empty
        assert!(
            vad_decoders.is_empty(),
            "PCMU uses inline decoding, not the cached decoder map"
        );
    }

    #[tokio::test]
    async fn process_vad_creates_decoder_for_g722() {
        let (id, ep) = make_rtp_endpoint(Some(sdp::CODEC_G722.clone())).await;
        let mut endpoints = HashMap::new();
        endpoints.insert(id, ep);

        let mut vad_monitors = HashMap::new();
        vad_monitors.insert(id, VadMonitor::new(16000, 0.5, 1000));

        let mut vad_decoders: HashMap<EndpointId, Box<dyn crate::media::codec::AudioDecoder>> =
            HashMap::new();
        let event_tx: Option<mpsc::Sender<Event>> = None;

        // G.722 encoded silence (zeros work as a valid bitstream for G.722)
        let packets = vec![RoutedRtpPacket {
            source_endpoint_id: id,
            payload_type: 9,
            sequence_number: 1,
            timestamp: 160,
            ssrc: 5678,
            marker: false,
            payload: vec![0u8; 160],
        }];

        process_vad(
            &packets,
            &endpoints,
            &mut vad_monitors,
            &mut vad_decoders,
            &event_tx,
            &test_dropped(),
            &test_metrics(),
        );

        // G.722 goes through the generic codec path and should create a cached decoder
        assert!(
            vad_decoders.contains_key(&id),
            "G.722 should create a decoder in the vad_decoders map"
        );
    }

    #[tokio::test]
    async fn process_vad_emits_events_through_channel() {
        let (id, ep) = make_rtp_endpoint(Some(sdp::CODEC_PCMU.clone())).await;
        let mut endpoints = HashMap::new();
        endpoints.insert(id, ep);

        let mut vad_monitors = HashMap::new();
        vad_monitors.insert(id, VadMonitor::new(8000, 0.5, 50));

        let mut vad_decoders: HashMap<EndpointId, Box<dyn crate::media::codec::AudioDecoder>> =
            HashMap::new();
        let (tx, mut rx) = mpsc::channel::<Event>(64);
        let event_tx = Some(tx);

        // Generate loud PCMU tone to trigger speech detection.
        // mu-law encode a high-amplitude sine wave.
        let encoder = xlaw::PcmXLawEncoder::new_ulaw();
        let pcmu_loud: Vec<u8> = (0..800)
            .map(|i| {
                let sample = (f64::sin(2.0 * std::f64::consts::PI * 440.0 * i as f64 / 8000.0)
                    * 25000.0) as i16;
                encoder.encode(sample)
            })
            .collect();

        // Send several packets of loud audio to trigger SpeechStarted
        let loud_packets: Vec<RoutedRtpPacket> = pcmu_loud
            .chunks(160)
            .enumerate()
            .map(|(i, chunk)| RoutedRtpPacket {
                source_endpoint_id: id,
                payload_type: 0,
                sequence_number: i as u16,
                timestamp: i as u32 * 160,
                ssrc: 9999,
                marker: i == 0,
                payload: chunk.to_vec(),
            })
            .collect();

        process_vad(
            &loud_packets,
            &endpoints,
            &mut vad_monitors,
            &mut vad_decoders,
            &event_tx,
            &test_dropped(),
            &test_metrics(),
        );

        // Check if any events were emitted
        let mut events = Vec::new();
        while let Ok(evt) = rx.try_recv() {
            events.push(evt);
        }

        // We expect at least a speech_started event from loud audio
        let has_speech_event = events.iter().any(|e| e.event == "vad.speech_started");
        assert!(
            has_speech_event,
            "expected vad.speech_started event from loud audio, got events: {:?}",
            events.iter().map(|e| &e.event).collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn process_vad_ignores_packets_from_unknown_sources() {
        // Packet from an endpoint that has no monitor
        let (id, ep) = make_rtp_endpoint(Some(sdp::CODEC_PCMU.clone())).await;
        let other_id = EndpointId::new_v4();
        let mut endpoints = HashMap::new();
        endpoints.insert(id, ep);

        let mut vad_monitors = HashMap::new();
        // Only monitor on `id`, but packet comes from `other_id`
        vad_monitors.insert(id, VadMonitor::new(8000, 0.5, 1000));

        let mut vad_decoders: HashMap<EndpointId, Box<dyn crate::media::codec::AudioDecoder>> =
            HashMap::new();
        let event_tx: Option<mpsc::Sender<Event>> = None;

        let packets = vec![RoutedRtpPacket {
            source_endpoint_id: other_id, // not monitored
            payload_type: 0,
            sequence_number: 1,
            timestamp: 160,
            ssrc: 1234,
            marker: false,
            payload: pcmu_silence_payload(160),
        }];

        process_vad(
            &packets,
            &endpoints,
            &mut vad_monitors,
            &mut vad_decoders,
            &event_tx,
            &test_dropped(),
            &test_metrics(),
        );

        // Should have done nothing
        assert!(vad_decoders.is_empty());
    }

    #[tokio::test]
    async fn process_vad_with_no_event_tx_does_not_panic() {
        let (id, ep) = make_rtp_endpoint(Some(sdp::CODEC_PCMU.clone())).await;
        let mut endpoints = HashMap::new();
        endpoints.insert(id, ep);

        let mut vad_monitors = HashMap::new();
        vad_monitors.insert(id, VadMonitor::new(8000, 0.5, 50));

        let mut vad_decoders: HashMap<EndpointId, Box<dyn crate::media::codec::AudioDecoder>> =
            HashMap::new();
        let event_tx: Option<mpsc::Sender<Event>> = None; // no channel

        // Loud audio that would trigger speech events
        let encoder = xlaw::PcmXLawEncoder::new_ulaw();
        let pcmu_loud: Vec<u8> = (0..160)
            .map(|i| {
                let sample = (f64::sin(2.0 * std::f64::consts::PI * 440.0 * i as f64 / 8000.0)
                    * 25000.0) as i16;
                encoder.encode(sample)
            })
            .collect();

        let packets = vec![RoutedRtpPacket {
            source_endpoint_id: id,
            payload_type: 0,
            sequence_number: 1,
            timestamp: 160,
            ssrc: 1234,
            marker: false,
            payload: pcmu_loud,
        }];

        // Should not panic even though event_tx is None
        process_vad(
            &packets,
            &endpoints,
            &mut vad_monitors,
            &mut vad_decoders,
            &event_tx,
            &test_dropped(),
            &test_metrics(),
        );
    }

    #[tokio::test]
    async fn process_vad_with_empty_packets_does_nothing() {
        let endpoints = HashMap::new();
        let mut vad_monitors = HashMap::new();
        let mut vad_decoders: HashMap<EndpointId, Box<dyn crate::media::codec::AudioDecoder>> =
            HashMap::new();
        let event_tx: Option<mpsc::Sender<Event>> = None;

        process_vad(
            &[],
            &endpoints,
            &mut vad_monitors,
            &mut vad_decoders,
            &event_tx,
            &test_dropped(),
            &test_metrics(),
        );
        // Just verifying no panic
    }

    #[tokio::test]
    async fn process_vad_endpoint_without_codec_skips_decoding() {
        // Endpoint exists and has a monitor, but endpoint_audio_codec returns None
        let (id, ep) = make_rtp_endpoint(None).await; // no send_codec -> from_name(None) -> None
        let mut endpoints = HashMap::new();
        endpoints.insert(id, ep);

        let mut vad_monitors = HashMap::new();
        vad_monitors.insert(id, VadMonitor::new(8000, 0.5, 1000));

        let mut vad_decoders: HashMap<EndpointId, Box<dyn crate::media::codec::AudioDecoder>> =
            HashMap::new();
        let event_tx: Option<mpsc::Sender<Event>> = None;

        let packets = vec![RoutedRtpPacket {
            source_endpoint_id: id,
            payload_type: 0,
            sequence_number: 1,
            timestamp: 160,
            ssrc: 1234,
            marker: false,
            payload: pcmu_silence_payload(160),
        }];

        process_vad(
            &packets,
            &endpoints,
            &mut vad_monitors,
            &mut vad_decoders,
            &event_tx,
            &test_dropped(),
            &test_metrics(),
        );

        // No codec -> None pcm -> nothing fed to monitor, no decoder created
        assert!(vad_decoders.is_empty());
    }
}
