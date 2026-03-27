use std::collections::HashMap;

use anyhow::Result;

use crate::control::protocol::EndpointId;
use crate::media::codec::{self, AudioCodec, AudioDecoder, AudioEncoder};
use crate::media::resample::Resampler;

use super::endpoint::RoutedRtpPacket;

/// Per-source decode state inside a destination mixer.
struct SourceState {
    decoder: Box<dyn AudioDecoder>,
    /// Resamples from source sample_rate to dest sample_rate (None if same).
    resampler: Option<Resampler>,
    /// Decoded + resampled PCM for the current frame.
    pcm_buffer: Vec<i16>,
    /// Whether this source contributed to the current frame.
    contributed: bool,
}

/// Per-destination audio mixer for multi-party conferences.
///
/// When a destination receives from 2+ sources, raw packet forwarding causes
/// timestamp oscillation (each source has an independent RTP clock). The mixer
/// decodes all sources to PCM, sums them with saturation, encodes the result,
/// and emits packets with proper monotonic timestamps.
///
/// Frame boundaries are detected by packet arrival: when a source that already
/// contributed feeds again, a new 20ms period has started. The accumulated
/// frame is mixed, queued, and contributions reset. This keeps the mixer
/// naturally synchronized with source timing without wall-clock pacing.
pub struct DestinationMixer {
    dest_codec: AudioCodec,
    dest_pt: u8,
    encoder: Box<dyn AudioEncoder>,
    sources: HashMap<EndpointId, SourceState>,
    /// Accumulated mixed PCM.
    mix_buffer: Vec<i16>,
    /// Encoder output buffer.
    encode_buffer: Vec<u8>,
    /// Monotonically increasing RTP timestamp for output.
    rtp_timestamp: u32,
    /// PCM samples per 20ms frame at the destination's sample rate.
    frame_samples: usize,
    /// RTP timestamp increment per frame (rtp_clock_rate / 50).
    clock_increment: u32,
    /// Number of frames emitted (for marker bit on first frame).
    frames_emitted: u64,
    /// Queued output packets ready for delivery.
    output_queue: Vec<RoutedRtpPacket>,
}

impl DestinationMixer {
    /// Create a new mixer that outputs encoded audio for `dest_codec`.
    pub fn new(dest_codec: AudioCodec, dest_pt: u8) -> Result<Self> {
        let encoder = codec::make_encoder(dest_codec)?;
        let frame_samples = dest_codec.ptime_samples();
        let clock_increment = dest_codec.rtp_clock_rate() / 50; // 20ms

        Ok(Self {
            dest_codec,
            dest_pt,
            encoder,
            sources: HashMap::new(),
            mix_buffer: vec![0i16; frame_samples],
            encode_buffer: Vec::with_capacity(frame_samples * 2),
            rtp_timestamp: rand::random(),
            frame_samples,
            clock_increment,
            frames_emitted: 0,
            output_queue: Vec::new(),
        })
    }

    /// Ensure a decoder exists for the given source, creating one if needed.
    fn ensure_source(
        &mut self,
        source_id: EndpointId,
        source_codec: AudioCodec,
    ) -> Result<&mut SourceState> {
        if !self.sources.contains_key(&source_id) {
            let decoder = codec::make_decoder(source_codec)?;
            let resampler = if source_codec.sample_rate() != self.dest_codec.sample_rate() {
                Some(Resampler::new(
                    source_codec.sample_rate(),
                    self.dest_codec.sample_rate(),
                ))
            } else {
                None
            };
            self.sources.insert(
                source_id,
                SourceState {
                    decoder,
                    resampler,
                    pcm_buffer: Vec::with_capacity(self.frame_samples),
                    contributed: false,
                },
            );
        }
        Ok(self.sources.get_mut(&source_id).unwrap())
    }

    /// Feed a packet from a source endpoint into the mixer.
    ///
    /// If this source already contributed to the current accumulation, a new
    /// frame period has started — the previous frame is mixed, encoded, and
    /// queued before the new packet is processed.
    pub fn feed(
        &mut self,
        source_id: EndpointId,
        source_codec: AudioCodec,
        payload: &[u8],
    ) -> Result<()> {
        let frame_samples = self.frame_samples;

        // Check if this source already contributed (= new frame boundary).
        // Must check before ensure_source to handle existing sources.
        let is_new_frame = self.sources.get(&source_id).is_some_and(|s| s.contributed);

        if is_new_frame {
            self.flush_frame()?;
        }

        let source = self.ensure_source(source_id, source_codec)?;

        // Decode to PCM
        source.decoder.decode(payload, &mut source.pcm_buffer)?;

        // Resample if needed
        if let Some(ref mut resampler) = source.resampler {
            let mut resampled = Vec::with_capacity(frame_samples);
            resampler.process(&source.pcm_buffer, &mut resampled);
            source.pcm_buffer = resampled;
        }

        // Pad or truncate to exact frame size
        source.pcm_buffer.resize(frame_samples, 0);
        source.contributed = true;

        Ok(())
    }

    /// Mix all contributed sources into an output packet and queue it.
    fn flush_frame(&mut self) -> Result<()> {
        let any_contributed = self.sources.values().any(|s| s.contributed);
        if !any_contributed {
            return Ok(());
        }

        // Zero the mix buffer
        self.mix_buffer.iter_mut().for_each(|s| *s = 0);

        // Sum all contributing sources with saturation
        for source in self.sources.values() {
            if source.contributed {
                for (i, &sample) in source.pcm_buffer.iter().enumerate() {
                    if i < self.mix_buffer.len() {
                        self.mix_buffer[i] = saturating_add(self.mix_buffer[i], sample);
                    }
                }
            }
        }

        // Encode mixed PCM
        self.encoder
            .encode(&self.mix_buffer, &mut self.encode_buffer)?;

        let marker = self.frames_emitted == 0;
        let timestamp = self.rtp_timestamp;

        // Advance state
        self.rtp_timestamp = self.rtp_timestamp.wrapping_add(self.clock_increment);
        self.frames_emitted += 1;

        // Reset source contribution flags
        for source in self.sources.values_mut() {
            source.contributed = false;
        }

        // Queue output
        self.output_queue.push(RoutedRtpPacket {
            source_endpoint_id: EndpointId::nil(),
            payload_type: self.dest_pt,
            sequence_number: 0, // destination endpoint replaces this
            timestamp,
            ssrc: 0, // destination endpoint replaces this
            marker,
            payload: self.encode_buffer.clone(),
        });

        Ok(())
    }

    /// Drain all queued output packets for delivery to the destination endpoint.
    pub fn drain(&mut self) -> impl Iterator<Item = RoutedRtpPacket> + '_ {
        self.output_queue.drain(..)
    }

    /// Force-flush the current accumulated frame and drain all output.
    /// Used in tests and when the mixer is being torn down.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn flush(&mut self) -> Result<Vec<RoutedRtpPacket>> {
        self.flush_frame()?;
        Ok(std::mem::take(&mut self.output_queue))
    }

    /// Seed the output timestamp to continue from a previous value.
    /// Call this when transitioning from passthrough to mixing so the
    /// receiver's jitter buffer doesn't see a discontinuity.
    pub fn continue_from_timestamp(&mut self, last_ts: u32) {
        self.rtp_timestamp = last_ts.wrapping_add(self.clock_increment);
    }

    /// Remove a source from the mixer (endpoint removed or no longer routes here).
    pub fn remove_source(&mut self, source_id: &EndpointId) {
        self.sources.remove(source_id);
    }

    /// Retain only sources in the given set.
    pub fn retain_sources(&mut self, active: &std::collections::HashSet<EndpointId>) {
        self.sources.retain(|id, _| active.contains(id));
    }

    /// Number of registered sources.
    #[cfg(test)]
    pub fn source_count(&self) -> usize {
        self.sources.len()
    }
}

/// Saturating i16 addition (avoids wrap-around distortion).
#[inline]
fn saturating_add(a: i16, b: i16) -> i16 {
    (a as i32 + b as i32).clamp(i16::MIN as i32, i16::MAX as i32) as i16
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::codec::{AudioEncoder, G722Encoder, PcmuEncoder};
    use uuid::Uuid;

    fn eid() -> EndpointId {
        Uuid::new_v4()
    }

    /// Generate a 20ms PCMU-encoded sine tone (160 bytes).
    fn pcmu_tone(freq: f64, offset: usize) -> Vec<u8> {
        let mut enc = PcmuEncoder::new();
        let pcm: Vec<i16> = (0..160)
            .map(|i| {
                let t = (offset + i) as f64 / 8000.0;
                (f64::sin(2.0 * std::f64::consts::PI * freq * t) * 8000.0) as i16
            })
            .collect();
        let mut out = Vec::new();
        enc.encode(&pcm, &mut out).unwrap();
        out
    }

    /// Generate a 20ms G.722-encoded sine tone.
    fn g722_tone(freq: f64, offset: usize) -> Vec<u8> {
        let mut enc = G722Encoder::new();
        let pcm: Vec<i16> = (0..320)
            .map(|i| {
                let t = (offset + i) as f64 / 16000.0;
                (f64::sin(2.0 * std::f64::consts::PI * freq * t) * 8000.0) as i16
            })
            .collect();
        let mut out = Vec::new();
        enc.encode(&pcm, &mut out).unwrap();
        out
    }

    #[test]
    fn test_mix_two_pcmu_sources() {
        let mut mixer = DestinationMixer::new(AudioCodec::Pcmu, 0).unwrap();
        let src_a = eid();
        let src_b = eid();

        // Frame 1: both sources feed
        mixer
            .feed(src_a, AudioCodec::Pcmu, &pcmu_tone(440.0, 0))
            .unwrap();
        mixer
            .feed(src_b, AudioCodec::Pcmu, &pcmu_tone(880.0, 0))
            .unwrap();

        // Frame 2 starts: src_a feeds again → flushes frame 1
        mixer
            .feed(src_a, AudioCodec::Pcmu, &pcmu_tone(440.0, 160))
            .unwrap();

        let pkts = mixer.flush().unwrap();
        // Frame 1 (flushed by feed) + frame 2 (flushed by flush())
        assert!(pkts.len() >= 1, "should produce at least 1 packet");
        assert_eq!(pkts[0].payload.len(), 160, "PCMU 20ms = 160 bytes");
        assert_eq!(pkts[0].payload_type, 0);
        assert!(pkts[0].marker, "first frame should have marker bit");
    }

    #[test]
    fn test_mix_two_g722_sources() {
        let mut mixer = DestinationMixer::new(AudioCodec::G722, 9).unwrap();
        let src_a = eid();
        let src_b = eid();

        mixer
            .feed(src_a, AudioCodec::G722, &g722_tone(440.0, 0))
            .unwrap();
        mixer
            .feed(src_b, AudioCodec::G722, &g722_tone(880.0, 0))
            .unwrap();
        // Trigger flush
        mixer
            .feed(src_a, AudioCodec::G722, &g722_tone(440.0, 320))
            .unwrap();

        let pkts = mixer.flush().unwrap();
        assert!(!pkts.is_empty());
        assert!(!pkts[0].payload.is_empty(), "should produce G.722 output");
        assert_eq!(pkts[0].payload_type, 9);
    }

    #[test]
    fn test_cross_codec_mixing() {
        let mut mixer = DestinationMixer::new(AudioCodec::Pcmu, 0).unwrap();
        let src_pcmu = eid();
        let src_g722 = eid();

        mixer
            .feed(src_pcmu, AudioCodec::Pcmu, &pcmu_tone(440.0, 0))
            .unwrap();
        mixer
            .feed(src_g722, AudioCodec::G722, &g722_tone(880.0, 0))
            .unwrap();

        let pkts = mixer.flush().unwrap();
        assert_eq!(pkts.len(), 1);
        assert_eq!(pkts[0].payload.len(), 160, "PCMU output");
    }

    #[test]
    fn test_single_source_contributing() {
        let mut mixer = DestinationMixer::new(AudioCodec::Pcmu, 0).unwrap();
        let src_a = eid();
        let src_b = eid();

        // Frame 1: both contribute
        mixer
            .feed(src_a, AudioCodec::Pcmu, &pcmu_tone(440.0, 0))
            .unwrap();
        mixer
            .feed(src_b, AudioCodec::Pcmu, &pcmu_tone(880.0, 0))
            .unwrap();

        // Frame 2: only src_a (triggers flush of frame 1)
        mixer
            .feed(src_a, AudioCodec::Pcmu, &pcmu_tone(440.0, 160))
            .unwrap();

        // Frame 3: src_a again (triggers flush of frame 2, which only had src_a)
        mixer
            .feed(src_a, AudioCodec::Pcmu, &pcmu_tone(440.0, 320))
            .unwrap();

        let pkts = mixer.flush().unwrap();
        // frame 1 + frame 2 + frame 3
        assert!(pkts.len() >= 2, "should produce frames for both periods");
        assert_eq!(pkts[1].payload.len(), 160);
    }

    #[test]
    fn test_no_sources_returns_empty() {
        let mut mixer = DestinationMixer::new(AudioCodec::Pcmu, 0).unwrap();
        let pkts = mixer.flush().unwrap();
        assert!(pkts.is_empty(), "no sources fed → no output");
    }

    #[test]
    fn test_saturation() {
        let mut mixer = DestinationMixer::new(AudioCodec::Pcmu, 0).unwrap();
        let src_a = eid();
        let src_b = eid();

        let mut enc = PcmuEncoder::new();
        let loud_pcm: Vec<i16> = vec![i16::MAX; 160];
        let mut loud_payload = Vec::new();
        enc.encode(&loud_pcm, &mut loud_payload).unwrap();

        mixer.feed(src_a, AudioCodec::Pcmu, &loud_payload).unwrap();
        mixer.feed(src_b, AudioCodec::Pcmu, &loud_payload).unwrap();

        let pkts = mixer.flush().unwrap();
        assert_eq!(pkts.len(), 1);
        assert_eq!(pkts[0].payload.len(), 160);
    }

    #[test]
    fn test_timestamp_monotonic() {
        let mut mixer = DestinationMixer::new(AudioCodec::Pcmu, 0).unwrap();
        let src = eid();

        // Feed 6 frames — each new feed from same source triggers a flush
        for i in 0..6 {
            mixer
                .feed(src, AudioCodec::Pcmu, &pcmu_tone(440.0, i * 160))
                .unwrap();
        }
        let pkts = mixer.flush().unwrap();

        // 5 flushed by feed + 1 flushed by flush = 6, but first feed doesn't flush
        assert!(
            pkts.len() >= 5,
            "should have at least 5 frames, got {}",
            pkts.len()
        );
        for i in 1..pkts.len() {
            let diff = pkts[i].timestamp.wrapping_sub(pkts[i - 1].timestamp);
            assert_eq!(
                diff, 160,
                "PCMU timestamp increment should be 160 per 20ms frame"
            );
        }
    }

    #[test]
    fn test_timestamp_monotonic_g722() {
        let mut mixer = DestinationMixer::new(AudioCodec::G722, 9).unwrap();
        let src = eid();

        for i in 0..6 {
            mixer
                .feed(src, AudioCodec::G722, &g722_tone(440.0, i * 320))
                .unwrap();
        }
        let pkts = mixer.flush().unwrap();

        assert!(pkts.len() >= 5);
        for i in 1..pkts.len() {
            let diff = pkts[i].timestamp.wrapping_sub(pkts[i - 1].timestamp);
            assert_eq!(diff, 160, "G.722 RTP timestamp increment should be 160");
        }
    }

    #[test]
    fn test_marker_bit_only_on_first_frame() {
        let mut mixer = DestinationMixer::new(AudioCodec::Pcmu, 0).unwrap();
        let src = eid();

        mixer
            .feed(src, AudioCodec::Pcmu, &pcmu_tone(440.0, 0))
            .unwrap();
        mixer
            .feed(src, AudioCodec::Pcmu, &pcmu_tone(440.0, 160))
            .unwrap();
        mixer
            .feed(src, AudioCodec::Pcmu, &pcmu_tone(440.0, 320))
            .unwrap();
        let pkts = mixer.flush().unwrap();

        assert!(pkts.len() >= 2);
        assert!(pkts[0].marker, "first frame should have marker");
        assert!(!pkts[1].marker, "subsequent frames should not have marker");
    }

    #[test]
    fn test_remove_source() {
        let mut mixer = DestinationMixer::new(AudioCodec::Pcmu, 0).unwrap();
        let src_a = eid();
        let src_b = eid();

        mixer
            .feed(src_a, AudioCodec::Pcmu, &pcmu_tone(440.0, 0))
            .unwrap();
        mixer
            .feed(src_b, AudioCodec::Pcmu, &pcmu_tone(880.0, 0))
            .unwrap();
        assert_eq!(mixer.source_count(), 2);

        mixer.remove_source(&src_a);
        assert_eq!(mixer.source_count(), 1);

        // Mixer still works with remaining source
        mixer
            .feed(src_b, AudioCodec::Pcmu, &pcmu_tone(880.0, 160))
            .unwrap();
        let pkts = mixer.flush().unwrap();
        assert!(!pkts.is_empty());
    }

    #[test]
    fn test_all_dest_codecs() {
        for (codec, pt, expected_min_len) in [
            (AudioCodec::Pcmu, 0u8, 160usize),
            (AudioCodec::G722, 9, 100),
            (AudioCodec::Opus, 111, 1),
        ] {
            let mut mixer = DestinationMixer::new(codec, pt).unwrap();
            let src = eid();
            mixer
                .feed(src, AudioCodec::Pcmu, &pcmu_tone(440.0, 0))
                .unwrap();
            let pkts = mixer.flush().unwrap();
            assert_eq!(pkts.len(), 1);
            assert!(
                pkts[0].payload.len() >= expected_min_len,
                "{:?} output too short: {} < {}",
                codec,
                pkts[0].payload.len(),
                expected_min_len
            );
            assert_eq!(pkts[0].payload_type, pt);
        }
    }

    #[test]
    fn test_saturating_add_function() {
        assert_eq!(saturating_add(i16::MAX, 1), i16::MAX);
        assert_eq!(saturating_add(i16::MIN, -1), i16::MIN);
        assert_eq!(saturating_add(100, 200), 300);
        assert_eq!(saturating_add(-100, -200), -300);
        assert_eq!(saturating_add(i16::MAX, i16::MAX), i16::MAX);
        assert_eq!(saturating_add(i16::MIN, i16::MIN), i16::MIN);
    }
}
