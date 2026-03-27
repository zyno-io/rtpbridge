use std::io::Write;
use std::net::SocketAddr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use pcap_file::pcap::{PcapHeader, PcapPacket, PcapWriter};

/// Builds a synthetic Ethernet + IPv4 + UDP frame wrapping an RTP/RTCP payload.
/// This allows PCAP files to be opened in Wireshark with proper protocol dissection.
pub fn build_pcap_frame(src_addr: SocketAddr, dst_addr: SocketAddr, payload: &[u8]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(14 + 20 + 8 + payload.len());

    // Ethernet header (14 bytes) — dummy MACs, EtherType = IPv4
    buf.extend_from_slice(&[0x00; 6]); // dst MAC
    buf.extend_from_slice(&[0x00; 6]); // src MAC
    buf.extend_from_slice(&[0x08, 0x00]); // EtherType IPv4

    // IPv4 header (20 bytes, no options)
    let total_len = (20 + 8 + payload.len()).min(u16::MAX as usize) as u16;
    let src_ip = match src_addr.ip() {
        std::net::IpAddr::V4(ip) => ip.octets(),
        std::net::IpAddr::V6(ip) => {
            // XOR-fold all 16 bytes into 3 synthetic octets for better
            // distribution across distinct IPv6 addresses in PCAP.
            let b = ip.octets();
            let o1 = b[0] ^ b[1] ^ b[2] ^ b[3] ^ b[4] ^ b[5];
            let o2 = b[6] ^ b[7] ^ b[8] ^ b[9] ^ b[10];
            let o3 = b[11] ^ b[12] ^ b[13] ^ b[14] ^ b[15];
            [10, o1, o2, o3]
        }
    };
    let dst_ip = match dst_addr.ip() {
        std::net::IpAddr::V4(ip) => ip.octets(),
        std::net::IpAddr::V6(ip) => {
            let b = ip.octets();
            let o1 = b[0] ^ b[1] ^ b[2] ^ b[3] ^ b[4] ^ b[5];
            let o2 = b[6] ^ b[7] ^ b[8] ^ b[9] ^ b[10];
            let o3 = b[11] ^ b[12] ^ b[13] ^ b[14] ^ b[15];
            [10, o1, o2, o3]
        }
    };

    let ip_header_start = buf.len();
    buf.push(0x45); // version=4, IHL=5
    buf.push(0x00); // DSCP/ECN
    buf.extend_from_slice(&total_len.to_be_bytes());
    buf.extend_from_slice(&[0x00, 0x00]); // identification
    buf.extend_from_slice(&[0x40, 0x00]); // flags=DF, fragment offset=0
    buf.push(64); // TTL
    buf.push(17); // protocol=UDP
    buf.extend_from_slice(&[0x00, 0x00]); // checksum placeholder
    buf.extend_from_slice(&src_ip);
    buf.extend_from_slice(&dst_ip);

    // Compute IPv4 header checksum
    let checksum = ipv4_checksum(&buf[ip_header_start..ip_header_start + 20]);
    buf[ip_header_start + 10] = (checksum >> 8) as u8;
    buf[ip_header_start + 11] = (checksum & 0xFF) as u8;

    // UDP header (8 bytes)
    let udp_len = (8 + payload.len()).min(u16::MAX as usize) as u16;
    buf.extend_from_slice(&src_addr.port().to_be_bytes());
    buf.extend_from_slice(&dst_addr.port().to_be_bytes());
    buf.extend_from_slice(&udp_len.to_be_bytes());
    buf.extend_from_slice(&[0x00, 0x00]); // checksum (0 = not computed)

    // Payload (RTP or RTCP)
    buf.extend_from_slice(payload);

    buf
}

/// Compute IPv4 header checksum (RFC 1071)
fn ipv4_checksum(header: &[u8]) -> u16 {
    let mut sum: u32 = 0;
    for i in (0..header.len()).step_by(2) {
        let word = if i + 1 < header.len() {
            ((header[i] as u32) << 8) | header[i + 1] as u32
        } else {
            (header[i] as u32) << 8
        };
        sum += word;
    }
    while sum > 0xFFFF {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }
    !sum as u16
}

/// A packet to be recorded, with metadata
pub struct RecordPacket {
    pub src_addr: SocketAddr,
    pub dst_addr: SocketAddr,
    pub payload: Vec<u8>,
    pub timestamp: SystemTime,
}

/// Create a new PCAP file writer for Ethernet link type
pub fn create_pcap_writer<W: Write>(writer: W) -> Result<PcapWriter<W>, pcap_file::PcapError> {
    let header = PcapHeader {
        datalink: pcap_file::DataLink::ETHERNET,
        ..PcapHeader::default()
    };
    PcapWriter::with_header(writer, header)
}

/// Write a record packet to a PCAP writer
pub fn write_record_packet<W: Write>(
    writer: &mut PcapWriter<W>,
    packet: &RecordPacket,
) -> Result<(), pcap_file::PcapError> {
    let frame = build_pcap_frame(packet.src_addr, packet.dst_addr, &packet.payload);
    let ts = packet
        .timestamp
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO);

    let pcap_pkt = PcapPacket::new(ts, frame.len() as u32, &frame);
    writer.write_packet(&pcap_pkt)?;
    Ok(())
}

/// Deterministic synthetic address for an endpoint, for PCAP identification.
/// Uses 10.{(index/254)+1}.0.{(index%254)+1}:10000 to avoid collisions.
/// Index 0xFFFF is reserved for the "bridge" side of outbound packets.
pub fn synthetic_addr(endpoint_index: u16) -> SocketAddr {
    if endpoint_index == 0xFFFF {
        // Bridge/outbound marker — use a distinct subnet
        let ip = std::net::Ipv4Addr::new(10, 255, 0, 1);
        SocketAddr::new(std::net::IpAddr::V4(ip), 10000)
    } else {
        // Map index to octets 1..=254 (skip 0 and 255), bump subnet every 254 endpoints
        let octet = (endpoint_index % 254 + 1) as u8; // 1..=254
        let subnet = (endpoint_index / 254 + 1).min(254) as u8;
        let ip = std::net::Ipv4Addr::new(10, subnet, 0, octet);
        SocketAddr::new(std::net::IpAddr::V4(ip), 10000)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pcap_file::pcap::PcapReader;
    use std::io::Cursor;

    #[test]
    fn test_build_pcap_frame() {
        let src: SocketAddr = "10.0.0.1:5000".parse().unwrap();
        let dst: SocketAddr = "10.0.0.2:6000".parse().unwrap();
        let payload = vec![0x80, 0x00, 0x00, 0x01]; // minimal RTP-like

        let frame = build_pcap_frame(src, dst, &payload);

        // Total: 14 (eth) + 20 (ip) + 8 (udp) + 4 (payload) = 46
        assert_eq!(frame.len(), 46);

        // Ethernet header
        assert_eq!(&frame[12..14], &[0x08, 0x00], "EtherType should be IPv4");

        // IPv4 header at offset 14
        assert_eq!(frame[14], 0x45, "IPv4 version=4, IHL=5");
        let ip_total_len = u16::from_be_bytes([frame[16], frame[17]]);
        assert_eq!(ip_total_len, 20 + 8 + 4, "IPv4 total length");
        assert_eq!(frame[23], 17, "protocol should be UDP");
        assert_eq!(&frame[26..30], &[10, 0, 0, 1], "src IP");
        assert_eq!(&frame[30..34], &[10, 0, 0, 2], "dst IP");

        // Verify IPv4 checksum
        let stored_checksum = u16::from_be_bytes([frame[24], frame[25]]);
        // Zero out checksum field, recompute, compare
        let mut header = frame[14..34].to_vec();
        header[10] = 0;
        header[11] = 0;
        let computed = ipv4_checksum(&header);
        assert_eq!(stored_checksum, computed, "IPv4 checksum should be valid");

        // UDP header at offset 34
        let src_port = u16::from_be_bytes([frame[34], frame[35]]);
        let dst_port = u16::from_be_bytes([frame[36], frame[37]]);
        let udp_len = u16::from_be_bytes([frame[38], frame[39]]);
        assert_eq!(src_port, 5000, "UDP src port");
        assert_eq!(dst_port, 6000, "UDP dst port");
        assert_eq!(udp_len, 8 + 4, "UDP length");

        // Payload at offset 42
        assert_eq!(&frame[42..], &payload[..], "payload should match input");
    }

    #[test]
    fn test_create_and_write_pcap() {
        let mut buf = Vec::new();
        {
            let mut writer = create_pcap_writer(&mut buf).unwrap();

            let pkt = RecordPacket {
                src_addr: "10.0.0.1:5000".parse().unwrap(),
                dst_addr: "10.0.0.2:6000".parse().unwrap(),
                payload: vec![
                    0x80, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                ],
                timestamp: SystemTime::now(),
            };

            write_record_packet(&mut writer, &pkt).unwrap();
        }

        // Parse back with PcapReader to verify structure
        let mut reader = PcapReader::new(Cursor::new(&buf)).unwrap();
        let header = reader.header();
        assert_eq!(
            header.datalink,
            pcap_file::DataLink::ETHERNET,
            "link type should be Ethernet"
        );

        let pkt = reader.next_packet().unwrap().unwrap().into_owned();
        // Packet data = 14 (eth) + 20 (ip) + 8 (udp) + 12 (payload) = 54
        assert_eq!(pkt.data.len(), 54, "packet frame size");
        assert_eq!(pkt.orig_len, 54, "orig_len should match frame size");
        assert!(
            reader.next_packet().is_none(),
            "should have exactly 1 packet"
        );
    }

    #[test]
    fn test_multiple_pcap_packets() {
        let mut buf = Vec::new();
        {
            let mut writer = create_pcap_writer(&mut buf).unwrap();

            for i in 0u8..5 {
                let pkt = RecordPacket {
                    src_addr: "10.0.0.1:5000".parse().unwrap(),
                    dst_addr: "10.0.0.2:6000".parse().unwrap(),
                    payload: vec![0x80, 0x00, 0x00, i],
                    timestamp: SystemTime::now(),
                };
                write_record_packet(&mut writer, &pkt).unwrap();
            }
        }

        let mut reader = PcapReader::new(Cursor::new(&buf)).unwrap();
        let mut packets = Vec::new();
        while let Some(pkt) = reader.next_packet() {
            packets.push(pkt.unwrap().into_owned());
        }
        assert_eq!(packets.len(), 5, "should have 5 packets");

        // Verify each packet has correct payload (last byte differs)
        for (i, pkt) in packets.iter().enumerate() {
            let payload_offset = 14 + 20 + 8; // eth + ip + udp
            assert_eq!(
                pkt.data[payload_offset + 3],
                i as u8,
                "packet {i} payload mismatch"
            );
        }
    }

    #[test]
    fn test_synthetic_addr() {
        let a = synthetic_addr(0);
        assert_eq!(a.to_string(), "10.1.0.1:10000");
        let b = synthetic_addr(1);
        assert_eq!(a.to_string(), "10.1.0.1:10000");
        assert_eq!(b.to_string(), "10.1.0.2:10000");
        // 0xFFFF is the bridge marker — distinct subnet
        let bridge = synthetic_addr(0xFFFF);
        assert_eq!(bridge.to_string(), "10.255.0.1:10000");
        // Index 254 wraps to next subnet
        let c = synthetic_addr(254);
        assert_eq!(c.to_string(), "10.2.0.1:10000");
        // Index 255 (previously reserved) now works as normal endpoint
        let d = synthetic_addr(255);
        assert_eq!(d.to_string(), "10.2.0.2:10000");
    }

    #[test]
    fn test_pcap_write_and_read_back() {
        let path = "/tmp/rtpbridge-test-pcap-readback.pcap";
        {
            let file = std::fs::File::create(path).unwrap();
            let mut writer = create_pcap_writer(std::io::BufWriter::new(file)).unwrap();

            for i in 0u8..3 {
                let pkt = RecordPacket {
                    src_addr: "10.0.0.1:5000".parse().unwrap(),
                    dst_addr: "10.0.0.2:6000".parse().unwrap(),
                    payload: vec![0x80, 0x00, 0x00, i, 0xDE, 0xAD],
                    timestamp: SystemTime::now(),
                };
                write_record_packet(&mut writer, &pkt).unwrap();
            }
        }

        // Read the file back and verify PCAP magic number
        let data = std::fs::read(path).unwrap();
        assert!(data.len() > 4, "PCAP file should have content");
        let magic = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
        assert!(
            magic == 0xa1b2c3d4 || magic == 0xd4c3b2a1,
            "file should start with a valid PCAP magic number, got {magic:#010x}"
        );

        // Parse with PcapReader and verify packet count
        let file = std::fs::File::open(path).unwrap();
        let mut reader = PcapReader::new(std::io::BufReader::new(file)).unwrap();
        let mut count = 0;
        while let Some(pkt) = reader.next_packet() {
            pkt.unwrap();
            count += 1;
        }
        assert_eq!(count, 3, "should have written 3 packets");

        std::fs::remove_file(path).ok();
    }

    #[test]
    fn test_pcap_empty_packet() {
        let mut buf = Vec::new();
        {
            let mut writer = create_pcap_writer(&mut buf).unwrap();

            let pkt = RecordPacket {
                src_addr: "10.0.0.1:5000".parse().unwrap(),
                dst_addr: "10.0.0.2:6000".parse().unwrap(),
                payload: vec![], // empty payload
                timestamp: SystemTime::now(),
            };
            write_record_packet(&mut writer, &pkt).unwrap();
        }

        // Parse back — should have exactly 1 packet with just headers
        let mut reader = PcapReader::new(Cursor::new(&buf)).unwrap();
        let pkt = reader.next_packet().unwrap().unwrap().into_owned();
        // Frame = 14 (eth) + 20 (ip) + 8 (udp) + 0 (empty payload) = 42
        assert_eq!(pkt.data.len(), 42, "empty payload frame should be 42 bytes");
        assert!(
            reader.next_packet().is_none(),
            "should have exactly 1 packet"
        );
    }

    #[test]
    fn test_ipv6_xor_folding_different_addresses() {
        // Two distinct IPv6 addresses that differ only in the first segment
        // should produce different synthetic IPs after XOR folding
        let src1: SocketAddr = "[2001:db8::1]:5000".parse().unwrap();
        let src2: SocketAddr = "[2001:db9::1]:5000".parse().unwrap();
        let dst: SocketAddr = "10.0.0.2:6000".parse().unwrap();

        let frame1 = build_pcap_frame(src1, dst, &[0x80]);
        let frame2 = build_pcap_frame(src2, dst, &[0x80]);

        // Source IP octets are at bytes 26-29 in the frame (offset 14 for IP header + 12 for src IP)
        assert_ne!(
            &frame1[26..30],
            &frame2[26..30],
            "Different IPv6 addresses should produce different synthetic IPs"
        );
    }

    #[test]
    fn test_udp_length_capped_for_oversized_payload() {
        // Verify that a payload > 65527 bytes doesn't produce a truncated UDP length field.
        // The cap ensures the UDP header length is clamped to u16::MAX rather than wrapping.
        let src: SocketAddr = "10.0.0.1:5000".parse().unwrap();
        let dst: SocketAddr = "10.0.0.2:6000".parse().unwrap();
        // Payload that would make (8 + len) overflow u16
        let payload = vec![0x80; 65535];
        let frame = build_pcap_frame(src, dst, &payload);

        // UDP header at offset 14+20=34. Length field at bytes 38-39.
        let udp_len = u16::from_be_bytes([frame[38], frame[39]]);
        // 8 + 65535 = 65543, which exceeds u16::MAX. Should be capped to 65535.
        assert_eq!(
            udp_len,
            u16::MAX,
            "UDP length should be capped to u16::MAX for oversized payloads"
        );

        // IP total length should also be capped
        let ip_total_len = u16::from_be_bytes([frame[16], frame[17]]);
        assert_eq!(ip_total_len, u16::MAX, "IP total length should be capped");
    }

    #[test]
    fn test_ipv6_address_stays_in_10_range() {
        let src: SocketAddr = "[ff02::1]:5000".parse().unwrap();
        let dst: SocketAddr = "[::1]:6000".parse().unwrap();
        let frame = build_pcap_frame(src, dst, &[0x80]);
        // Source IP at offset 26, Dest IP at offset 30
        assert_eq!(frame[26], 10, "Synthetic src IP first octet must be 10");
        assert_eq!(frame[30], 10, "Synthetic dst IP first octet must be 10");
    }
}
