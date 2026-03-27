use std::collections::HashMap;
use std::sync::atomic::AtomicU64;
use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use tracing::warn;

use super::endpoint::RoutedRtpPacket;
use super::endpoint_enum::{Endpoint, endpoint_rtp_clock_rate};
use super::routing::RoutingTable;
use crate::control::protocol::*;
use crate::media::dtmf::{self, DtmfDetector, DtmfGenerator};

/// Per-endpoint DTMF state
pub struct EndpointDtmf {
    pub detector: DtmfDetector,
    pub te_pt: Option<u8>,
}

/// Queued DTMF injection that drains one packet per 20ms tick,
/// keeping the session event loop non-blocking.
#[derive(Debug)]
pub struct PendingDtmfInjection {
    pub endpoint_id: EndpointId,
    pub packets: Vec<RoutedRtpPacket>,
    pub next_index: usize,
    pub next_send: Instant,
}

/// Build a DTMF injection from the given parameters. Returns the injection
/// to be stored in session state.
pub fn build_dtmf_injection(
    endpoints: &HashMap<EndpointId, Endpoint>,
    existing_injection: &Option<PendingDtmfInjection>,
    endpoint_id: &EndpointId,
    digit: char,
    duration_ms: u32,
    volume: u8,
) -> anyhow::Result<PendingDtmfInjection> {
    if existing_injection.is_some() {
        anyhow::bail!("DTMF injection already in progress");
    }

    let ep = endpoints
        .get(endpoint_id)
        .ok_or_else(|| anyhow::anyhow!("Endpoint not found"))?;

    if matches!(ep, Endpoint::File(_) | Endpoint::Tone(_)) {
        anyhow::bail!("Cannot inject DTMF into file endpoint");
    }

    let te_pt = ep
        .telephone_event_pt()
        .ok_or_else(|| anyhow::anyhow!("No telephone-event PT negotiated"))?;

    let clock_rate = endpoint_rtp_clock_rate(ep);
    let dtmf_packets = DtmfGenerator::generate(digit, duration_ms, volume, clock_rate, 20)
        .ok_or_else(|| anyhow::anyhow!("Invalid DTMF digit: {digit}"))?;

    let timestamp = rand::random::<u32>();
    let ssrc = rand::random::<u32>();
    let base_seq: u16 = rand::random();

    let routed_packets: Vec<RoutedRtpPacket> = dtmf_packets
        .iter()
        .enumerate()
        .map(|(i, dtmf_pkt)| RoutedRtpPacket {
            source_endpoint_id: *endpoint_id,
            payload_type: te_pt,
            sequence_number: base_seq.wrapping_add(i as u16),
            timestamp,
            ssrc,
            marker: dtmf_pkt.marker,
            payload: dtmf_pkt.payload.clone(),
        })
        .collect();

    Ok(PendingDtmfInjection {
        endpoint_id: *endpoint_id,
        packets: routed_packets,
        next_index: 0,
        next_send: Instant::now(),
    })
}

/// Drain pending DTMF injection one packet at a time (non-blocking).
/// Returns true if the injection is complete and should be cleared.
pub async fn drain_dtmf_injection(
    injection: &mut PendingDtmfInjection,
    endpoints: &mut HashMap<EndpointId, Endpoint>,
) -> bool {
    let now = Instant::now();
    while injection.next_index < injection.packets.len() && now >= injection.next_send {
        let pkt = &injection.packets[injection.next_index];
        if let Some(ep) = endpoints.get_mut(&injection.endpoint_id) {
            let result = match ep {
                Endpoint::WebRtc(wep) => wep.write_rtp(pkt),
                Endpoint::Rtp(rep) => rep.write_rtp(pkt).await,
                Endpoint::File(_) | Endpoint::Tone(_) | Endpoint::Bridge(_) => Ok(()),
            };
            if let Err(e) = result {
                warn!(endpoint_id = %injection.endpoint_id, error = %e, "DTMF inject write error");
            }
        }
        injection.next_index += 1;
        injection.next_send += Duration::from_millis(20);
    }
    injection.next_index >= injection.packets.len()
}

/// Detect DTMF events and forward DTMF packets to destinations.
pub async fn process_dtmf_packets(
    dtmf_packets: &[RoutedRtpPacket],
    dtmf_state: &mut HashMap<EndpointId, EndpointDtmf>,
    routing: &RoutingTable,
    endpoints: &mut HashMap<EndpointId, Endpoint>,
    event_tx: &Option<mpsc::Sender<Event>>,
    dropped_events: &AtomicU64,
    metrics: &crate::metrics::Metrics,
) {
    for pkt in dtmf_packets {
        // Run DTMF detector
        if let Some(ds) = dtmf_state.get_mut(&pkt.source_endpoint_id) {
            let clock_rate = endpoints
                .get(&pkt.source_endpoint_id)
                .map(endpoint_rtp_clock_rate)
                .unwrap_or(8000);
            if let Some(evt) = ds.detector.process(&pkt.payload, pkt.timestamp, clock_rate) {
                metrics.dtmf_events.inc();
                super::media_session::emit_event(
                    event_tx,
                    "dtmf",
                    DtmfEventData {
                        endpoint_id: pkt.source_endpoint_id,
                        digit: evt.digit.to_string(),
                        duration_ms: evt.duration_ms,
                    },
                    dropped_events,
                    metrics,
                );
            }
        }

        // Forward DTMF to destinations (no transcoding, just PT remap if needed)
        if let Some(dests) = routing.destinations(&pkt.source_endpoint_id) {
            for &dest_id in dests {
                if let Some(dest_ep) = endpoints.get_mut(&dest_id) {
                    let mut forwarded = pkt.clone();
                    if let Some(te_pt) = dest_ep.telephone_event_pt() {
                        forwarded.payload_type = te_pt;
                    }
                    let result = match dest_ep {
                        Endpoint::WebRtc(wep) => wep.write_rtp(&forwarded),
                        Endpoint::Rtp(rep) => rep.write_rtp(&forwarded).await,
                        Endpoint::File(_) | Endpoint::Tone(_) | Endpoint::Bridge(_) => Ok(()),
                    };
                    if let Err(e) = result {
                        warn!(dst = %dest_id, error = %e, "DTMF forward error");
                    }
                }
            }
        }
    }
}

/// Check all DTMF detectors for timed-out events (end-bit lost in transit).
/// Call this periodically from the session event loop.
pub fn check_dtmf_timeouts(
    dtmf_state: &mut HashMap<EndpointId, EndpointDtmf>,
    endpoints: &HashMap<EndpointId, Endpoint>,
    event_tx: &Option<mpsc::Sender<Event>>,
    dropped_events: &AtomicU64,
    metrics: &crate::metrics::Metrics,
) {
    for (eid, ds) in dtmf_state.iter_mut() {
        let clock_rate = endpoints
            .get(eid)
            .map(endpoint_rtp_clock_rate)
            .unwrap_or(8000);
        if let Some(evt) = ds.detector.check_timeout(clock_rate) {
            metrics.dtmf_events.inc();
            super::media_session::emit_event(
                event_tx,
                "dtmf",
                DtmfEventData {
                    endpoint_id: *eid,
                    digit: evt.digit.to_string(),
                    duration_ms: evt.duration_ms,
                },
                dropped_events,
                metrics,
            );
        }
    }
}

/// Returns true if any DTMF detector has an active (in-progress) digit.
/// Used by the session loop to ensure 20ms tick frequency for timeout detection.
pub fn has_active_dtmf(dtmf_state: &HashMap<EndpointId, EndpointDtmf>) -> bool {
    dtmf_state.values().any(|ds| ds.detector.has_active_digit())
}

/// Classify inbound packets as DTMF or audio based on payload type.
pub fn classify_dtmf(
    pkt: &RoutedRtpPacket,
    dtmf_state: &HashMap<EndpointId, EndpointDtmf>,
) -> bool {
    let te_pt = dtmf_state
        .get(&pkt.source_endpoint_id)
        .and_then(|ds| ds.te_pt);
    dtmf::is_telephone_event_pt(pkt.payload_type, te_pt)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_routed_packet(endpoint_id: EndpointId, payload_type: u8) -> RoutedRtpPacket {
        RoutedRtpPacket {
            source_endpoint_id: endpoint_id,
            payload_type,
            sequence_number: 0,
            timestamp: 0,
            ssrc: 0,
            marker: false,
            payload: vec![0; 4],
        }
    }

    fn make_dtmf_state(
        endpoint_id: EndpointId,
        te_pt: Option<u8>,
    ) -> HashMap<EndpointId, EndpointDtmf> {
        let mut state = HashMap::new();
        state.insert(
            endpoint_id,
            EndpointDtmf {
                detector: DtmfDetector::new(),
                te_pt,
            },
        );
        state
    }

    #[test]
    fn test_classify_dtmf_known_te_pt() {
        let ep_id = EndpointId::new_v4();
        let state = make_dtmf_state(ep_id, Some(101));
        let pkt = make_routed_packet(ep_id, 101);
        assert!(
            classify_dtmf(&pkt, &state),
            "PT 101 should match known te_pt"
        );

        let pkt_other = make_routed_packet(ep_id, 0);
        assert!(
            !classify_dtmf(&pkt_other, &state),
            "PT 0 should not match te_pt 101"
        );
    }

    #[test]
    fn test_classify_dtmf_no_te_pt_uses_defaults() {
        let ep_id = EndpointId::new_v4();
        let state = make_dtmf_state(ep_id, None);

        // Default telephone-event PTs: 101, 96, 126
        assert!(classify_dtmf(&make_routed_packet(ep_id, 101), &state));
        assert!(classify_dtmf(&make_routed_packet(ep_id, 96), &state));
        assert!(classify_dtmf(&make_routed_packet(ep_id, 126), &state));

        // Non-default PT should not match
        assert!(!classify_dtmf(&make_routed_packet(ep_id, 97), &state));
    }

    #[test]
    fn test_classify_dtmf_unknown_endpoint() {
        let ep_id = EndpointId::new_v4();
        let other_ep = EndpointId::new_v4();
        let state = make_dtmf_state(ep_id, Some(101));

        // Packet from an unknown endpoint — te_pt is None, so falls back to defaults
        let pkt = make_routed_packet(other_ep, 101);
        assert!(
            classify_dtmf(&pkt, &state),
            "unknown endpoint with default PT 101 should match"
        );

        let pkt2 = make_routed_packet(other_ep, 97);
        assert!(
            !classify_dtmf(&pkt2, &state),
            "unknown endpoint with PT 97 should not match"
        );
    }

    #[test]
    fn test_classify_dtmf_audio_packet() {
        let ep_id = EndpointId::new_v4();
        let state = make_dtmf_state(ep_id, Some(101));

        // Common audio PTs should not be classified as DTMF
        assert!(
            !classify_dtmf(&make_routed_packet(ep_id, 0), &state),
            "PCMU (PT 0) is not DTMF"
        );
        assert!(
            !classify_dtmf(&make_routed_packet(ep_id, 8), &state),
            "PCMA (PT 8) is not DTMF"
        );
        assert!(
            !classify_dtmf(&make_routed_packet(ep_id, 9), &state),
            "G722 (PT 9) is not DTMF"
        );
    }

    #[test]
    fn test_build_injection_already_in_progress() {
        let endpoints = HashMap::new();
        let existing = Some(PendingDtmfInjection {
            endpoint_id: EndpointId::new_v4(),
            packets: vec![],
            next_index: 0,
            next_send: Instant::now(),
        });
        let ep_id = EndpointId::new_v4();

        let result = build_dtmf_injection(&endpoints, &existing, &ep_id, '5', 100, 10);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("already in progress")
        );
    }

    #[test]
    fn test_build_injection_endpoint_not_found() {
        let endpoints: HashMap<EndpointId, Endpoint> = HashMap::new();
        let ep_id = EndpointId::new_v4();

        let result = build_dtmf_injection(&endpoints, &None, &ep_id, '5', 100, 10);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Endpoint not found")
        );
    }
}
