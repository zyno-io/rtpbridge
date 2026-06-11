//! Codec-tagged PCAP metadata: per-endpoint "stream descriptor" packets and the
//! classifier the decoder uses to tell descriptors, RTCP and RTP apart.
//!
//! A descriptor is written into the PCAP as a synthetic UDP packet whose payload
//! is `MAGIC` followed by JSON. It declares the codec/clock/addressing of the RTP
//! stream that follows for one endpoint, so a consumer (e.g. `pcap2audio`) knows
//! how to decode the media. See `docs/plans/recording-codec-tagged-pcap.md`.

use serde::{Deserialize, Serialize};

/// Magic prefix for descriptor payloads. `'R'` (0x52) has RTP version bits `01`,
/// so a descriptor can never be confused with a real RTP packet (V=2) or RTCP
/// (PT 200–204), and Wireshark's RTP heuristic skips it.
pub const MAGIC: [u8; 4] = *b"RBP1";

/// Current descriptor format version (the `v` field).
pub const VERSION: u8 = 1;

/// Per-endpoint stream descriptor embedded in the PCAP ahead of that endpoint's
/// RTP. Field order/names are wire-visible (JSON); keep them stable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamDescriptor {
    /// Format version (`VERSION`).
    pub v: u8,
    /// Endpoint UUID (string form).
    pub endpoint_id: String,
    /// `"remote"` (real peer source) or `"internal"` (synthesized source).
    pub role: String,
    /// Endpoint type label: `rtp`/`webrtc`/`file`/`tone`/`bridge`/`websocket`.
    #[serde(rename = "type")]
    pub ep_type: String,
    /// Codec name: `PCMU`/`G722`/`opus`/`L16`.
    pub codec: String,
    /// Payload type the RTP that follows uses.
    pub pt: u8,
    /// RTP clock rate (for G.722 this is 8000 even though audio is 16 kHz).
    pub clock_rate: u32,
    /// Channel count (mono = 1).
    pub channels: u8,
    /// Byte order, only meaningful for `L16` (`"le"` in this codebase). Omitted
    /// otherwise.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub endian: Option<String>,
    /// RTP SSRC, when known. A secondary consistency hint — NOT the channel key.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub ssrc: Option<u32>,
    /// Source socket addr of the frame (cosmetic; the frame itself is the key).
    pub local: String,
    /// Remote socket addr / internal marker (cosmetic).
    pub remote: String,
}

impl StreamDescriptor {
    /// Encode as `MAGIC || JSON`.
    pub fn encode(&self) -> Vec<u8> {
        let json = serde_json::to_vec(self).expect("StreamDescriptor always serializes");
        let mut buf = Vec::with_capacity(MAGIC.len() + json.len());
        buf.extend_from_slice(&MAGIC);
        buf.extend_from_slice(&json);
        buf
    }

    /// Parse a `MAGIC || JSON` payload. Returns `None` if the magic is absent or
    /// the JSON is malformed. (Decoder side — used by the `pcap2audio` binary.)
    #[allow(dead_code)]
    pub fn parse(payload: &[u8]) -> Option<StreamDescriptor> {
        let rest = payload.strip_prefix(&MAGIC[..])?;
        serde_json::from_slice(rest).ok()
    }
}

/// What a recorded UDP payload is. (Decoder side — used by the `pcap2audio` binary.)
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PacketKind {
    /// A `StreamDescriptor` (magic-prefixed).
    Descriptor,
    /// An RTCP packet (V=2, PT 200–204) — the decoder skips these.
    Rtcp,
    /// An RTP packet — decode against the bound channel's codec.
    Rtp,
}

/// Classify a recorded UDP payload. Checks the descriptor magic first, then the
/// RTCP PT range, otherwise assumes RTP. (RTP with PT 72–76 — i.e. RTCP PTs masked
/// to 7 bits — is never produced by this server's audio PTs, so the simple RTCP
/// test is safe here.) (Decoder side — used by the `pcap2audio` binary.)
#[allow(dead_code)]
pub fn classify(payload: &[u8]) -> PacketKind {
    if payload.starts_with(&MAGIC) {
        return PacketKind::Descriptor;
    }
    if payload.len() >= 2 && (payload[0] & 0xC0) == 0x80 && (200..=204).contains(&payload[1]) {
        return PacketKind::Rtcp;
    }
    PacketKind::Rtp
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> StreamDescriptor {
        StreamDescriptor {
            v: VERSION,
            endpoint_id: "0c2f1d3e-0000-4000-8000-000000000001".to_string(),
            role: "remote".to_string(),
            ep_type: "rtp".to_string(),
            codec: "PCMU".to_string(),
            pt: 0,
            clock_rate: 8000,
            channels: 1,
            endian: None,
            ssrc: Some(0x12345678),
            local: "10.0.0.1:4000".to_string(),
            remote: "203.0.113.7:5004".to_string(),
        }
    }

    #[test]
    fn round_trip() {
        let d = sample();
        let bytes = d.encode();
        assert_eq!(&bytes[..4], &MAGIC);
        let parsed = StreamDescriptor::parse(&bytes).expect("parses");
        assert_eq!(parsed, d);
    }

    #[test]
    fn l16_endian_round_trip() {
        let mut d = sample();
        d.codec = "L16".to_string();
        d.endian = Some("le".to_string());
        d.clock_rate = 48000;
        let parsed = StreamDescriptor::parse(&d.encode()).unwrap();
        assert_eq!(parsed.endian.as_deref(), Some("le"));
    }

    #[test]
    fn classify_descriptor() {
        assert_eq!(classify(&sample().encode()), PacketKind::Descriptor);
    }

    #[test]
    fn classify_rtp() {
        // V=2, PT=0 (PCMU), then header bytes.
        let rtp = [0x80u8, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0, 0, 0];
        assert_eq!(classify(&rtp), PacketKind::Rtp);
        // Opus dynamic PT 111.
        let opus = [0x80u8, 111, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0];
        assert_eq!(classify(&opus), PacketKind::Rtp);
    }

    #[test]
    fn classify_rtcp() {
        // V=2, PT=200 (sender report).
        let sr = [0x80u8, 200, 0, 6, 0, 0, 0, 0];
        assert_eq!(classify(&sr), PacketKind::Rtcp);
        // PT=201 (receiver report).
        let rr = [0x81u8, 201, 0, 7, 0, 0, 0, 0];
        assert_eq!(classify(&rr), PacketKind::Rtcp);
    }

    #[test]
    fn parse_rejects_non_magic() {
        assert!(StreamDescriptor::parse(b"XXXX{}").is_none());
        assert!(StreamDescriptor::parse(&[0x80, 0x00]).is_none());
    }
}
