/// Parsed RTP header (RFC 3550)
#[derive(Debug, Clone)]
pub struct RtpHeader {
    #[allow(dead_code)] // protocol field kept for spec completeness
    pub version: u8,
    #[allow(dead_code)]
    pub padding: bool,
    #[allow(dead_code)]
    pub extension: bool,
    #[allow(dead_code)]
    pub csrc_count: u8,
    pub marker: bool,
    pub payload_type: u8,
    pub sequence_number: u16,
    pub timestamp: u32,
    pub ssrc: u32,
    /// Total header size in bytes (including CSRC and extensions)
    pub header_len: usize,
}

impl RtpHeader {
    /// Parse an RTP header from a byte slice. Returns None if too short or invalid.
    pub fn parse(data: &[u8]) -> Option<Self> {
        if data.len() < 12 {
            return None;
        }

        let version = (data[0] >> 6) & 0x03;
        if version != 2 {
            return None;
        }

        let padding = (data[0] >> 5) & 0x01 != 0;
        let extension = (data[0] >> 4) & 0x01 != 0;
        let csrc_count = data[0] & 0x0F;
        let marker = (data[1] >> 7) & 0x01 != 0;
        let payload_type = data[1] & 0x7F;
        let sequence_number = u16::from_be_bytes([data[2], data[3]]);
        let timestamp = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
        let ssrc = u32::from_be_bytes([data[8], data[9], data[10], data[11]]);

        let mut header_len = 12 + (csrc_count as usize * 4);

        // Skip header extension if present
        if extension {
            if data.len() < header_len + 4 {
                return None;
            }
            let ext_len = u16::from_be_bytes([data[header_len + 2], data[header_len + 3]]) as usize;
            header_len = ext_len
                .checked_mul(4)
                .and_then(|b| header_len.checked_add(4 + b))?;
        }

        if data.len() < header_len {
            return None;
        }

        Some(Self {
            version,
            padding,
            extension,
            csrc_count,
            marker,
            payload_type,
            sequence_number,
            timestamp,
            ssrc,
            header_len,
        })
    }

    /// Get the payload portion of an RTP packet, stripping any padding bytes.
    pub fn payload<'a>(&self, data: &'a [u8]) -> &'a [u8] {
        let payload = &data[self.header_len..];
        if self.padding && !payload.is_empty() {
            // RFC 3550 §5.1: last byte indicates number of padding bytes (including itself)
            let pad_len = payload[payload.len() - 1] as usize;
            if pad_len > 0 && pad_len <= payload.len() {
                return &payload[..payload.len() - pad_len];
            }
        }
        payload
    }

    /// Build an RTP packet from header fields and payload
    pub fn build(
        payload_type: u8,
        sequence_number: u16,
        timestamp: u32,
        ssrc: u32,
        marker: bool,
        payload: &[u8],
    ) -> Vec<u8> {
        let mut buf = Vec::with_capacity(12 + payload.len());

        // Byte 0: V=2, P=0, X=0, CC=0
        buf.push(0x80);
        // Byte 1: M + PT
        buf.push(if marker { 0x80 } else { 0x00 } | (payload_type & 0x7F));
        // Bytes 2-3: sequence number
        buf.extend_from_slice(&sequence_number.to_be_bytes());
        // Bytes 4-7: timestamp
        buf.extend_from_slice(&timestamp.to_be_bytes());
        // Bytes 8-11: SSRC
        buf.extend_from_slice(&ssrc.to_be_bytes());
        // Payload
        buf.extend_from_slice(payload);

        buf
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_basic() {
        let pkt = RtpHeader::build(0, 1234, 5678, 0xDEADBEEF, false, b"hello");
        let hdr = RtpHeader::parse(&pkt).unwrap();
        assert_eq!(hdr.version, 2);
        assert_eq!(hdr.payload_type, 0);
        assert_eq!(hdr.sequence_number, 1234);
        assert_eq!(hdr.timestamp, 5678);
        assert_eq!(hdr.ssrc, 0xDEADBEEF);
        assert!(!hdr.marker);
        assert_eq!(hdr.header_len, 12);
        assert_eq!(hdr.payload(&pkt), b"hello");
    }

    #[test]
    fn test_marker_bit() {
        let pkt = RtpHeader::build(111, 0, 0, 0, true, &[]);
        let hdr = RtpHeader::parse(&pkt).unwrap();
        assert!(hdr.marker);
        assert_eq!(hdr.payload_type, 111);
    }

    #[test]
    fn test_too_short() {
        assert!(RtpHeader::parse(&[0x80; 11]).is_none());
    }

    #[test]
    fn test_wrong_version() {
        let mut pkt = RtpHeader::build(0, 0, 0, 0, false, &[]);
        pkt[0] = 0x00; // version 0
        assert!(RtpHeader::parse(&pkt).is_none());
    }

    #[test]
    fn test_roundtrip() {
        let payload = vec![1, 2, 3, 4, 5];
        let pkt = RtpHeader::build(96, 65535, 0xFFFFFFFF, 12345, true, &payload);
        let hdr = RtpHeader::parse(&pkt).unwrap();
        assert_eq!(hdr.payload_type, 96);
        assert_eq!(hdr.sequence_number, 65535);
        assert_eq!(hdr.timestamp, 0xFFFFFFFF);
        assert_eq!(hdr.ssrc, 12345);
        assert!(hdr.marker);
        assert_eq!(hdr.payload(&pkt), &payload);
    }

    #[test]
    fn test_csrc_count_max() {
        // Build packet with CC=15 (max CSRC count) — needs 12 + 15*4 = 72 bytes header
        let mut pkt = vec![0u8; 72 + 5]; // header + 5 bytes payload
        pkt[0] = 0x80 | 15; // V=2, CC=15
        pkt[1] = 0; // PT=0
        // Fill CSRC and payload with recognizable data
        for i in 72..77 {
            pkt[i] = 0xAA;
        }

        let hdr = RtpHeader::parse(&pkt).unwrap();
        assert_eq!(hdr.csrc_count, 15);
        assert_eq!(hdr.header_len, 72);
        assert_eq!(hdr.payload(&pkt), &[0xAA; 5]);

        // Too short for claimed CC=15 — should return None
        let short_pkt = vec![0x80 | 15, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        assert!(RtpHeader::parse(&short_pkt).is_none());
    }

    #[test]
    fn test_extension_header() {
        // Build a packet with X bit set and a 1-word extension
        let mut pkt = Vec::new();
        pkt.push(0x90); // V=2, X=1, CC=0
        pkt.push(0); // PT=0
        pkt.extend_from_slice(&0u16.to_be_bytes()); // seq
        pkt.extend_from_slice(&0u32.to_be_bytes()); // ts
        pkt.extend_from_slice(&0u32.to_be_bytes()); // ssrc
        // Extension header: 2-byte profile + 2-byte length (in 32-bit words)
        pkt.extend_from_slice(&0xBEDEu16.to_be_bytes()); // one-byte header profile
        pkt.extend_from_slice(&2u16.to_be_bytes()); // 2 words of extension data
        pkt.extend_from_slice(&[0; 8]); // 2 words = 8 bytes
        pkt.extend_from_slice(b"hi"); // payload

        let hdr = RtpHeader::parse(&pkt).unwrap();
        assert!(hdr.extension);
        // header_len = 12 (fixed) + 4 (ext header) + 8 (ext data) = 24
        assert_eq!(hdr.header_len, 24);
        assert_eq!(hdr.payload(&pkt), b"hi");
    }

    #[test]
    fn test_extension_truncated_header() {
        // Extension bit set, but packet only has 13 bytes (12 fixed + 1).
        // Not enough room for the 4-byte extension header — should return None.
        let mut pkt = vec![0u8; 13];
        pkt[0] = 0x90; // V=2, X=1, CC=0
        assert!(RtpHeader::parse(&pkt).is_none());
    }

    #[test]
    fn test_extension_truncated_data() {
        // Extension bit set, 4-byte extension header present claiming 2 words
        // (8 bytes) of extension data, but actual data after the extension header
        // is shorter than 8 bytes — should return None.
        let mut pkt = Vec::new();
        pkt.push(0x90); // V=2, X=1, CC=0
        pkt.push(0); // PT=0
        pkt.extend_from_slice(&0u16.to_be_bytes()); // seq
        pkt.extend_from_slice(&0u32.to_be_bytes()); // ts
        pkt.extend_from_slice(&0u32.to_be_bytes()); // ssrc
        // Extension header
        pkt.extend_from_slice(&0xBEDEu16.to_be_bytes()); // profile
        pkt.extend_from_slice(&2u16.to_be_bytes()); // claims 2 words = 8 bytes
        pkt.extend_from_slice(&[0; 4]); // only 4 bytes instead of 8
        assert!(RtpHeader::parse(&pkt).is_none());
    }

    #[test]
    fn test_extension_oversized_length() {
        // Extension header claims far more words than available data — should return None.
        let mut pkt = Vec::new();
        pkt.push(0x90); // V=2, X=1, CC=0
        pkt.push(0); // PT=0
        pkt.extend_from_slice(&0u16.to_be_bytes()); // seq
        pkt.extend_from_slice(&0u32.to_be_bytes()); // ts
        pkt.extend_from_slice(&0u32.to_be_bytes()); // ssrc
        // Extension header
        pkt.extend_from_slice(&0xBEDEu16.to_be_bytes()); // profile
        pkt.extend_from_slice(&100u16.to_be_bytes()); // claims 100 words = 400 bytes
        pkt.extend_from_slice(&[0; 8]); // only 8 bytes of data
        assert!(RtpHeader::parse(&pkt).is_none());
    }

    #[test]
    fn test_large_rtp_packet() {
        // RTP packet with payload larger than typical MTU (> 1500 bytes)
        let large_payload = vec![0xBB; 4000];
        let pkt = RtpHeader::build(111, 42, 96000, 0xFEEDFACE, true, &large_payload);
        let hdr = RtpHeader::parse(&pkt).unwrap();
        assert_eq!(hdr.payload_type, 111);
        assert_eq!(hdr.sequence_number, 42);
        assert_eq!(hdr.ssrc, 0xFEEDFACE);
        assert!(hdr.marker);
        assert_eq!(hdr.payload(&pkt).len(), 4000);
        assert_eq!(hdr.payload(&pkt), &large_payload[..]);
    }

    #[test]
    fn test_rtp_with_csrc_and_extension() {
        // Combined: CC=2 + extension header
        let mut pkt = Vec::new();
        pkt.push(0x80 | 0x10 | 2); // V=2, X=1, CC=2
        pkt.push(96); // PT=96, M=0
        pkt.extend_from_slice(&1u16.to_be_bytes()); // seq
        pkt.extend_from_slice(&320u32.to_be_bytes()); // ts
        pkt.extend_from_slice(&0xAAAAAAAAu32.to_be_bytes()); // ssrc
        pkt.extend_from_slice(&0x11111111u32.to_be_bytes()); // csrc 1
        pkt.extend_from_slice(&0x22222222u32.to_be_bytes()); // csrc 2
        // Extension header
        pkt.extend_from_slice(&0xBEDEu16.to_be_bytes()); // profile
        pkt.extend_from_slice(&1u16.to_be_bytes()); // 1 word = 4 bytes
        pkt.extend_from_slice(&[0xEE; 4]); // extension data
        pkt.extend_from_slice(&[0xDD; 80]); // payload

        let hdr = RtpHeader::parse(&pkt).unwrap();
        assert_eq!(hdr.csrc_count, 2);
        assert!(hdr.extension);
        assert_eq!(hdr.payload_type, 96);
        // header = 12 + 2*4(csrc) + 4(ext hdr) + 4(ext data) = 28
        assert_eq!(hdr.header_len, 28);
        assert_eq!(hdr.payload(&pkt).len(), 80);
    }
}
