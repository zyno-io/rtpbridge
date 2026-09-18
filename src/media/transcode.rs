use anyhow::Result;

use super::codec::{self, AudioCodec, AudioDecoder, AudioEncoder};
use super::resample::Resampler;

/// A transcode pipeline: decode source codec → resample → encode to destination codec.
/// If source and destination codecs are the same, operates in passthrough mode.
#[allow(dead_code)] // standalone packet-transcoding API is also used by embeddings/benchmarks
pub struct TranscodePipeline {
    decoder: Option<Box<dyn AudioDecoder>>,
    source: AudioCodec,
    timeline: Option<(u32, u32)>,
    encoder: Box<dyn AudioEncoder>,
    resampler: Option<Resampler>,
    passthrough: bool,
    decode_buf: Vec<i16>,
    resample_buf: Vec<i16>,
    encode_buf: Vec<u8>,
    framer: super::pcm_frames::PcmFrames,
}

#[allow(dead_code)]
impl TranscodePipeline {
    pub fn matches_codecs(&self, source: AudioCodec, destination: AudioCodec) -> bool {
        self.source == source && self.encoder.codec() == destination
    }

    /// Create a new transcode pipeline between two codecs.
    /// If they're the same codec, this is a no-op passthrough.
    pub fn new(from: AudioCodec, to: AudioCodec) -> Result<Self> {
        let passthrough = from == to;

        let decoder = codec::make_decoder(from)?;
        let encoder = codec::make_encoder(to)?;

        let resampler = if !passthrough && from.sample_rate() != to.sample_rate() {
            Some(Resampler::new(from.sample_rate(), to.sample_rate()))
        } else {
            None
        };

        Ok(Self {
            decoder: Some(decoder),
            source: from,
            timeline: None,
            encoder,
            resampler,
            passthrough,
            decode_buf: Vec::with_capacity(960),
            resample_buf: Vec::with_capacity(960),
            encode_buf: Vec::with_capacity(960),
            framer: super::pcm_frames::PcmFrames::new(to.sample_rate()),
        })
    }

    pub fn for_pcm(from: AudioCodec, to: AudioCodec) -> Result<Self> {
        Ok(Self {
            decoder: None,
            source: from,
            timeline: None,
            encoder: codec::make_encoder(to)?,
            resampler: None,
            passthrough: false,
            decode_buf: Vec::new(),
            resample_buf: Vec::new(),
            encode_buf: Vec::new(),
            framer: super::pcm_frames::PcmFrames::new(to.sample_rate()),
        })
    }

    /// Returns true if no transcoding is needed
    #[allow(dead_code)] // used in tests
    pub fn is_passthrough(&self) -> bool {
        self.passthrough
    }

    /// Transcode encoded audio from source codec to destination codec.
    /// In passthrough mode, returns the input unchanged.
    pub fn process(&mut self, input: &[u8]) -> Result<&[u8]> {
        if self.passthrough {
            self.encode_buf.clear();
            self.encode_buf.extend_from_slice(input);
            return Ok(&self.encode_buf);
        }

        // Decode source codec → PCM
        self.decoder
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("encoder-only pipeline"))?
            .decode(input, &mut self.decode_buf)?;

        // Resample if needed
        if let Some(resampler) = &mut self.resampler {
            resampler.process(&self.decode_buf, &mut self.resample_buf);
        } else {
            self.resample_buf.clear();
            self.resample_buf.extend_from_slice(&self.decode_buf);
        }

        let target_samples = self.encoder.codec().ptime_samples();
        anyhow::ensure!(
            self.resample_buf.len() == target_samples,
            "process requires 20 ms input; use process_frames for variable packet durations"
        );
        self.encoder
            .encode(&self.resample_buf, &mut self.encode_buf)?;
        Ok(&self.encode_buf)
    }

    /// Zero or many complete output frames, preserving partial input across calls.
    pub fn process_frames(&mut self, input: &[u8]) -> Result<Vec<Vec<u8>>> {
        if self.passthrough {
            return Ok(vec![input.to_vec()]);
        }
        self.decoder
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("encoder-only pipeline"))?
            .decode(input, &mut self.decode_buf)?;
        if let Some(resampler) = &mut self.resampler {
            resampler.process(&self.decode_buf, &mut self.resample_buf);
        } else {
            self.resample_buf.clear();
            self.resample_buf.extend_from_slice(&self.decode_buf);
        }
        let frames = self.framer.push(&self.resample_buf)?;
        let mut output = Vec::with_capacity(frames.len());
        for frame in frames {
            self.encoder.encode(&frame, &mut self.encode_buf)?;
            output.push(self.encode_buf.clone());
        }
        Ok(output)
    }

    pub fn map_timestamp(&mut self, timestamp: u32, source_rate: u32, dest_rate: u32) -> u32 {
        let mapped = if let Some((previous, output)) = self.timeline {
            let delta = timestamp.wrapping_sub(previous);
            output
                .wrapping_add((delta as u64 * dest_rate as u64 / source_rate.max(1) as u64) as u32)
        } else {
            (timestamp as u64 * dest_rate as u64 / source_rate.max(1) as u64) as u32
        };
        self.timeline = Some((timestamp, mapped));
        mapped
    }

    /// Encode a source-shared frame already at the destination's PCM rate.
    pub fn encode_pcm(&mut self, samples: &[i16]) -> Result<&[u8]> {
        anyhow::ensure!(
            samples.len() == self.encoder.codec().ptime_samples(),
            "encoder requires 20 ms PCM"
        );
        self.encoder.encode(samples, &mut self.encode_buf)?;
        Ok(&self.encode_buf)
    }

    #[allow(dead_code)] // used in tests
    pub fn source_codec(&self) -> AudioCodec {
        self.source
    }

    #[allow(dead_code)] // used in tests
    pub fn dest_codec(&self) -> AudioCodec {
        self.encoder.codec()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_passthrough() {
        let mut pipeline = TranscodePipeline::new(AudioCodec::Pcmu, AudioCodec::Pcmu).unwrap();
        assert!(pipeline.is_passthrough());

        let input = vec![0x55u8; 160];
        let output = pipeline.process(&input).unwrap();
        assert_eq!(output, &input[..]);
    }

    #[test]
    fn test_pcmu_to_g722() {
        let mut pipeline = TranscodePipeline::new(AudioCodec::Pcmu, AudioCodec::G722).unwrap();
        assert!(!pipeline.is_passthrough());

        // Encode a sine wave as PCMU
        let mut pcmu_enc = codec::PcmuEncoder::new();
        let pcm: Vec<i16> = (0..160)
            .map(|i| ((i as f64 * 0.1).sin() * 5000.0) as i16)
            .collect();
        let mut pcmu_data = Vec::new();
        pcmu_enc.encode(&pcm, &mut pcmu_data).unwrap();

        // Transcode PCMU → G.722
        let g722_output = pipeline.process(&pcmu_data).unwrap();
        assert!(!g722_output.is_empty(), "expected non-empty G.722 output");

        // Roundtrip: transcode G.722 → PCMU and verify signal is preserved
        let mut reverse = TranscodePipeline::new(AudioCodec::G722, AudioCodec::Pcmu).unwrap();
        let roundtrip = reverse.process(&g722_output).unwrap();
        assert!(!roundtrip.is_empty(), "roundtrip should produce output");

        // Decode both to PCM and compare sign of samples (signal shape preserved)
        let mut dec = codec::PcmuDecoder::new();
        let mut original_pcm = Vec::new();
        dec.decode(&pcmu_data, &mut original_pcm).unwrap();
        let mut roundtrip_pcm = Vec::new();
        dec.decode(&roundtrip, &mut roundtrip_pcm).unwrap();

        let min_len = original_pcm.len().min(roundtrip_pcm.len());
        assert!(min_len > 0, "both should have samples");
        // G.722 filtering and causal resampling introduce delay. Compare the
        // signal after alignment rather than treating delay as distortion.
        let match_pct = (0..32)
            .map(|delay| {
                let count = min_len.saturating_sub(delay);
                let matches = (0..count)
                    .filter(|&i| {
                        original_pcm[i].signum() == roundtrip_pcm[i + delay].signum()
                            || original_pcm[i] == 0
                    })
                    .count();
                matches as f64 / count as f64
            })
            .fold(0.0, f64::max);
        assert!(
            match_pct > 0.9,
            "signal shape should be roughly preserved, got {:.0}% sign match",
            match_pct * 100.0
        );
    }

    #[test]
    fn test_opus_to_pcmu() {
        let mut pipeline = TranscodePipeline::new(AudioCodec::Opus, AudioCodec::Pcmu).unwrap();
        assert!(!pipeline.is_passthrough());

        // Encode some Opus first
        let mut opus_enc = codec::OpusEncoder::new().unwrap();
        let pcm: Vec<i16> = (0..960)
            .map(|i| ((i as f64 * 0.01).sin() * 5000.0) as i16)
            .collect();
        let mut opus_data = Vec::new();
        opus_enc.encode(&pcm, &mut opus_data).unwrap();

        // Transcode Opus → PCMU
        let output = pipeline.process(&opus_data).unwrap();
        assert!(output.len() > 0, "expected non-empty PCMU output");
        // 960 Opus samples at 48kHz → 160 PCMU samples at 8kHz
        assert!(
            output.len() >= 155 && output.len() <= 170,
            "expected ~160 PCMU bytes, got {}",
            output.len()
        );
    }

    #[test]
    fn test_pcmu_to_opus() {
        let mut pipeline = TranscodePipeline::new(AudioCodec::Pcmu, AudioCodec::Opus).unwrap();

        // 160 PCMU samples → 960 Opus samples (8kHz→48kHz)
        let mut pcmu_enc = codec::PcmuEncoder::new();
        let pcm: Vec<i16> = (0..160)
            .map(|i| ((i as f64 * 0.1).sin() * 5000.0) as i16)
            .collect();
        let mut pcmu_data = Vec::new();
        pcmu_enc.encode(&pcm, &mut pcmu_data).unwrap();

        let output = pipeline.process(&pcmu_data).unwrap();
        assert!(output.len() > 0, "expected non-empty Opus output");
    }

    #[test]
    fn test_g722_to_opus() {
        let mut pipeline = TranscodePipeline::new(AudioCodec::G722, AudioCodec::Opus).unwrap();

        // Encode G.722 first
        let mut g722_enc = codec::G722Encoder::new();
        let pcm: Vec<i16> = (0..320)
            .map(|i| ((i as f64 * 0.05).sin() * 5000.0) as i16)
            .collect();
        let mut g722_data = Vec::new();
        g722_enc.encode(&pcm, &mut g722_data).unwrap();

        let output = pipeline.process(&g722_data).unwrap();
        assert!(output.len() > 0, "expected non-empty Opus output");
    }

    #[test]
    fn short_input_accumulates_without_padding() {
        let mut pipeline = TranscodePipeline::new(AudioCodec::Pcmu, AudioCodec::G722).unwrap();
        assert!(pipeline.process_frames(&[0x55; 80]).unwrap().is_empty());
        let frames = pipeline.process_frames(&[0x55; 80]).unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].len(), 160);
        assert_eq!(pipeline.process_frames(&[0x55; 480]).unwrap().len(), 3);
    }

    #[test]
    fn test_empty_input() {
        let mut pipeline = TranscodePipeline::new(AudioCodec::Pcmu, AudioCodec::G722).unwrap();

        // Feed an empty byte slice
        let output = pipeline.process(&[]);
        // Should either return an error or produce output without panicking
        match output {
            Ok(data) => {
                // If it succeeds, it should produce some output (from zero-padded frame)
                let _ = data; // succeeds without panic
            }
            Err(_) => {
                // Returning an error is also acceptable
            }
        }
    }

    #[test]
    fn test_opus_decode_garbage_graceful() {
        // Opus → PCMU pipeline receiving corrupt Opus data.
        // This exercises the error path in media_session.rs:978 where
        // pipeline.process() can fail and the packet should be dropped.
        let mut pipeline = TranscodePipeline::new(AudioCodec::Opus, AudioCodec::Pcmu).unwrap();

        let garbage = vec![0xFF; 100];
        let result = pipeline.process(&garbage);
        // Should return an error (Opus decoder rejects garbage), not panic
        assert!(
            result.is_err(),
            "garbage Opus data should cause decode error"
        );
    }

    #[test]
    fn test_opus_to_g722() {
        // Opus (48kHz) → G.722 (16kHz): multi-step transcode with 3:1 downsampling
        let mut pipeline = TranscodePipeline::new(AudioCodec::Opus, AudioCodec::G722).unwrap();
        assert!(!pipeline.is_passthrough());
        assert!(
            pipeline.resampler.is_some(),
            "48kHz→16kHz should need a resampler"
        );

        // Encode a sine wave as Opus first
        let mut opus_enc = codec::OpusEncoder::new().unwrap();
        let pcm: Vec<i16> = (0..960)
            .map(|i| ((i as f64 * 0.01).sin() * 5000.0) as i16)
            .collect();
        let mut opus_data = Vec::new();
        opus_enc.encode(&pcm, &mut opus_data).unwrap();

        // Transcode Opus → G.722
        let g722_output = pipeline.process(&opus_data).unwrap();
        assert!(
            !g722_output.is_empty(),
            "expected non-empty G.722 output from Opus input"
        );

        // Verify the G.722 output is decodable
        let mut g722_dec = codec::G722Decoder::new();
        let mut decoded_pcm = Vec::new();
        g722_dec.decode(g722_output, &mut decoded_pcm).unwrap();
        assert!(
            !decoded_pcm.is_empty(),
            "G.722 output should be decodable to PCM"
        );
        // 960 Opus samples at 48kHz = 20ms → 320 G.722 samples at 16kHz = 20ms
        assert!(
            decoded_pcm.len() >= 310 && decoded_pcm.len() <= 330,
            "expected ~320 decoded G.722 samples, got {}",
            decoded_pcm.len()
        );
    }

    #[test]
    fn test_g722_to_pcmu() {
        // G.722 (16kHz) → PCMU (8kHz): 2:1 downsampling
        let mut pipeline = TranscodePipeline::new(AudioCodec::G722, AudioCodec::Pcmu).unwrap();
        assert!(!pipeline.is_passthrough());

        let mut g722_enc = codec::G722Encoder::new();
        let pcm: Vec<i16> = (0..320)
            .map(|i| ((i as f64 * 0.05).sin() * 5000.0) as i16)
            .collect();
        let mut g722_data = Vec::new();
        g722_enc.encode(&pcm, &mut g722_data).unwrap();

        let output = pipeline.process(&g722_data).unwrap();
        assert!(
            !output.is_empty(),
            "expected non-empty PCMU output from G.722 input"
        );
        // G.722 20ms = 160 encoded bytes → decode → 320 PCM samples at 16kHz
        // → downsample to 160 PCM at 8kHz → 160 PCMU bytes
        assert!(
            output.len() >= 155 && output.len() <= 165,
            "expected ~160 PCMU bytes, got {}",
            output.len()
        );
    }

    #[test]
    fn test_all_codec_pairs_create_successfully() {
        // Every valid source→destination combination should create a pipeline
        let codecs = [AudioCodec::Pcmu, AudioCodec::G722, AudioCodec::Opus];
        for &src in &codecs {
            for &dst in &codecs {
                let result = TranscodePipeline::new(src, dst);
                assert!(
                    result.is_ok(),
                    "pipeline {:?} → {:?} should succeed: {:?}",
                    src,
                    dst,
                    result.err()
                );
            }
        }
    }
}
