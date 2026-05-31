//! Per-endpoint fax-tone detection tap.
//!
//! Provides the lifecycle (`fax_start`/`fax_stop`) and the PCM feed
//! (`feed_fax`) for fax-tone detection. Inbound RTP is decoded to PCM once by
//! [`super::audio_analysis`] (shared with VAD) and handed here. Detection is
//! purely a notification — the bridge is a media router, so acting on a
//! detected fax (e.g. switching to T.38 or G.711 pass-through) is the
//! controller's responsibility.

use std::collections::HashMap;
use std::sync::atomic::AtomicU64;

use tokio::sync::mpsc;

use super::endpoint_enum::{Endpoint, endpoint_audio_codec};
use crate::control::protocol::*;
use crate::media::fax::{FaxDetector, FaxTone};

/// Start fax-tone detection on an endpoint.
pub fn fax_start(
    endpoints: &HashMap<EndpointId, Endpoint>,
    fax_detectors: &mut HashMap<EndpointId, FaxDetector>,
    endpoint_id: EndpointId,
) -> anyhow::Result<()> {
    let ep = endpoints
        .get(&endpoint_id)
        .ok_or_else(|| anyhow::anyhow!("Endpoint not found"))?;
    let sample_rate = endpoint_audio_codec(ep)
        .map(|c| c.sample_rate())
        .unwrap_or(8000);
    fax_detectors.insert(endpoint_id, FaxDetector::new(sample_rate));
    Ok(())
}

/// Stop fax-tone detection on an endpoint. The shared analysis decoder (if any)
/// is left in place — it is pruned when the endpoint is torn down, and is reused
/// if VAD is still active or fax detection is restarted.
pub fn fax_stop(
    fax_detectors: &mut HashMap<EndpointId, FaxDetector>,
    endpoint_id: EndpointId,
) -> anyhow::Result<()> {
    fax_detectors
        .remove(&endpoint_id)
        .map(|_| ())
        .ok_or_else(|| anyhow::anyhow!("Fax detection not active for endpoint"))
}

/// Re-arm fax detectors whose input has paused (e.g. RTP silence suppression /
/// DTX during the CNG off period), so a later tone burst still fires. Emits no
/// events. Call periodically from the session loop, like `check_vad_timeouts`.
pub fn check_fax_timeouts(fax_detectors: &mut HashMap<EndpointId, FaxDetector>) {
    for det in fax_detectors.values_mut() {
        det.check_timeout();
    }
}

/// Feed already-decoded PCM to its fax detector, emitting `fax.cng_detected` /
/// `fax.ced_detected` events. No-op if the endpoint has no active detector.
///
/// `sample_rate` is the rate of the PCM (i.e. the endpoint's *negotiated* codec
/// rate at decode time). If the detector was created for a different rate — e.g.
/// `fax_detect.start` ran before the SDP answer finalised the codec — it is
/// rebuilt at the correct rate so the Goertzel bins line up. Without this, a
/// detector latched at 8 kHz would silently miss tones in 16/48 kHz audio.
pub fn feed_fax(
    endpoint_id: EndpointId,
    pcm: &[i16],
    sample_rate: u32,
    fax_detectors: &mut HashMap<EndpointId, FaxDetector>,
    event_tx: &Option<mpsc::Sender<Event>>,
    dropped_events: &AtomicU64,
    metrics: &crate::metrics::Metrics,
) {
    let Some(det) = fax_detectors.get_mut(&endpoint_id) else {
        return;
    };
    if det.sample_rate() != sample_rate {
        *det = FaxDetector::new(sample_rate);
    }
    for tone in det.process(pcm) {
        let name = match tone {
            FaxTone::Cng => "fax.cng_detected",
            FaxTone::Ced => "fax.ced_detected",
        };
        super::media_session::emit_event(
            event_tx,
            name,
            FaxDetectedData { endpoint_id },
            dropped_events,
            metrics,
        );
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
        let pool = SocketPool::new("127.0.0.1".parse().unwrap(), 57000, 57100).unwrap();
        pool.allocate_pair().await.unwrap()
    }

    async fn make_rtp_endpoint(codec: Option<sdp::SdpCodec>) -> (EndpointId, Endpoint) {
        let id = EndpointId::new_v4();
        let pair = test_socket_pair().await;
        let mut ep = RtpEndpoint::new(id, EndpointDirection::SendRecv, pair);
        ep.send_codec = codec;
        (id, Endpoint::Rtp(Box::new(ep)))
    }

    /// Sine wave at `freq` (8kHz PCM) — for feeding fax detectors directly.
    fn sine_8k(freq: f64, num_samples: usize) -> Vec<i16> {
        (0..num_samples)
            .map(|i| (f64::sin(2.0 * PI * freq * i as f64 / 8000.0) * 12000.0) as i16)
            .collect()
    }

    fn drain(rx: &mut mpsc::Receiver<Event>) -> Vec<String> {
        let mut out = Vec::new();
        while let Ok(e) = rx.try_recv() {
            out.push(e.event);
        }
        out
    }

    // ── fax_start / fax_stop ─────────────────────────────────────────────

    #[tokio::test]
    async fn fax_start_with_valid_endpoint() {
        let (id, ep) = make_rtp_endpoint(Some(sdp::CODEC_PCMU.clone())).await;
        let mut endpoints = HashMap::new();
        endpoints.insert(id, ep);
        let mut detectors = HashMap::new();
        assert!(fax_start(&endpoints, &mut detectors, id).is_ok());
        assert!(detectors.contains_key(&id));
    }

    #[tokio::test]
    async fn fax_start_missing_endpoint_errors() {
        let endpoints: HashMap<EndpointId, Endpoint> = HashMap::new();
        let mut detectors = HashMap::new();
        let result = fax_start(&endpoints, &mut detectors, EndpointId::new_v4());
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    #[tokio::test]
    async fn fax_stop_removes_detector() {
        let id = EndpointId::new_v4();
        let mut detectors = HashMap::new();
        detectors.insert(id, FaxDetector::new(8000));
        assert!(fax_stop(&mut detectors, id).is_ok());
        assert!(!detectors.contains_key(&id));
    }

    #[tokio::test]
    async fn fax_stop_when_not_active_errors() {
        let mut detectors = HashMap::new();
        let result = fax_stop(&mut detectors, EndpointId::new_v4());
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not active"));
    }

    // ── feed_fax ─────────────────────────────────────────────────────────

    #[tokio::test]
    async fn feed_fax_emits_ced_for_2100hz() {
        let id = EndpointId::new_v4();
        let mut detectors = HashMap::new();
        detectors.insert(id, FaxDetector::new(8000));
        let (tx, mut rx) = mpsc::channel::<Event>(64);
        let event_tx = Some(tx);

        feed_fax(
            id,
            &sine_8k(2100.0, 4000), // 500ms
            8000,
            &mut detectors,
            &event_tx,
            &test_dropped(),
            &test_metrics(),
        );

        let events = drain(&mut rx);
        assert!(
            events.iter().any(|e| e == "fax.ced_detected"),
            "expected fax.ced_detected, got {events:?}"
        );
        assert!(events.iter().all(|e| e != "fax.cng_detected"));
    }

    #[tokio::test]
    async fn feed_fax_emits_cng_for_1100hz() {
        let id = EndpointId::new_v4();
        let mut detectors = HashMap::new();
        detectors.insert(id, FaxDetector::new(8000));
        let (tx, mut rx) = mpsc::channel::<Event>(64);
        let event_tx = Some(tx);

        feed_fax(
            id,
            &sine_8k(1100.0, 4000),
            8000,
            &mut detectors,
            &event_tx,
            &test_dropped(),
            &test_metrics(),
        );

        let events = drain(&mut rx);
        assert!(
            events.iter().any(|e| e == "fax.cng_detected"),
            "expected fax.cng_detected, got {events:?}"
        );
    }

    #[tokio::test]
    async fn feed_fax_no_detector_is_noop() {
        let mut detectors = HashMap::new();
        let event_tx: Option<mpsc::Sender<Event>> = None;
        feed_fax(
            EndpointId::new_v4(),
            &sine_8k(2100.0, 4000),
            8000,
            &mut detectors,
            &event_tx,
            &test_dropped(),
            &test_metrics(),
        );
    }

    #[tokio::test]
    async fn feed_fax_no_event_tx_does_not_panic() {
        let id = EndpointId::new_v4();
        let mut detectors = HashMap::new();
        detectors.insert(id, FaxDetector::new(8000));
        let event_tx: Option<mpsc::Sender<Event>> = None;
        feed_fax(
            id,
            &sine_8k(2100.0, 4000),
            8000,
            &mut detectors,
            &event_tx,
            &test_dropped(),
            &test_metrics(),
        );
    }

    #[tokio::test]
    async fn feed_fax_rebuilds_detector_on_rate_change() {
        // Detector latched at 8kHz (e.g. fax_detect.start before the SDP answer
        // selected G.722), but PCM now arrives at 16kHz. feed_fax must rebuild
        // at the real rate so the Goertzel bins line up and the tone is found.
        let id = EndpointId::new_v4();
        let mut detectors = HashMap::new();
        detectors.insert(id, FaxDetector::new(8000));
        let (tx, mut rx) = mpsc::channel::<Event>(64);
        let event_tx = Some(tx);

        // 16kHz 2100Hz tone, 500ms = 8000 samples.
        let pcm: Vec<i16> = (0..8000)
            .map(|i| (f64::sin(2.0 * PI * 2100.0 * i as f64 / 16000.0) * 12000.0) as i16)
            .collect();
        feed_fax(
            id,
            &pcm,
            16000,
            &mut detectors,
            &event_tx,
            &test_dropped(),
            &test_metrics(),
        );

        assert_eq!(
            detectors.get(&id).unwrap().sample_rate(),
            16000,
            "detector should be rebuilt at the PCM's actual rate"
        );
        let events = drain(&mut rx);
        assert!(
            events.iter().any(|e| e == "fax.ced_detected"),
            "CED should be detected once the detector matches the audio rate, got {events:?}"
        );
    }
}
