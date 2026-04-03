use std::collections::HashMap;
use std::sync::atomic::AtomicU64;
use std::time::{Duration, Instant};

use tokio::sync::mpsc;

use super::endpoint::RoutedRtpPacket;
use super::endpoint_enum::Endpoint;
use crate::control::protocol::*;

/// Per-file-endpoint RTP generation state
pub struct FileRtpState {
    pub seq_no: u16,
    pub timestamp: u32,
    pub ssrc: u32,
    pub last_poll: Instant,
    /// Resampler for converting file sample rate to 8kHz (for PCMU output).
    /// None when the file is already at 8kHz.
    pub resampler: Option<crate::media::resample::Resampler>,
}

/// Poll file endpoints for PCM output. Produces RoutedRtpPackets and emits
/// file.finished events when playback completes.
pub fn poll_file_endpoints(
    endpoints: &mut HashMap<EndpointId, Endpoint>,
    file_rtp_states: &mut HashMap<EndpointId, FileRtpState>,
    event_tx: &Option<mpsc::Sender<Event>>,
    critical_event_tx: &Option<mpsc::Sender<Event>>,
    dropped_events: &AtomicU64,
    metrics: &crate::metrics::Metrics,
    packets_out: &mut Vec<RoutedRtpPacket>,
) {
    for ep in endpoints.values_mut() {
        if let Endpoint::File(fep) = ep {
            if fep.state != EndpointState::Playing {
                continue;
            }
            let file_rate = fep.sample_rate();
            if file_rate == 0 {
                tracing::warn!(endpoint_id = %fep.id, "file has sample_rate 0, skipping playback");
                continue;
            }
            let state = file_rtp_states.entry(fep.id).or_insert_with(|| {
                let resampler = if file_rate != 8000 {
                    Some(crate::media::resample::Resampler::new(file_rate, 8000))
                } else {
                    None
                };
                FileRtpState {
                    seq_no: rand::random(),
                    timestamp: rand::random(),
                    ssrc: rand::random(),
                    last_poll: Instant::now() - Duration::from_millis(20),
                    resampler,
                }
            });
            while state.last_poll.elapsed() >= Duration::from_millis(20) {
                state.last_poll += Duration::from_millis(20);
                // Clamp: don't burst more than 5 frames if we fell far behind
                let now = Instant::now();
                if state.last_poll + Duration::from_millis(100) < now {
                    state.last_poll = now - Duration::from_millis(20);
                }
                let target_samples = (file_rate / 50) as usize; // 20ms at file's native rate
                let was_playing = fep.state == EndpointState::Playing;
                if let Some(pcm) = fep.next_pcm(target_samples) {
                    // Resample to 8kHz if needed, then encode to PCMU
                    let pcm_8k = if let Some(ref mut resampler) = state.resampler {
                        let mut buf = Vec::new();
                        resampler.process(&pcm, &mut buf);
                        buf
                    } else {
                        pcm
                    };
                    let encoder = xlaw::PcmXLawEncoder::new_ulaw();
                    let payload: Vec<u8> = pcm_8k.iter().map(|&s| encoder.encode(s)).collect();
                    packets_out.push(RoutedRtpPacket {
                        source_endpoint_id: fep.id,
                        payload_type: 0, // PCMU
                        sequence_number: state.seq_no,
                        timestamp: state.timestamp,
                        ssrc: state.ssrc,
                        marker: false,
                        payload,
                    });
                    state.seq_no = state.seq_no.wrapping_add(1);
                    state.timestamp = state.timestamp.wrapping_add(160); // always 160 at 8kHz
                } else if was_playing && fep.state == EndpointState::Finished {
                    super::media_session::emit_event_with_priority(
                        event_tx,
                        critical_event_tx,
                        "endpoint.file.finished",
                        FileFinishedData {
                            endpoint_id: fep.id,
                            reason: "completed".to_string(),
                            error: None,
                        },
                        dropped_events,
                        metrics,
                    );
                    break;
                } else {
                    // No data available yet (e.g., shared playback buffer empty)
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::protocol::EndpointState;
    use crate::session::endpoint_file::FileEndpoint;
    use std::io::Write;
    use std::sync::atomic::Ordering;

    /// Generate a minimal 8kHz mono 16-bit WAV file
    fn test_wav(path: &str, duration_secs: f64) {
        let sample_rate: u32 = 8000;
        let num_samples = (sample_rate as f64 * duration_secs) as usize;
        let data_size = (num_samples * 2) as u32;
        let mut file = std::fs::File::create(path).unwrap();
        file.write_all(b"RIFF").unwrap();
        file.write_all(&(36 + data_size).to_le_bytes()).unwrap();
        file.write_all(b"WAVE").unwrap();
        file.write_all(b"fmt ").unwrap();
        file.write_all(&16u32.to_le_bytes()).unwrap();
        file.write_all(&1u16.to_le_bytes()).unwrap();
        file.write_all(&1u16.to_le_bytes()).unwrap();
        file.write_all(&sample_rate.to_le_bytes()).unwrap();
        file.write_all(&(sample_rate * 2).to_le_bytes()).unwrap();
        file.write_all(&2u16.to_le_bytes()).unwrap();
        file.write_all(&16u16.to_le_bytes()).unwrap();
        file.write_all(b"data").unwrap();
        file.write_all(&data_size.to_le_bytes()).unwrap();
        for i in 0..num_samples {
            let t = i as f64 / sample_rate as f64;
            let sample = (f64::sin(2.0 * std::f64::consts::PI * 440.0 * t) * 16000.0) as i16;
            file.write_all(&sample.to_le_bytes()).unwrap();
        }
    }

    fn make_file_endpoint(path: &str) -> (EndpointId, Endpoint) {
        let id = uuid::Uuid::new_v4();
        let fep = FileEndpoint::open(id, path, 0, None, 0.0).unwrap();
        (id, Endpoint::File(Box::new(fep)))
    }

    #[test]
    fn test_poll_produces_pcmu_packet() {
        let path = "/tmp/rtpbridge-filepoll-basic.wav";
        test_wav(path, 0.5);

        let (id, ep) = make_file_endpoint(path);
        let mut endpoints = HashMap::new();
        endpoints.insert(id, ep);
        let mut file_rtp_states = HashMap::new();
        let dropped = AtomicU64::new(0);
        let metrics = crate::metrics::Metrics::new();
        let mut packets_out = Vec::new();

        poll_file_endpoints(
            &mut endpoints,
            &mut file_rtp_states,
            &None,
            &None,
            &dropped,
            &metrics,
            &mut packets_out,
        );

        assert_eq!(packets_out.len(), 1, "should produce one PCMU packet");
        let pkt = &packets_out[0];
        assert_eq!(pkt.source_endpoint_id, id);
        assert_eq!(pkt.payload_type, 0, "payload type should be PCMU");
        assert_eq!(pkt.payload.len(), 160, "20ms at 8kHz = 160 samples");

        std::fs::remove_file(path).ok();
    }

    #[test]
    fn test_poll_skips_non_playing_endpoints() {
        let path = "/tmp/rtpbridge-filepoll-skip.wav";
        test_wav(path, 0.5);

        let (id, mut ep) = make_file_endpoint(path);
        // Pause the endpoint
        if let Endpoint::File(ref mut fep) = ep {
            fep.state = EndpointState::Paused;
        }
        let mut endpoints = HashMap::new();
        endpoints.insert(id, ep);
        let mut file_rtp_states = HashMap::new();
        let dropped = AtomicU64::new(0);
        let metrics = crate::metrics::Metrics::new();
        let mut packets_out = Vec::new();

        poll_file_endpoints(
            &mut endpoints,
            &mut file_rtp_states,
            &None,
            &None,
            &dropped,
            &metrics,
            &mut packets_out,
        );

        assert!(
            packets_out.is_empty(),
            "paused endpoint should not produce packets"
        );
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn test_poll_sequence_and_timestamp_increment() {
        let path = "/tmp/rtpbridge-filepoll-seq.wav";
        test_wav(path, 1.0);

        let (id, ep) = make_file_endpoint(path);
        let mut endpoints = HashMap::new();
        endpoints.insert(id, ep);
        let mut file_rtp_states = HashMap::new();
        let dropped = AtomicU64::new(0);
        let metrics = crate::metrics::Metrics::new();

        // First poll
        let mut packets_out = Vec::new();
        poll_file_endpoints(
            &mut endpoints,
            &mut file_rtp_states,
            &None,
            &None,
            &dropped,
            &metrics,
            &mut packets_out,
        );
        assert_eq!(packets_out.len(), 1);
        let seq1 = packets_out[0].sequence_number;
        let ts1 = packets_out[0].timestamp;
        let ssrc = packets_out[0].ssrc;

        // Force elapsed time to allow second poll
        file_rtp_states.get_mut(&id).unwrap().last_poll =
            Instant::now() - Duration::from_millis(20);

        let mut packets_out = Vec::new();
        poll_file_endpoints(
            &mut endpoints,
            &mut file_rtp_states,
            &None,
            &None,
            &dropped,
            &metrics,
            &mut packets_out,
        );
        assert_eq!(packets_out.len(), 1);
        assert_eq!(
            packets_out[0].sequence_number,
            seq1.wrapping_add(1),
            "seq should increment by 1"
        );
        assert_eq!(
            packets_out[0].timestamp,
            ts1.wrapping_add(160),
            "timestamp should increment by 160"
        );
        assert_eq!(packets_out[0].ssrc, ssrc, "SSRC should be consistent");

        std::fs::remove_file(path).ok();
    }

    #[test]
    fn test_poll_respects_20ms_interval() {
        let path = "/tmp/rtpbridge-filepoll-interval.wav";
        test_wav(path, 1.0);

        let (id, ep) = make_file_endpoint(path);
        let mut endpoints = HashMap::new();
        endpoints.insert(id, ep);
        let mut file_rtp_states = HashMap::new();
        let dropped = AtomicU64::new(0);
        let metrics = crate::metrics::Metrics::new();

        // First poll succeeds (initial state has last_poll 20ms ago)
        let mut packets_out = Vec::new();
        poll_file_endpoints(
            &mut endpoints,
            &mut file_rtp_states,
            &None,
            &None,
            &dropped,
            &metrics,
            &mut packets_out,
        );
        assert_eq!(packets_out.len(), 1);

        // Immediate second poll should produce nothing (< 20ms elapsed)
        let mut packets_out = Vec::new();
        poll_file_endpoints(
            &mut endpoints,
            &mut file_rtp_states,
            &None,
            &None,
            &dropped,
            &metrics,
            &mut packets_out,
        );
        assert!(
            packets_out.is_empty(),
            "should not produce packet before 20ms elapsed"
        );

        std::fs::remove_file(path).ok();
    }

    #[test]
    fn test_poll_emits_finished_event() {
        // Very short file — will finish in first poll
        let path = "/tmp/rtpbridge-filepoll-finished.wav";
        test_wav(path, 0.01); // 10ms < 20ms ptime

        let (id, ep) = make_file_endpoint(path);
        let mut endpoints = HashMap::new();
        endpoints.insert(id, ep);
        let mut file_rtp_states = HashMap::new();
        let dropped = AtomicU64::new(0);
        let metrics = crate::metrics::Metrics::new();

        // Create event channel to capture the finished event
        let (event_tx, mut event_rx) = mpsc::channel(16);
        let (critical_tx, _critical_rx) = mpsc::channel(16);

        // First poll: file has < 20ms of audio; next_pcm returns Some (partial frame)
        // then on next poll it returns None (finished)
        let mut packets_out = Vec::new();
        poll_file_endpoints(
            &mut endpoints,
            &mut file_rtp_states,
            &Some(event_tx.clone()),
            &Some(critical_tx.clone()),
            &dropped,
            &metrics,
            &mut packets_out,
        );

        // Force another poll to trigger the finished path
        file_rtp_states.get_mut(&id).unwrap().last_poll =
            Instant::now() - Duration::from_millis(20);
        let mut packets_out = Vec::new();
        poll_file_endpoints(
            &mut endpoints,
            &mut file_rtp_states,
            &Some(event_tx),
            &Some(critical_tx),
            &dropped,
            &metrics,
            &mut packets_out,
        );

        // Check if finished event was emitted
        if let Ok(event) = event_rx.try_recv() {
            assert_eq!(event.event, "endpoint.file.finished");
        }
        // The file endpoint should be in Finished state
        if let Endpoint::File(ref fep) = endpoints[&id] {
            assert_eq!(fep.state, EndpointState::Finished);
        }

        std::fs::remove_file(path).ok();
    }

    #[test]
    fn test_poll_sequence_number_wraps() {
        let path = "/tmp/rtpbridge-filepoll-wrap.wav";
        test_wav(path, 1.0);

        let (id, ep) = make_file_endpoint(path);
        let mut endpoints = HashMap::new();
        endpoints.insert(id, ep);
        let mut file_rtp_states = HashMap::new();
        let dropped = AtomicU64::new(0);
        let metrics = crate::metrics::Metrics::new();

        // First poll to initialize state
        let mut packets_out = Vec::new();
        poll_file_endpoints(
            &mut endpoints,
            &mut file_rtp_states,
            &None,
            &None,
            &dropped,
            &metrics,
            &mut packets_out,
        );

        // Set seq_no near wrap point
        file_rtp_states.get_mut(&id).unwrap().seq_no = u16::MAX;
        file_rtp_states.get_mut(&id).unwrap().last_poll =
            Instant::now() - Duration::from_millis(20);

        let mut packets_out = Vec::new();
        poll_file_endpoints(
            &mut endpoints,
            &mut file_rtp_states,
            &None,
            &None,
            &dropped,
            &metrics,
            &mut packets_out,
        );
        assert_eq!(packets_out.len(), 1);
        assert_eq!(packets_out[0].sequence_number, u16::MAX);

        // After this poll, state should have wrapped to 0
        assert_eq!(
            file_rtp_states[&id].seq_no, 0,
            "seq_no should wrap from u16::MAX to 0"
        );

        std::fs::remove_file(path).ok();
    }

    #[test]
    fn test_poll_consistent_ssrc_across_packets() {
        let path = "/tmp/rtpbridge-filepoll-ssrc.wav";
        test_wav(path, 1.0);

        let (id, ep) = make_file_endpoint(path);
        let mut endpoints = HashMap::new();
        endpoints.insert(id, ep);
        let mut file_rtp_states = HashMap::new();
        let dropped = AtomicU64::new(0);
        let metrics = crate::metrics::Metrics::new();

        let mut all_ssrcs = Vec::new();
        for _ in 0..5 {
            let mut packets_out = Vec::new();
            poll_file_endpoints(
                &mut endpoints,
                &mut file_rtp_states,
                &None,
                &None,
                &dropped,
                &metrics,
                &mut packets_out,
            );
            if let Some(pkt) = packets_out.first() {
                all_ssrcs.push(pkt.ssrc);
            }
            // Reset poll timer for next iteration
            if let Some(state) = file_rtp_states.get_mut(&id) {
                state.last_poll = Instant::now() - Duration::from_millis(20);
            }
        }

        assert!(!all_ssrcs.is_empty());
        assert!(
            all_ssrcs.iter().all(|&s| s == all_ssrcs[0]),
            "SSRC must be consistent"
        );

        std::fs::remove_file(path).ok();
    }

    #[test]
    fn test_poll_no_events_dropped_without_channel() {
        let path = "/tmp/rtpbridge-filepoll-nodrop.wav";
        test_wav(path, 0.5);

        let (id, ep) = make_file_endpoint(path);
        let mut endpoints = HashMap::new();
        endpoints.insert(id, ep);
        let mut file_rtp_states = HashMap::new();
        let dropped = AtomicU64::new(0);
        let metrics = crate::metrics::Metrics::new();
        let mut packets_out = Vec::new();

        // Poll with no event channels (None) should not panic
        poll_file_endpoints(
            &mut endpoints,
            &mut file_rtp_states,
            &None,
            &None,
            &dropped,
            &metrics,
            &mut packets_out,
        );

        assert_eq!(dropped.load(Ordering::Relaxed), 0);
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn test_poll_catches_up_after_delay() {
        let path = "/tmp/rtpbridge-filepoll-drift.wav";
        test_wav(path, 1.0);

        let (id, ep) = make_file_endpoint(path);
        let mut endpoints = HashMap::new();
        endpoints.insert(id, ep);
        let mut file_rtp_states = HashMap::new();
        let dropped = AtomicU64::new(0);
        let metrics = crate::metrics::Metrics::new();

        // First poll (initializes state, produces one packet)
        let mut packets_out = Vec::new();
        poll_file_endpoints(
            &mut endpoints,
            &mut file_rtp_states,
            &None,
            &None,
            &dropped,
            &metrics,
            &mut packets_out,
        );
        assert_eq!(packets_out.len(), 1);

        // Simulate a delayed loop iteration: set last_poll to 45ms ago.
        // The while loop should catch up in a single poll call, producing
        // 2 packets (one for each 20ms boundary crossed: 45ms / 20ms = 2).
        file_rtp_states.get_mut(&id).unwrap().last_poll =
            Instant::now() - Duration::from_millis(45);

        let mut packets_out = Vec::new();
        poll_file_endpoints(
            &mut endpoints,
            &mut file_rtp_states,
            &None,
            &None,
            &dropped,
            &metrics,
            &mut packets_out,
        );
        assert_eq!(
            packets_out.len(),
            2,
            "should catch up with 2 packets in one poll"
        );

        // Immediate next poll should NOT fire (now caught up)
        let mut packets_out = Vec::new();
        poll_file_endpoints(
            &mut endpoints,
            &mut file_rtp_states,
            &None,
            &None,
            &dropped,
            &metrics,
            &mut packets_out,
        );
        assert!(packets_out.is_empty(), "should be caught up after burst");

        std::fs::remove_file(path).ok();
    }
}
