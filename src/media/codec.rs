use anyhow::Result;

/// Supported audio codecs
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AudioCodec {
    Pcmu, // G.711 mu-law, 8kHz
    G722, // G.722, 16kHz (SDP clock rate is 8kHz but actual audio is 16kHz)
    Opus, // Opus, 48kHz
    L16,  // Raw PCM, 48kHz (internal only, used for bridge endpoints)
}

impl AudioCodec {
    /// Actual audio sample rate
    pub fn sample_rate(&self) -> u32 {
        match self {
            AudioCodec::Pcmu => 8000,
            AudioCodec::G722 => 16000,
            AudioCodec::Opus | AudioCodec::L16 => 48000,
        }
    }

    /// RTP clock rate (may differ from sample rate for G.722)
    pub fn rtp_clock_rate(&self) -> u32 {
        match self {
            AudioCodec::Pcmu => 8000,
            AudioCodec::G722 => 8000, // SDP says 8000 even though audio is 16kHz
            AudioCodec::Opus | AudioCodec::L16 => 48000,
        }
    }

    /// Number of PCM samples per 20ms ptime
    pub fn ptime_samples(&self) -> usize {
        (self.sample_rate() / 50) as usize // 20ms = 1/50 second
    }

    /// Try to identify codec from RTP payload type.
    /// Only reliable for static PTs (0=PCMU, 9=G.722). Dynamic PTs like Opus
    /// should be resolved via `from_name` using SDP negotiation results.
    #[cfg(test)]
    pub fn from_pt(pt: u8) -> Option<Self> {
        match pt {
            0 => Some(AudioCodec::Pcmu),
            9 => Some(AudioCodec::G722),
            111 => Some(AudioCodec::Opus), // common dynamic PT
            _ => None,
        }
    }

    /// Try to identify codec from name string
    pub fn from_name(name: &str) -> Option<Self> {
        match name.to_lowercase().as_str() {
            "pcmu" | "g711" | "g.711" => Some(AudioCodec::Pcmu),
            "g722" | "g.722" => Some(AudioCodec::G722),
            "opus" => Some(AudioCodec::Opus),
            _ => None,
        }
    }
}

/// Trait for decoding encoded audio to PCM i16
pub trait AudioDecoder: Send {
    fn decode(&mut self, encoded: &[u8], pcm_out: &mut Vec<i16>) -> Result<()>;
    #[allow(dead_code)] // called through Box<dyn AudioDecoder> — invisible to compiler
    fn codec(&self) -> AudioCodec;
}

/// Trait for encoding PCM i16 to codec bytes
pub trait AudioEncoder: Send {
    fn encode(&mut self, pcm_in: &[i16], encoded_out: &mut Vec<u8>) -> Result<()>;
    #[allow(dead_code)] // called through Box<dyn AudioEncoder> — invisible to compiler
    fn codec(&self) -> AudioCodec;
}

// ── PCMU (G.711 mu-law) ────────────────────────────────────────────────

pub struct PcmuDecoder {
    decoder: xlaw::PcmXLawDecoder,
}

impl Default for PcmuDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl PcmuDecoder {
    pub fn new() -> Self {
        Self {
            decoder: xlaw::PcmXLawDecoder::new_ulaw(),
        }
    }
}

impl AudioDecoder for PcmuDecoder {
    fn decode(&mut self, encoded: &[u8], pcm_out: &mut Vec<i16>) -> Result<()> {
        pcm_out.clear();
        pcm_out.reserve(encoded.len());
        for &byte in encoded {
            pcm_out.push(self.decoder.decode(byte));
        }
        Ok(())
    }

    fn codec(&self) -> AudioCodec {
        AudioCodec::Pcmu
    }
}

pub struct PcmuEncoder {
    encoder: xlaw::PcmXLawEncoder,
}

impl Default for PcmuEncoder {
    fn default() -> Self {
        Self::new()
    }
}

impl PcmuEncoder {
    pub fn new() -> Self {
        Self {
            encoder: xlaw::PcmXLawEncoder::new_ulaw(),
        }
    }
}

impl AudioEncoder for PcmuEncoder {
    fn encode(&mut self, pcm_in: &[i16], encoded_out: &mut Vec<u8>) -> Result<()> {
        encoded_out.clear();
        encoded_out.reserve(pcm_in.len());
        for &sample in pcm_in {
            encoded_out.push(self.encoder.encode(sample));
        }
        Ok(())
    }

    fn codec(&self) -> AudioCodec {
        AudioCodec::Pcmu
    }
}

// ── G.722 ───────────────────────────────────────────────────────────────

pub struct G722Decoder {
    decoder: ezk_g722::libg722::decoder::Decoder,
}

impl Default for G722Decoder {
    fn default() -> Self {
        Self::new()
    }
}

impl G722Decoder {
    pub fn new() -> Self {
        Self {
            decoder: ezk_g722::libg722::decoder::Decoder::new(
                ezk_g722::libg722::Bitrate::Mode1_64000,
                false, // packed
                false, // eight_k (false = 16kHz output)
            ),
        }
    }
}

impl AudioDecoder for G722Decoder {
    fn decode(&mut self, encoded: &[u8], pcm_out: &mut Vec<i16>) -> Result<()> {
        let samples = self.decoder.decode(encoded);
        pcm_out.clear();
        pcm_out.extend_from_slice(&samples);
        Ok(())
    }

    fn codec(&self) -> AudioCodec {
        AudioCodec::G722
    }
}

pub struct G722Encoder {
    encoder: ezk_g722::libg722::encoder::Encoder,
}

impl Default for G722Encoder {
    fn default() -> Self {
        Self::new()
    }
}

impl G722Encoder {
    pub fn new() -> Self {
        Self {
            encoder: ezk_g722::libg722::encoder::Encoder::new(
                ezk_g722::libg722::Bitrate::Mode1_64000,
                false, // eight_k (false = 16kHz input)
                false, // packed
            ),
        }
    }
}

impl AudioEncoder for G722Encoder {
    fn encode(&mut self, pcm_in: &[i16], encoded_out: &mut Vec<u8>) -> Result<()> {
        let encoded = self.encoder.encode(pcm_in);
        encoded_out.clear();
        encoded_out.extend_from_slice(&encoded);
        Ok(())
    }

    fn codec(&self) -> AudioCodec {
        AudioCodec::G722
    }
}

// ── Opus ────────────────────────────────────────────────────────────────

pub struct OpusDecoder {
    decoder: opus::Decoder,
}

impl OpusDecoder {
    pub fn new() -> Result<Self> {
        let decoder = opus::Decoder::new(48000, opus::Channels::Mono)?;
        Ok(Self { decoder })
    }
}

impl AudioDecoder for OpusDecoder {
    fn decode(&mut self, encoded: &[u8], pcm_out: &mut Vec<i16>) -> Result<()> {
        // Max frame: 120ms at 48kHz = 5760 samples
        pcm_out.resize(5760, 0);
        let decoded = self.decoder.decode(encoded, pcm_out, false)?;
        pcm_out.truncate(decoded);
        Ok(())
    }

    fn codec(&self) -> AudioCodec {
        AudioCodec::Opus
    }
}

pub struct OpusEncoder {
    encoder: opus::Encoder,
}

impl OpusEncoder {
    pub fn new() -> Result<Self> {
        let mut encoder = opus::Encoder::new(48000, opus::Channels::Mono, opus::Application::Voip)?;
        encoder.set_bitrate(opus::Bitrate::Bits(24000))?;
        Ok(Self { encoder })
    }
}

impl AudioEncoder for OpusEncoder {
    fn encode(&mut self, pcm_in: &[i16], encoded_out: &mut Vec<u8>) -> Result<()> {
        // Max encoded size
        encoded_out.resize(4000, 0);
        let encoded = self.encoder.encode(pcm_in, encoded_out)?;
        encoded_out.truncate(encoded);
        Ok(())
    }

    fn codec(&self) -> AudioCodec {
        AudioCodec::Opus
    }
}

// ── L16 (raw PCM, 48kHz) — internal bridge codec ───────────────────────

/// L16 "decoder": payload is raw LE i16 bytes, just reinterpret as samples
pub struct L16Decoder;

impl AudioDecoder for L16Decoder {
    fn decode(&mut self, encoded: &[u8], pcm_out: &mut Vec<i16>) -> Result<()> {
        pcm_out.clear();
        pcm_out.reserve(encoded.len() / 2);
        for chunk in encoded.chunks_exact(2) {
            pcm_out.push(i16::from_le_bytes([chunk[0], chunk[1]]));
        }
        Ok(())
    }

    fn codec(&self) -> AudioCodec {
        AudioCodec::L16
    }
}

/// L16 "encoder": just serialize i16 samples as LE bytes
pub struct L16Encoder;

impl AudioEncoder for L16Encoder {
    fn encode(&mut self, pcm_in: &[i16], encoded_out: &mut Vec<u8>) -> Result<()> {
        encoded_out.clear();
        encoded_out.reserve(pcm_in.len() * 2);
        for &sample in pcm_in {
            encoded_out.extend_from_slice(&sample.to_le_bytes());
        }
        Ok(())
    }

    fn codec(&self) -> AudioCodec {
        AudioCodec::L16
    }
}

// ── Factory functions ───────────────────────────────────────────────────

pub fn make_decoder(codec: AudioCodec) -> Result<Box<dyn AudioDecoder>> {
    match codec {
        AudioCodec::Pcmu => Ok(Box::new(PcmuDecoder::new())),
        AudioCodec::G722 => Ok(Box::new(G722Decoder::new())),
        AudioCodec::Opus => Ok(Box::new(OpusDecoder::new()?)),
        AudioCodec::L16 => Ok(Box::new(L16Decoder)),
    }
}

pub fn make_encoder(codec: AudioCodec) -> Result<Box<dyn AudioEncoder>> {
    match codec {
        AudioCodec::Pcmu => Ok(Box::new(PcmuEncoder::new())),
        AudioCodec::G722 => Ok(Box::new(G722Encoder::new())),
        AudioCodec::Opus => Ok(Box::new(OpusEncoder::new()?)),
        AudioCodec::L16 => Ok(Box::new(L16Encoder)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pcmu_roundtrip() {
        let mut enc = PcmuEncoder::new();
        let mut dec = PcmuDecoder::new();

        let input: Vec<i16> = (0..160)
            .map(|i| ((i as f64 * 0.1).sin() * 10000.0) as i16)
            .collect();
        let mut encoded = Vec::new();
        let mut decoded = Vec::new();

        enc.encode(&input, &mut encoded).unwrap();
        assert_eq!(encoded.len(), 160); // 1 byte per sample

        dec.decode(&encoded, &mut decoded).unwrap();
        assert_eq!(decoded.len(), 160);

        // PCMU is lossy but close
        for (orig, dec) in input.iter().zip(decoded.iter()) {
            assert!(
                (*orig as i32 - *dec as i32).unsigned_abs() < 500,
                "PCMU roundtrip error too large: {} vs {}",
                orig,
                dec
            );
        }
    }

    #[test]
    fn test_g722_roundtrip() {
        let mut enc = G722Encoder::new();
        let mut dec = G722Decoder::new();

        // G.722 operates at 16kHz, 20ms = 320 samples
        let input: Vec<i16> = (0..320)
            .map(|i| ((i as f64 * 0.05).sin() * 10000.0) as i16)
            .collect();
        let mut encoded = Vec::new();
        let mut decoded = Vec::new();

        enc.encode(&input, &mut encoded).unwrap();
        // G.722 at 64kbps: 320 samples → 160 bytes
        assert!(encoded.len() > 0);

        dec.decode(&encoded, &mut decoded).unwrap();
        assert_eq!(decoded.len(), 320);
    }

    #[test]
    fn test_opus_roundtrip() {
        let mut enc = OpusEncoder::new().unwrap();
        let mut dec = OpusDecoder::new().unwrap();

        // Opus at 48kHz, 20ms = 960 samples
        let input: Vec<i16> = (0..960)
            .map(|i| ((i as f64 * 0.01).sin() * 10000.0) as i16)
            .collect();
        let mut encoded = Vec::new();
        let mut decoded = Vec::new();

        enc.encode(&input, &mut encoded).unwrap();
        assert!(encoded.len() > 0 && encoded.len() < 960);

        dec.decode(&encoded, &mut decoded).unwrap();
        assert_eq!(decoded.len(), 960);
    }

    #[test]
    fn test_factory() {
        let _dec = make_decoder(AudioCodec::Pcmu).unwrap();
        let _enc = make_encoder(AudioCodec::Pcmu).unwrap();
        let _dec = make_decoder(AudioCodec::G722).unwrap();
        let _enc = make_encoder(AudioCodec::G722).unwrap();
        let _dec = make_decoder(AudioCodec::Opus).unwrap();
        let _enc = make_encoder(AudioCodec::Opus).unwrap();
    }

    #[test]
    fn test_codec_properties() {
        assert_eq!(AudioCodec::Pcmu.sample_rate(), 8000);
        assert_eq!(AudioCodec::Pcmu.ptime_samples(), 160);
        assert_eq!(AudioCodec::G722.sample_rate(), 16000);
        assert_eq!(AudioCodec::G722.rtp_clock_rate(), 8000);
        assert_eq!(AudioCodec::G722.ptime_samples(), 320);
        assert_eq!(AudioCodec::Opus.sample_rate(), 48000);
        assert_eq!(AudioCodec::Opus.ptime_samples(), 960);
    }

    #[test]
    fn test_pcmu_empty_encode() {
        let mut enc = PcmuEncoder::new();
        let mut encoded = Vec::new();
        enc.encode(&[], &mut encoded).unwrap();
        assert!(
            encoded.is_empty(),
            "encoding empty PCM should produce empty output"
        );
    }

    #[test]
    fn test_pcmu_empty_decode() {
        let mut dec = PcmuDecoder::new();
        let mut decoded = Vec::new();
        dec.decode(&[], &mut decoded).unwrap();
        assert!(
            decoded.is_empty(),
            "decoding empty bytes should produce empty output"
        );
    }

    #[test]
    fn test_opus_decode_garbage() {
        let mut dec = OpusDecoder::new().unwrap();
        let garbage = vec![0xFFu8; 100];
        let mut decoded = Vec::new();
        let result = dec.decode(&garbage, &mut decoded);
        assert!(
            result.is_err(),
            "decoding garbage bytes should return an error"
        );
    }

    #[test]
    fn test_codec_sample_rates() {
        assert_eq!(AudioCodec::Pcmu.sample_rate(), 8000);
        assert_eq!(AudioCodec::G722.sample_rate(), 16000);
        assert_eq!(AudioCodec::Opus.sample_rate(), 48000);

        assert_eq!(AudioCodec::Pcmu.rtp_clock_rate(), 8000);
        assert_eq!(
            AudioCodec::G722.rtp_clock_rate(),
            8000,
            "G.722 has 8kHz RTP clock despite 16kHz audio"
        );
        assert_eq!(AudioCodec::Opus.rtp_clock_rate(), 48000);
    }

    #[test]
    fn test_codec_ptime_samples() {
        assert_eq!(AudioCodec::Pcmu.ptime_samples(), 160, "20ms at 8kHz");
        assert_eq!(AudioCodec::G722.ptime_samples(), 320, "20ms at 16kHz");
        assert_eq!(AudioCodec::Opus.ptime_samples(), 960, "20ms at 48kHz");
    }
}
