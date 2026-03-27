use std::collections::HashMap;
use std::sync::atomic::AtomicU64;
use std::time::{Duration, Instant};

use tokio::sync::mpsc;

use super::endpoint::RoutedRtpPacket;
use super::endpoint_enum::Endpoint;
use crate::control::protocol::*;

/// Per-tone-endpoint RTP generation state (same shape as FileRtpState).
pub struct ToneRtpState {
    pub seq_no: u16,
    pub timestamp: u32,
    pub ssrc: u32,
    pub last_poll: Instant,
}

/// Poll tone endpoints for PCM output. Produces RoutedRtpPackets and emits
/// endpoint.tone.finished events when a duration-limited tone completes.
pub fn poll_tone_endpoints(
    endpoints: &mut HashMap<EndpointId, Endpoint>,
    tone_rtp_states: &mut HashMap<EndpointId, ToneRtpState>,
    event_tx: &Option<mpsc::Sender<Event>>,
    critical_event_tx: &Option<mpsc::Sender<Event>>,
    dropped_events: &AtomicU64,
    metrics: &crate::metrics::Metrics,
    packets_out: &mut Vec<RoutedRtpPacket>,
) {
    for ep in endpoints.values_mut() {
        if let Endpoint::Tone(tep) = ep {
            if tep.state != EndpointState::Playing {
                continue;
            }
            let state = tone_rtp_states
                .entry(tep.id)
                .or_insert_with(|| ToneRtpState {
                    seq_no: rand::random(),
                    timestamp: rand::random(),
                    ssrc: rand::random(),
                    last_poll: Instant::now() - Duration::from_millis(20),
                });
            while state.last_poll.elapsed() >= Duration::from_millis(20) {
                state.last_poll += Duration::from_millis(20);
                // Clamp: don't burst more than 5 frames if we fell far behind
                let now = Instant::now();
                if state.last_poll + Duration::from_millis(100) < now {
                    state.last_poll = now - Duration::from_millis(20);
                }
                let was_playing = tep.state == EndpointState::Playing;
                if let Some(pcm) = tep.next_pcm(160) {
                    // Encode to PCMU
                    let encoder = xlaw::PcmXLawEncoder::new_ulaw();
                    let payload: Vec<u8> = pcm.iter().map(|&s| encoder.encode(s)).collect();
                    packets_out.push(RoutedRtpPacket {
                        source_endpoint_id: tep.id,
                        payload_type: 0, // PCMU
                        sequence_number: state.seq_no,
                        timestamp: state.timestamp,
                        ssrc: state.ssrc,
                        marker: false,
                        payload,
                    });
                    state.seq_no = state.seq_no.wrapping_add(1);
                    state.timestamp = state.timestamp.wrapping_add(160);
                } else if was_playing && tep.state == EndpointState::Finished {
                    super::media_session::emit_event_with_priority(
                        event_tx,
                        critical_event_tx,
                        "endpoint.tone.finished",
                        ToneFinishedData {
                            endpoint_id: tep.id,
                        },
                        dropped_events,
                        metrics,
                    );
                    break;
                } else {
                    break;
                }
            }
        }
    }
}
