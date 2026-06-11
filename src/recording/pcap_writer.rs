use std::io::Write;
use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use pcap_file::pcap::{PcapHeader, PcapPacket, PcapWriter};

/// Builds an Ethernet + IP + UDP frame wrapping an RTP/RTCP payload so the PCAP
/// opens in Wireshark with proper protocol dissection. The IP family follows the
/// addresses: a real IPv4 or IPv6 header is emitted (both endpoints of one frame
/// always share a family — the dual-stack guards forbid a v4/v6 mix). A stray
/// mismatch defensively falls back to the IPv4 path with the IPv6 side folded.
pub fn build_pcap_frame(src_addr: SocketAddr, dst_addr: SocketAddr, payload: &[u8]) -> Vec<u8> {
    match (src_addr.ip(), dst_addr.ip()) {
        (IpAddr::V6(src), IpAddr::V6(dst)) => {
            build_ipv6_frame(src, src_addr.port(), dst, dst_addr.port(), payload)
        }
        (src, dst) => build_ipv4_frame(
            ipv4_octets(src),
            src_addr.port(),
            ipv4_octets(dst),
            dst_addr.port(),
            payload,
        ),
    }
}

/// IPv4 octets for an address. A V6 address (only reached on a defensive
/// family-mismatch fallback) is XOR-folded into a synthetic `10.x.x.x`.
fn ipv4_octets(ip: IpAddr) -> [u8; 4] {
    match ip {
        IpAddr::V4(ip) => ip.octets(),
        IpAddr::V6(ip) => {
            let b = ip.octets();
            [
                10,
                b[0] ^ b[1] ^ b[2] ^ b[3] ^ b[4] ^ b[5],
                b[6] ^ b[7] ^ b[8] ^ b[9] ^ b[10],
                b[11] ^ b[12] ^ b[13] ^ b[14] ^ b[15],
            ]
        }
    }
}

/// Ethernet + IPv4 + UDP frame. UDP checksum 0 is legal for IPv4.
fn build_ipv4_frame(
    src_ip: [u8; 4],
    src_port: u16,
    dst_ip: [u8; 4],
    dst_port: u16,
    payload: &[u8],
) -> Vec<u8> {
    let mut buf = Vec::with_capacity(14 + 20 + 8 + payload.len());

    // Ethernet header (14 bytes) — dummy MACs, EtherType = IPv4
    buf.extend_from_slice(&[0x00; 6]); // dst MAC
    buf.extend_from_slice(&[0x00; 6]); // src MAC
    buf.extend_from_slice(&[0x08, 0x00]); // EtherType IPv4

    // IPv4 header (20 bytes, no options)
    let total_len = (20 + 8 + payload.len()).min(u16::MAX as usize) as u16;
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

    // UDP header (8 bytes) — checksum 0 (optional for IPv4)
    let udp_len = (8 + payload.len()).min(u16::MAX as usize) as u16;
    buf.extend_from_slice(&src_port.to_be_bytes());
    buf.extend_from_slice(&dst_port.to_be_bytes());
    buf.extend_from_slice(&udp_len.to_be_bytes());
    buf.extend_from_slice(&[0x00, 0x00]); // checksum (0 = not computed)

    buf.extend_from_slice(payload);
    buf
}

/// Ethernet + IPv6 + UDP frame. IPv6 UDP requires a real checksum (0 is illegal),
/// so it is computed over the IPv6 pseudo-header.
fn build_ipv6_frame(
    src: Ipv6Addr,
    src_port: u16,
    dst: Ipv6Addr,
    dst_port: u16,
    payload: &[u8],
) -> Vec<u8> {
    let mut buf = Vec::with_capacity(14 + 40 + 8 + payload.len());

    // Ethernet header (14 bytes) — dummy MACs, EtherType = IPv6
    buf.extend_from_slice(&[0x00; 6]); // dst MAC
    buf.extend_from_slice(&[0x00; 6]); // src MAC
    buf.extend_from_slice(&[0x86, 0xDD]); // EtherType IPv6

    let udp_len = (8 + payload.len()).min(u16::MAX as usize) as u16;

    // IPv6 header (40 bytes)
    buf.push(0x60); // version=6, traffic class high nibble=0
    buf.extend_from_slice(&[0x00, 0x00, 0x00]); // traffic class low + flow label
    buf.extend_from_slice(&udp_len.to_be_bytes()); // payload length (UDP header + data)
    buf.push(17); // next header = UDP
    buf.push(64); // hop limit
    buf.extend_from_slice(&src.octets());
    buf.extend_from_slice(&dst.octets());

    // UDP header (8 bytes) with a computed checksum (mandatory for IPv6)
    let checksum = udp6_checksum(&src, &dst, src_port, dst_port, payload);
    buf.extend_from_slice(&src_port.to_be_bytes());
    buf.extend_from_slice(&dst_port.to_be_bytes());
    buf.extend_from_slice(&udp_len.to_be_bytes());
    buf.extend_from_slice(&checksum.to_be_bytes());

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

/// UDP checksum over the IPv6 pseudo-header (RFC 8200 §8.1 / RFC 768). The result
/// is never 0 — a computed 0 is transmitted as 0xFFFF.
fn udp6_checksum(
    src: &Ipv6Addr,
    dst: &Ipv6Addr,
    src_port: u16,
    dst_port: u16,
    payload: &[u8],
) -> u16 {
    fn add_bytes(sum: &mut u32, bytes: &[u8]) {
        let mut i = 0;
        while i + 1 < bytes.len() {
            *sum += ((bytes[i] as u32) << 8) | bytes[i + 1] as u32;
            i += 2;
        }
        if i < bytes.len() {
            *sum += (bytes[i] as u32) << 8;
        }
    }

    let udp_len = (8 + payload.len()).min(u16::MAX as usize) as u16;
    let mut sum: u32 = 0;
    // Pseudo-header: src(16) + dst(16) + upper-layer length(32) + next-header(8, zero-padded)
    add_bytes(&mut sum, &src.octets());
    add_bytes(&mut sum, &dst.octets());
    sum += udp_len as u32; // upper-layer packet length (high 16 bits are 0)
    sum += 17; // next header (UDP)
    // UDP header (checksum field counted as 0)
    sum += src_port as u32;
    sum += dst_port as u32;
    sum += udp_len as u32;
    // Payload
    add_bytes(&mut sum, payload);

    while sum > 0xFFFF {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }
    let cs = !sum as u16;
    if cs == 0 { 0xFFFF } else { cs }
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
    fn test_ipv4_frame_uses_real_addresses() {
        let src: SocketAddr = "203.0.113.5:5000".parse().unwrap();
        let dst: SocketAddr = "198.51.100.9:6000".parse().unwrap();
        let frame = build_pcap_frame(src, dst, &[0x80]);
        assert_eq!(&frame[12..14], &[0x08, 0x00], "EtherType should be IPv4");
        // Real addresses appear verbatim — IPv4 src at 26..30, dst at 30..34.
        assert_eq!(&frame[26..30], &[203, 0, 113, 5], "real IPv4 source");
        assert_eq!(&frame[30..34], &[198, 51, 100, 9], "real IPv4 dest");
        assert_eq!(u16::from_be_bytes([frame[34], frame[35]]), 5000);
        assert_eq!(u16::from_be_bytes([frame[36], frame[37]]), 6000);
    }

    #[test]
    fn test_ipv6_frame_has_real_addresses_and_checksum() {
        let src: SocketAddr = "[2001:db8::1]:5000".parse().unwrap();
        let dst: SocketAddr = "[2001:db8::2]:6000".parse().unwrap();
        let payload = [0x80, 0x00, 0x01, 0x02];
        let frame = build_pcap_frame(src, dst, &payload);

        // Ethernet → IPv6, UDP.
        assert_eq!(&frame[12..14], &[0x86, 0xDD], "EtherType should be IPv6");
        assert_eq!(frame[14] & 0xF0, 0x60, "IP version should be 6");
        assert_eq!(frame[20], 17, "next header should be UDP");

        // Real IPv6 addresses are embedded verbatim (not folded to a 10.x synth).
        let IpAddr::V6(src_v6) = src.ip() else {
            unreachable!()
        };
        let IpAddr::V6(dst_v6) = dst.ip() else {
            unreachable!()
        };
        assert_eq!(&frame[22..38], &src_v6.octets(), "real IPv6 source address");
        assert_eq!(&frame[38..54], &dst_v6.octets(), "real IPv6 dest address");
        assert_eq!(u16::from_be_bytes([frame[54], frame[55]]), 5000);
        assert_eq!(u16::from_be_bytes([frame[56], frame[57]]), 6000);

        // IPv6 UDP checksum is mandatory — must not be zero.
        assert_ne!(
            u16::from_be_bytes([frame[60], frame[61]]),
            0,
            "IPv6 UDP checksum must be computed (0 is illegal)"
        );
        // Payload follows the 8-byte UDP header.
        assert_eq!(&frame[62..66], &payload);
    }
}
