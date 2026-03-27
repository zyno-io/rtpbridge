use anyhow::Result;

use super::codec::{self, AudioCodec, AudioDecoder, AudioEncoder};
use super::resample::Resampler;

/// A transcode pipeline: decode source codec → resample → encode to destination codec.
/// If source and destination codecs are the same, operates in passthrough mode.
pub struct TranscodePipeline {
    decoder: Box<dyn AudioDecoder>,
    encoder: Box<dyn AudioEncoder>,
    resampler: Option<Resampler>,
    passthrough: bool,
    decode_buf: Vec<i16>,
    resample_buf: Vec<i16>,
    encode_buf: Vec<u8>,
}

impl TranscodePipeline {
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
            decoder,
            encoder,
            resampler,
            passthrough,
            decode_buf: Vec::with_capacity(960),
            resample_buf: Vec::with_capacity(960),
            encode_buf: Vec::with_capacity(960),
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
        self.decoder.decode(input, &mut self.decode_buf)?;

        // Resample if needed
        if let Some(resampler) = &mut self.resampler {
            resampler.process(&self.decode_buf, &mut self.resample_buf);
        } else {
            self.resample_buf.clear();
            self.resample_buf.extend_from_slice(&self.decode_buf);
        }

        // All codecs require exact frame sizes. Pad or truncate to target ptime.
        let target_samples = self.encoder.codec().ptime_samples();
        if self.resample_buf.len() != target_samples {
            tracing::trace!(
                expected = target_samples,
                actual = self.resample_buf.len(),
                "transcode frame size mismatch, padding/truncating"
            );
            self.resample_buf.resize(target_samples, 0);
        }

        // Encode PCM → destination codec
        self.encoder
            .encode(&self.resample_buf, &mut self.encode_buf)?;

        Ok(&self.encode_buf)
    }

    #[allow(dead_code)] // used in tests
    pub fn source_codec(&self) -> AudioCodec {
        self.decoder.codec()
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
        let sign_matches: usize = (0..min_len)
            .filter(|&i| {
                original_pcm[i].signum() == roundtrip_pcm[i].signum() || original_pcm[i] == 0
            })
            .count();
        let match_pct = sign_matches as f64 / min_len as f64;
        assert!(
            match_pct > 0.6,
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
    fn test_short_input_padded() {
        let mut pipeline = TranscodePipeline::new(AudioCodec::Pcmu, AudioCodec::G722).unwrap();

        // Feed only 10 bytes of PCMU instead of the expected 160
        let short_input = vec![0x55u8; 10];
        let output = pipeline.process(&short_input).unwrap();
        assert!(
            !output.is_empty(),
            "short input should still produce output after padding"
        );
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
