//! Source-owned decoder and resamplers, shared by mixing, transcoding and analysis.
use super::endpoint::RoutedRtpPacket;
use crate::media::{
    codec::{self, AudioCodec, AudioDecoder},
    pcm_frames::PcmFrames,
    resample::Resampler,
};
use anyhow::Result;
use std::collections::HashMap;
use std::sync::Arc;

pub struct SourceAudio {
    decoder: Box<dyn AudioDecoder>,
    epoch: Option<(u32, u16, u32)>, // SSRC, last sequence, expected timestamp
    framer: PcmFrames,
    timestamp: u32,
    marker: bool,
    resamplers: HashMap<u32, Resampler>,
}
pub struct PcmFrame {
    pub timestamp: u32,
    pub marker: bool,
    pub rates: HashMap<u32, Arc<Vec<i16>>>,
}
impl SourceAudio {
    pub fn new(codec: AudioCodec) -> Result<Self> {
        Ok(Self::from_decoder(codec::make_decoder(codec)?))
    }
    pub fn from_decoder(decoder: Box<dyn AudioDecoder>) -> Self {
        let rate = decoder.codec().sample_rate();
        Self {
            decoder,
            epoch: None,
            framer: PcmFrames::new(rate),
            timestamp: 0,
            marker: true,
            resamplers: HashMap::new(),
        }
    }
    pub fn codec(&self) -> AudioCodec {
        self.decoder.codec()
    }
    pub fn decode(&mut self, packet: &RoutedRtpPacket) -> Result<Vec<i16>> {
        let codec = self.codec();
        anyhow::ensure!(
            packet.payload.len() <= 16 * 1024,
            "encoded audio packet too large"
        );
        let discontinuity = self.epoch.is_some_and(|(ssrc, seq, ts)| {
            ssrc != packet.ssrc
                || seq.wrapping_add(1) != packet.sequence_number
                || ts != packet.timestamp
        });
        if discontinuity {
            self.decoder = codec::make_decoder(codec)?;
            self.framer.clear();
            self.resamplers.clear();
        }
        if self.framer.is_empty() {
            self.timestamp = packet.timestamp;
            self.marker = packet.marker || discontinuity || self.epoch.is_none();
        }
        let mut pcm = Vec::new();
        self.decoder.decode(&packet.payload, &mut pcm)?;
        anyhow::ensure!(
            pcm.len() <= codec.ptime_samples() * 6,
            "audio packet exceeds 120 ms"
        );
        let duration =
            (pcm.len() as u64 * codec.rtp_clock_rate() as u64 / codec.sample_rate() as u64) as u32;
        self.epoch = Some((
            packet.ssrc,
            packet.sequence_number,
            packet.timestamp.wrapping_add(duration),
        ));
        Ok(pcm)
    }
    pub fn frames(&mut self, pcm: &[i16], rates: &[u32]) -> Result<Vec<PcmFrame>> {
        let source_rate = self.codec().sample_rate();
        let increment = self.codec().rtp_clock_rate() / 50;
        let frames = self.framer.push(pcm)?;
        let mut output = Vec::with_capacity(frames.len());
        for samples in frames {
            let mut converted = HashMap::new();
            for &rate in rates {
                anyhow::ensure!(
                    matches!(rate, 8000 | 16000 | 48000),
                    "unsupported media rate"
                );
                if rate == source_rate || converted.contains_key(&rate) {
                    continue;
                }
                let resampler = self
                    .resamplers
                    .entry(rate)
                    .or_insert_with(|| Resampler::new(source_rate, rate));
                let mut buffer = Vec::new();
                resampler.process(&samples, &mut buffer);
                anyhow::ensure!(
                    buffer.len() == (rate / 50) as usize,
                    "resampler frame mismatch"
                );
                converted.insert(rate, Arc::new(buffer));
            }
            converted.insert(source_rate, Arc::new(samples));
            output.push(PcmFrame {
                timestamp: self.timestamp,
                marker: self.marker,
                rates: converted,
            });
            self.timestamp = self.timestamp.wrapping_add(increment);
            self.marker = false;
        }
        Ok(output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::protocol::EndpointId;
    #[test]
    fn legal_packet_durations_preserve_samples_and_rollover() {
        for codec in [
            AudioCodec::Pcmu,
            AudioCodec::G722,
            AudioCodec::Opus,
            AudioCodec::L16 { sample_rate: 16000 },
        ] {
            let durations: &[usize] = if codec == AudioCodec::Opus {
                &[120, 240, 480, 960, 1920, 2880]
            } else {
                &[
                    codec.ptime_samples() / 2,
                    codec.ptime_samples(),
                    codec.ptime_samples() * 2,
                    codec.ptime_samples() * 3,
                ]
            };
            for &samples in durations {
                let mut encoder = codec::make_encoder(codec).unwrap();
                let mut source = SourceAudio::new(codec).unwrap();
                let mut timestamp = u32::MAX - 100;
                let mut expected = timestamp;
                let mut produced = 0;
                for sequence in 0..8 {
                    let mut payload = Vec::new();
                    encoder.encode(&vec![1234; samples], &mut payload).unwrap();
                    let packet = RoutedRtpPacket {
                        source_endpoint_id: EndpointId::nil(),
                        payload_type: 0,
                        sequence_number: sequence,
                        timestamp,
                        ssrc: 1,
                        marker: sequence == 0,
                        payload,
                    };
                    let pcm = source.decode(&packet).unwrap();
                    assert_eq!(pcm.len(), samples);
                    let frames = source.frames(&pcm, &[8000, 16000, 48000]).unwrap();
                    for frame in frames {
                        assert_eq!(frame.timestamp, expected, "{codec:?} at {samples} samples");
                        for (&rate, pcm) in &frame.rates {
                            assert_eq!(pcm.len(), (rate / 50) as usize);
                        }
                        expected = expected.wrapping_add(codec.rtp_clock_rate() / 50);
                        produced += codec.ptime_samples();
                    }
                    timestamp = timestamp.wrapping_add(
                        (samples as u64 * codec.rtp_clock_rate() as u64
                            / codec.sample_rate() as u64) as u32,
                    );
                }
                assert_eq!(produced, samples * 8, "{codec:?} at {samples} samples");
            }
        }
    }

    #[test]
    fn new_ssrc_discards_only_old_partial_frame() {
        let codec = AudioCodec::L16 { sample_rate: 8000 };
        let mut source = SourceAudio::new(codec).unwrap();
        let mut packet = RoutedRtpPacket {
            source_endpoint_id: EndpointId::nil(),
            payload_type: 127,
            sequence_number: 1,
            timestamp: 0,
            ssrc: 1,
            marker: false,
            payload: vec![0; 160],
        };
        let pcm = source.decode(&packet).unwrap();
        assert!(source.frames(&pcm, &[]).unwrap().is_empty());
        packet.ssrc = 2;
        packet.timestamp = 12345;
        packet.payload = (0..160).flat_map(|_| 2345i16.to_le_bytes()).collect();
        let pcm = source.decode(&packet).unwrap();
        let frames = source.frames(&pcm, &[]).unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].timestamp, 12345);
        assert!(frames[0].marker);
        assert!(frames[0].rates[&8000].iter().all(|&sample| sample == 2345));
    }
}
