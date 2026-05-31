//! Shared inbound-audio analysis: decode each routed RTP packet to PCM **once**
//! and feed it to whichever passive analysers are active on the source endpoint
//! (voice activity detection and/or fax-tone detection).
//!
//! Both analysers consume PCM at the endpoint's source sample rate
//! ([`VadMonitor`] resamples to 16kHz internally; [`FaxDetector`] runs Goertzel
//! at the source rate), so a single decode and a single per-endpoint decoder
//! cache serve both — avoiding a redundant decode (and a second stateful
//! decoder drifting on the same stream) when both are active at once.

use std::collections::HashMap;
use std::sync::atomic::AtomicU64;

use tokio::sync::mpsc;
use tracing::warn;

use super::endpoint::RoutedRtpPacket;
use super::endpoint_enum::{Endpoint, endpoint_audio_codec};
use super::{fax_tap, vad_tap};
use crate::control::protocol::*;
use crate::media::codec::{AudioCodec, AudioDecoder};
use crate::media::fax::FaxDetector;
use crate::media::vad::VadMonitor;

/// Outcome of decoding one routed RTP packet for analysis.
pub enum AnalysisPcm {
    /// Successfully decoded PCM at the endpoint's source sample rate.
    Pcm(Vec<i16>),
    /// Nothing to feed: no known codec, or a decode error on an existing
    /// decoder. Silently skipped (matches prior VAD behavior).
    Empty,
    /// A decoder could not be created for the endpoint's codec. Carries the
    /// error message so the caller can surface a `vad.error` event.
    DecoderInitFailed(String),
}

/// Decode one routed RTP packet to PCM at the endpoint's source sample rate.
///
/// PCMU decodes inline (stateless). Stateful codecs (G.722/Opus) use a
/// per-endpoint cached decoder — there must be exactly one decoder instance per
/// endpoint, fed the whole packet stream in order.
pub fn decode_packet_pcm(
    pkt: &RoutedRtpPacket,
    codec: Option<AudioCodec>,
    decoders: &mut HashMap<EndpointId, Box<dyn AudioDecoder>>,
) -> AnalysisPcm {
    match codec {
        Some(AudioCodec::Pcmu) => {
            let decoder = xlaw::PcmXLawDecoder::new_ulaw();
            AnalysisPcm::Pcm(pkt.payload.iter().map(|&b| decoder.decode(b)).collect())
        }
        Some(codec) => {
            use std::collections::hash_map::Entry;
            let dec = match decoders.entry(pkt.source_endpoint_id) {
                Entry::Occupied(e) => e.into_mut(),
                Entry::Vacant(e) => match crate::media::codec::make_decoder(codec) {
                    Ok(d) => e.insert(d),
                    Err(err) => {
                        warn!(endpoint_id = %pkt.source_endpoint_id, %err, "analysis decoder creation failed");
                        return AnalysisPcm::DecoderInitFailed(err.to_string());
                    }
                },
            };
            let mut pcm = Vec::new();
            match dec.decode(&pkt.payload, &mut pcm) {
                Ok(()) => AnalysisPcm::Pcm(pcm),
                Err(_) => AnalysisPcm::Empty,
            }
        }
        None => AnalysisPcm::Empty,
    }
}

/// Decode each routed audio packet once and feed it to the VAD and/or fax
/// analysers active on its source endpoint. A packet is decoded only when at
/// least one analyser is active on that endpoint.
#[allow(clippy::too_many_arguments)]
pub fn process_analysis(
    packets: &[RoutedRtpPacket],
    endpoints: &HashMap<EndpointId, Endpoint>,
    vad_monitors: &mut HashMap<EndpointId, VadMonitor>,
    fax_detectors: &mut HashMap<EndpointId, FaxDetector>,
    decoders: &mut HashMap<EndpointId, Box<dyn AudioDecoder>>,
    event_tx: &Option<mpsc::Sender<Event>>,
    dropped_events: &AtomicU64,
    metrics: &crate::metrics::Metrics,
) {
    if vad_monitors.is_empty() && fax_detectors.is_empty() {
        return;
    }
    for pkt in packets {
        let src = pkt.source_endpoint_id;
        let want_vad = vad_monitors.contains_key(&src);
        let want_fax = fax_detectors.contains_key(&src);
        if !want_vad && !want_fax {
            continue;
        }

        let codec = endpoints.get(&src).and_then(endpoint_audio_codec);
        let pcm = match decode_packet_pcm(pkt, codec, decoders) {
            AnalysisPcm::Pcm(pcm) => pcm,
            AnalysisPcm::Empty => continue,
            AnalysisPcm::DecoderInitFailed(err) => {
                // Surface the decoder failure to whichever analyser(s) requested
                // the decode. Carriers deliver voice (and thus fax CNG/CED tones)
                // over G.722/Opus, whose decoders can fail to initialise, so the
                // fax side needs its own signal — it is not PCMU-only.
                if want_vad {
                    super::media_session::emit_event(
                        event_tx,
                        "vad.error",
                        serde_json::json!({
                            "endpoint_id": src.to_string(),
                            "error": format!("VAD decoder creation failed: {err}"),
                        }),
                        dropped_events,
                        metrics,
                    );
                }
                if want_fax {
                    super::media_session::emit_event(
                        event_tx,
                        "fax.error",
                        serde_json::json!({
                            "endpoint_id": src.to_string(),
                            "error": format!("Fax detection decoder creation failed: {err}"),
                        }),
                        dropped_events,
                        metrics,
                    );
                }
                continue;
            }
        };

        if want_vad {
            vad_tap::feed_vad(src, &pcm, vad_monitors, event_tx, dropped_events, metrics);
        }
        if want_fax {
            // Rate of the decoded PCM = the endpoint's negotiated codec rate.
            let sample_rate = codec.map(|c| c.sample_rate()).unwrap_or(8000);
            fax_tap::feed_fax(
                src,
                &pcm,
                sample_rate,
                fax_detectors,
                event_tx,
                dropped_events,
                metrics,
            );
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
    use std::f64::consts::PI;

    fn test_dropped() -> AtomicU64 {
        AtomicU64::new(0)
    }
    fn test_metrics() -> crate::metrics::Metrics {
        crate::metrics::Metrics::new()
    }

    async fn test_socket_pair() -> crate::net::socket_pool::SocketPair {
        let pool = SocketPool::new("127.0.0.1".parse().unwrap(), 58000, 58100).unwrap();
        pool.allocate_pair().await.unwrap()
    }

    async fn make_rtp_endpoint(codec: Option<sdp::SdpCodec>) -> (EndpointId, Endpoint) {
        let id = EndpointId::new_v4();
        let pair = test_socket_pair().await;
        let mut ep = RtpEndpoint::new(id, EndpointDirection::SendRecv, pair);
        ep.send_codec = codec;
        (id, Endpoint::Rtp(Box::new(ep)))
    }

    /// Encode a sine tone through a real codec into per-frame RTP packets, so a
    /// detector must decode the carrier codec to see the tone. Models a carrier
    /// delivering voice (and fax CNG/CED tones) over G.722/Opus.
    fn encoded_tone_packets(
        id: EndpointId,
        codec: AudioCodec,
        freq: f64,
        sample_rate: u32,
        frame_samples: usize,
        n_packets: usize,
    ) -> Vec<RoutedRtpPacket> {
        let mut enc = crate::media::codec::make_encoder(codec).unwrap();
        (0..n_packets)
            .map(|p| {
                let pcm: Vec<i16> = (0..frame_samples)
                    .map(|i| {
                        let n = (p * frame_samples + i) as f64;
                        (f64::sin(2.0 * PI * freq * n / sample_rate as f64) * 12000.0) as i16
                    })
                    .collect();
                let mut payload = Vec::new();
                enc.encode(&pcm, &mut payload).unwrap();
                RoutedRtpPacket {
                    source_endpoint_id: id,
                    payload_type: 0,
                    sequence_number: p as u16,
                    timestamp: (p * frame_samples) as u32,
                    ssrc: 1,
                    marker: p == 0,
                    payload,
                }
            })
            .collect()
    }

    fn pcmu_tone_packets(id: EndpointId, freq: f64, packets: usize) -> Vec<RoutedRtpPacket> {
        let encoder = xlaw::PcmXLawEncoder::new_ulaw();
        (0..packets)
            .map(|p| {
                let payload: Vec<u8> = (0..160)
                    .map(|i| {
                        let n = (p * 160 + i) as f64;
                        let s = (f64::sin(2.0 * PI * freq * n / 8000.0) * 12000.0) as i16;
                        encoder.encode(s)
                    })
                    .collect();
                RoutedRtpPacket {
                    source_endpoint_id: id,
                    payload_type: 0,
                    sequence_number: p as u16,
                    timestamp: p as u32 * 160,
                    ssrc: 1234,
                    marker: p == 0,
                    payload,
                }
            })
            .collect()
    }

    #[test]
    fn decode_pcmu_inline_no_cached_decoder() {
        let id = EndpointId::new_v4();
        let mut decoders: HashMap<EndpointId, Box<dyn AudioDecoder>> = HashMap::new();
        let pkt = &pcmu_tone_packets(id, 2100.0, 1)[0];
        match decode_packet_pcm(pkt, Some(AudioCodec::Pcmu), &mut decoders) {
            AnalysisPcm::Pcm(pcm) => assert_eq!(pcm.len(), 160),
            _ => panic!("PCMU should decode to PCM"),
        }
        assert!(
            decoders.is_empty(),
            "PCMU should not allocate a cached decoder"
        );
    }

    #[test]
    fn decode_g722_creates_cached_decoder() {
        let id = EndpointId::new_v4();
        let mut decoders: HashMap<EndpointId, Box<dyn AudioDecoder>> = HashMap::new();
        let pkt = RoutedRtpPacket {
            source_endpoint_id: id,
            payload_type: 9,
            sequence_number: 1,
            timestamp: 160,
            ssrc: 1,
            marker: false,
            payload: vec![0u8; 160],
        };
        assert!(matches!(
            decode_packet_pcm(&pkt, Some(AudioCodec::G722), &mut decoders),
            AnalysisPcm::Pcm(_)
        ));
        assert!(
            decoders.contains_key(&id),
            "G.722 should create a cached decoder"
        );
    }

    #[test]
    fn decode_no_codec_is_empty() {
        let id = EndpointId::new_v4();
        let mut decoders: HashMap<EndpointId, Box<dyn AudioDecoder>> = HashMap::new();
        let pkt = &pcmu_tone_packets(id, 2100.0, 1)[0];
        assert!(matches!(
            decode_packet_pcm(pkt, None, &mut decoders),
            AnalysisPcm::Empty
        ));
    }

    #[tokio::test]
    async fn skips_endpoints_without_any_analyser() {
        let (id, ep) = make_rtp_endpoint(Some(sdp::CODEC_PCMU.clone())).await;
        let mut endpoints = HashMap::new();
        endpoints.insert(id, ep);
        let mut vad = HashMap::new();
        let mut fax = HashMap::new();
        let mut decoders: HashMap<EndpointId, Box<dyn AudioDecoder>> = HashMap::new();
        let event_tx: Option<mpsc::Sender<Event>> = None;

        process_analysis(
            &pcmu_tone_packets(id, 2100.0, 5),
            &endpoints,
            &mut vad,
            &mut fax,
            &mut decoders,
            &event_tx,
            &test_dropped(),
            &test_metrics(),
        );
        assert!(decoders.is_empty(), "no analyser → no decode");
    }

    /// A single decode pass feeds BOTH analysers when both are active on the
    /// same endpoint: a 2100Hz tone yields a fax CED event and (being loud
    /// audio) a VAD speech event.
    #[tokio::test]
    async fn single_decode_feeds_both_vad_and_fax() {
        let (id, ep) = make_rtp_endpoint(Some(sdp::CODEC_PCMU.clone())).await;
        let mut endpoints = HashMap::new();
        endpoints.insert(id, ep);

        let mut vad = HashMap::new();
        vad.insert(id, VadMonitor::new(8000, 0.5, 1000));
        let mut fax = HashMap::new();
        fax.insert(id, FaxDetector::new(8000));
        let mut decoders: HashMap<EndpointId, Box<dyn AudioDecoder>> = HashMap::new();
        let (tx, mut rx) = mpsc::channel::<Event>(64);
        let event_tx = Some(tx);

        process_analysis(
            &pcmu_tone_packets(id, 2100.0, 25), // 500ms
            &endpoints,
            &mut vad,
            &mut fax,
            &mut decoders,
            &event_tx,
            &test_dropped(),
            &test_metrics(),
        );

        let mut events = Vec::new();
        while let Ok(e) = rx.try_recv() {
            events.push(e.event);
        }
        assert!(
            events.iter().any(|e| e == "fax.ced_detected"),
            "expected fax.ced_detected, got {events:?}"
        );
        assert!(
            events.iter().any(|e| e == "vad.speech_started"),
            "expected vad.speech_started from the same decode, got {events:?}"
        );
    }

    /// Carrier delivers fax tones over G.722 — CED must be detected after the
    /// stream is decoded from the carrier codec.
    #[tokio::test]
    async fn detects_ced_through_g722() {
        let (id, ep) = make_rtp_endpoint(Some(sdp::CODEC_G722.clone())).await;
        let mut endpoints = HashMap::new();
        endpoints.insert(id, ep);
        let mut vad = HashMap::new();
        let mut fax = HashMap::new();
        fax.insert(id, FaxDetector::new(16000)); // G.722 audio rate
        let mut decoders: HashMap<EndpointId, Box<dyn AudioDecoder>> = HashMap::new();
        let (tx, mut rx) = mpsc::channel::<Event>(64);
        let event_tx = Some(tx);

        // 320 samples/frame @16kHz = 20ms; 30 frames = 600ms.
        let packets = encoded_tone_packets(id, AudioCodec::G722, 2100.0, 16000, 320, 30);
        process_analysis(
            &packets,
            &endpoints,
            &mut vad,
            &mut fax,
            &mut decoders,
            &event_tx,
            &test_dropped(),
            &test_metrics(),
        );

        let mut events = Vec::new();
        while let Ok(e) = rx.try_recv() {
            events.push(e.event);
        }
        assert!(
            events.iter().any(|e| e == "fax.ced_detected"),
            "CED should be detected through a G.722 carrier, got {events:?}"
        );
        assert!(decoders.contains_key(&id), "G.722 needs a cached decoder");
    }

    /// Carrier delivers fax tones over Opus — CED must be detected after the
    /// stream is decoded from the carrier codec.
    #[tokio::test]
    async fn detects_ced_through_opus() {
        let (id, ep) = make_rtp_endpoint(Some(sdp::CODEC_OPUS.clone())).await;
        let mut endpoints = HashMap::new();
        endpoints.insert(id, ep);
        let mut vad = HashMap::new();
        let mut fax = HashMap::new();
        fax.insert(id, FaxDetector::new(48000)); // Opus audio rate
        let mut decoders: HashMap<EndpointId, Box<dyn AudioDecoder>> = HashMap::new();
        let (tx, mut rx) = mpsc::channel::<Event>(64);
        let event_tx = Some(tx);

        // Opus requires exact frame sizes: 960 samples = 20ms @48kHz; 30 frames = 600ms.
        let packets = encoded_tone_packets(id, AudioCodec::Opus, 2100.0, 48000, 960, 30);
        process_analysis(
            &packets,
            &endpoints,
            &mut vad,
            &mut fax,
            &mut decoders,
            &event_tx,
            &test_dropped(),
            &test_metrics(),
        );

        let mut events = Vec::new();
        while let Ok(e) = rx.try_recv() {
            events.push(e.event);
        }
        assert!(
            events.iter().any(|e| e == "fax.ced_detected"),
            "CED should be detected through an Opus carrier, got {events:?}"
        );
        assert!(decoders.contains_key(&id), "Opus needs a cached decoder");
    }

    #[tokio::test]
    async fn empty_packets_no_panic() {
        let endpoints = HashMap::new();
        let mut vad = HashMap::new();
        vad.insert(EndpointId::new_v4(), VadMonitor::new(8000, 0.5, 1000));
        let mut fax = HashMap::new();
        let mut decoders: HashMap<EndpointId, Box<dyn AudioDecoder>> = HashMap::new();
        let event_tx: Option<mpsc::Sender<Event>> = None;
        process_analysis(
            &[],
            &endpoints,
            &mut vad,
            &mut fax,
            &mut decoders,
            &event_tx,
            &test_dropped(),
            &test_metrics(),
        );
    }
}
