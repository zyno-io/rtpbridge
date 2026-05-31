use std::collections::HashMap;
use std::sync::atomic::AtomicU64;

use tokio::sync::mpsc;

use super::endpoint_enum::{Endpoint, endpoint_audio_codec};
use crate::control::protocol::*;
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

/// Stop VAD monitoring on an endpoint. The shared analysis decoder (if any) is
/// left in place — it is pruned when the endpoint is torn down, and is reused if
/// fax detection is still active or VAD is restarted.
pub fn vad_stop(
    vad_monitors: &mut HashMap<EndpointId, VadMonitor>,
    endpoint_id: EndpointId,
) -> anyhow::Result<()> {
    vad_monitors
        .remove(&endpoint_id)
        .map(|_| ())
        .ok_or_else(|| anyhow::anyhow!("VAD not active for endpoint"))
}

/// Check VAD monitors for timeout-based speech→silence transitions.
/// Call periodically (e.g. every second) to handle the case where an endpoint
/// stops sending packets while the VAD is in the speaking state.
pub fn check_vad_timeouts(
    vad_monitors: &mut HashMap<EndpointId, VadMonitor>,
    event_tx: &Option<mpsc::Sender<Event>>,
    dropped_events: &AtomicU64,
    metrics: &crate::metrics::Metrics,
) {
    for (endpoint_id, vad) in vad_monitors.iter_mut() {
        for vad_event in vad.check_timeout() {
            match vad_event {
                crate::media::vad::VadEvent::SpeechStarted => {
                    super::media_session::emit_event(
                        event_tx,
                        "vad.speech_started",
                        VadSpeechStartedData {
                            endpoint_id: *endpoint_id,
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
                            endpoint_id: *endpoint_id,
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

/// Feed already-decoded PCM (at the endpoint's source sample rate) to its VAD
/// monitor, emitting `vad.speech_started` / `vad.silence` events. No-op if the
/// endpoint has no active monitor. Decoding is handled once by
/// [`super::audio_analysis`] so VAD and fax detection share a single decode.
pub fn feed_vad(
    endpoint_id: EndpointId,
    pcm: &[i16],
    vad_monitors: &mut HashMap<EndpointId, VadMonitor>,
    event_tx: &Option<mpsc::Sender<Event>>,
    dropped_events: &AtomicU64,
    metrics: &crate::metrics::Metrics,
) {
    let Some(vad) = vad_monitors.get_mut(&endpoint_id) else {
        return;
    };
    for vad_event in vad.process(pcm) {
        match vad_event {
            crate::media::vad::VadEvent::SpeechStarted => {
                super::media_session::emit_event(
                    event_tx,
                    "vad.speech_started",
                    VadSpeechStartedData { endpoint_id },
                    dropped_events,
                    metrics,
                );
            }
            crate::media::vad::VadEvent::Silence { duration_ms } => {
                super::media_session::emit_event(
                    event_tx,
                    "vad.silence",
                    VadSilenceData {
                        endpoint_id,
                        silence_duration_ms: duration_ms,
                    },
                    dropped_events,
                    metrics,
                );
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
    use std::f64::consts::PI;
    use tokio::sync::mpsc;

    fn test_dropped() -> AtomicU64 {
        AtomicU64::new(0)
    }

    fn test_metrics() -> crate::metrics::Metrics {
        crate::metrics::Metrics::new()
    }

    async fn test_socket_pair() -> crate::net::socket_pool::SocketPair {
        let pool = SocketPool::new("127.0.0.1".parse().unwrap(), 56000, 56100).unwrap();
        pool.allocate_pair().await.unwrap()
    }

    async fn make_rtp_endpoint(codec: Option<sdp::SdpCodec>) -> (EndpointId, Endpoint) {
        let id = EndpointId::new_v4();
        let pair = test_socket_pair().await;
        let mut ep = RtpEndpoint::new(id, EndpointDirection::SendRecv, pair);
        ep.send_codec = codec;
        (id, Endpoint::Rtp(Box::new(ep)))
    }

    /// A loud 440Hz sine at 16kHz — triggers VAD speech detection.
    fn loud_pcm_16k(num_samples: usize) -> Vec<i16> {
        (0..num_samples)
            .map(|i| (f64::sin(2.0 * PI * 440.0 * i as f64 / 16000.0) * 25000.0) as i16)
            .collect()
    }

    // ── vad_start ────────────────────────────────────────────────────────

    #[tokio::test]
    async fn vad_start_with_valid_endpoint() {
        let (id, ep) = make_rtp_endpoint(Some(sdp::CODEC_PCMU.clone())).await;
        let mut endpoints = HashMap::new();
        endpoints.insert(id, ep);
        let mut vad_monitors = HashMap::new();

        assert!(vad_start(&endpoints, &mut vad_monitors, id, 1000, 0.5).is_ok());
        assert!(vad_monitors.contains_key(&id));
    }

    #[tokio::test]
    async fn vad_start_missing_endpoint_returns_error() {
        let endpoints: HashMap<EndpointId, Endpoint> = HashMap::new();
        let mut vad_monitors = HashMap::new();
        let result = vad_start(
            &endpoints,
            &mut vad_monitors,
            EndpointId::new_v4(),
            1000,
            0.5,
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    #[tokio::test]
    async fn vad_start_defaults_to_8000_when_no_codec() {
        let (id, ep) = make_rtp_endpoint(None).await;
        let mut endpoints = HashMap::new();
        endpoints.insert(id, ep);
        let mut vad_monitors = HashMap::new();
        assert!(vad_start(&endpoints, &mut vad_monitors, id, 1000, 0.5).is_ok());
        assert!(vad_monitors.contains_key(&id));
    }

    #[tokio::test]
    async fn vad_start_replaces_existing_monitor() {
        let (id, ep) = make_rtp_endpoint(Some(sdp::CODEC_PCMU.clone())).await;
        let mut endpoints = HashMap::new();
        endpoints.insert(id, ep);
        let mut vad_monitors = HashMap::new();
        vad_start(&endpoints, &mut vad_monitors, id, 1000, 0.5).unwrap();
        vad_start(&endpoints, &mut vad_monitors, id, 500, 0.8).unwrap();
        assert_eq!(vad_monitors.len(), 1);
    }

    // ── vad_stop ─────────────────────────────────────────────────────────

    #[tokio::test]
    async fn vad_stop_removes_monitor() {
        let id = EndpointId::new_v4();
        let mut vad_monitors = HashMap::new();
        vad_monitors.insert(id, VadMonitor::new(8000, 0.5, 1000));
        assert!(vad_stop(&mut vad_monitors, id).is_ok());
        assert!(!vad_monitors.contains_key(&id));
    }

    #[tokio::test]
    async fn vad_stop_when_not_active_returns_error() {
        let mut vad_monitors = HashMap::new();
        let result = vad_stop(&mut vad_monitors, EndpointId::new_v4());
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not active"));
    }

    // ── feed_vad ─────────────────────────────────────────────────────────

    #[tokio::test]
    async fn feed_vad_emits_speech_started() {
        let id = EndpointId::new_v4();
        let mut vad_monitors = HashMap::new();
        vad_monitors.insert(id, VadMonitor::new(16000, 0.5, 1000));
        let (tx, mut rx) = mpsc::channel::<Event>(64);
        let event_tx = Some(tx);

        feed_vad(
            id,
            &loud_pcm_16k(8000), // 500ms of loud tone
            &mut vad_monitors,
            &event_tx,
            &test_dropped(),
            &test_metrics(),
        );

        let mut events = Vec::new();
        while let Ok(e) = rx.try_recv() {
            events.push(e.event);
        }
        assert!(
            events.iter().any(|e| e == "vad.speech_started"),
            "expected vad.speech_started, got {events:?}"
        );
    }

    #[tokio::test]
    async fn feed_vad_no_monitor_is_noop() {
        let mut vad_monitors = HashMap::new();
        let event_tx: Option<mpsc::Sender<Event>> = None;
        // No monitor for this endpoint — must not panic.
        feed_vad(
            EndpointId::new_v4(),
            &loud_pcm_16k(4096),
            &mut vad_monitors,
            &event_tx,
            &test_dropped(),
            &test_metrics(),
        );
    }

    #[tokio::test]
    async fn feed_vad_no_event_tx_does_not_panic() {
        let id = EndpointId::new_v4();
        let mut vad_monitors = HashMap::new();
        vad_monitors.insert(id, VadMonitor::new(16000, 0.5, 1000));
        let event_tx: Option<mpsc::Sender<Event>> = None;
        feed_vad(
            id,
            &loud_pcm_16k(4096),
            &mut vad_monitors,
            &event_tx,
            &test_dropped(),
            &test_metrics(),
        );
    }
}
