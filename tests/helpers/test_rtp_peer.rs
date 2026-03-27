#![allow(dead_code)]

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::net::UdpSocket;
use tokio::sync::Mutex;

/// Simple plain-RTP test peer for integration tests.
/// Sends and receives raw RTP packets over UDP.
pub struct TestRtpPeer {
    pub socket: Arc<UdpSocket>,
    pub local_addr: SocketAddr,
    pub remote_addr: Option<SocketAddr>,
    ssrc: u32,
    seq_no: u16,
    timestamp: u32,
    packets_received: Arc<AtomicU64>,
    last_received_payload: Arc<Mutex<Vec<u8>>>,
    last_received_raw: Arc<Mutex<Vec<u8>>>,
    all_received_raw: Arc<Mutex<Vec<Vec<u8>>>>,
    recv_handle: Option<tokio::task::JoinHandle<()>>,
    last_dtmf_timestamp: Option<u32>,
}

impl TestRtpPeer {
    pub async fn new() -> Self {
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let local_addr = socket.local_addr().unwrap();
        Self {
            socket: Arc::new(socket),
            local_addr,
            remote_addr: None,
            ssrc: rand::random(),
            seq_no: 0,
            timestamp: 0,
            packets_received: Arc::new(AtomicU64::new(0)),
            last_received_payload: Arc::new(Mutex::new(Vec::new())),
            last_received_raw: Arc::new(Mutex::new(Vec::new())),
            all_received_raw: Arc::new(Mutex::new(Vec::new())),
            recv_handle: None,
            last_dtmf_timestamp: None,
        }
    }

    pub fn set_remote(&mut self, addr: SocketAddr) {
        self.remote_addr = Some(addr);
    }

    /// Build a plain RTP/AVP SDP offer pointing to this peer's address
    pub fn make_sdp_offer(&self) -> String {
        format!(
            "v=0\r\n\
             o=- 100 1 IN IP4 {ip}\r\n\
             s=-\r\n\
             c=IN IP4 {ip}\r\n\
             t=0 0\r\n\
             m=audio {port} RTP/AVP 0 101\r\n\
             a=rtpmap:0 PCMU/8000\r\n\
             a=rtpmap:101 telephone-event/8000\r\n\
             a=fmtp:101 0-16\r\n\
             a=sendrecv\r\n",
            ip = self.local_addr.ip(),
            port = self.local_addr.port(),
        )
    }

    /// Build a plain RTP SDP answer pointing to this peer's address
    pub fn make_sdp_answer(&self) -> String {
        format!(
            "v=0\r\n\
             o=- 200 1 IN IP4 {ip}\r\n\
             s=-\r\n\
             c=IN IP4 {ip}\r\n\
             t=0 0\r\n\
             m=audio {port} RTP/AVP 0 101\r\n\
             a=rtpmap:0 PCMU/8000\r\n\
             a=rtpmap:101 telephone-event/8000\r\n\
             a=fmtp:101 0-16\r\n\
             a=sendrecv\r\n",
            ip = self.local_addr.ip(),
            port = self.local_addr.port(),
        )
    }

    /// Send a raw RTP packet with PCMU payload
    pub async fn send_pcmu(&mut self, payload: &[u8]) {
        let remote = self.remote_addr.expect("remote address not set");
        let packet = build_rtp_packet(0, self.seq_no, self.timestamp, self.ssrc, false, payload);
        self.socket.send_to(&packet, remote).await.unwrap();
        self.seq_no = self.seq_no.wrapping_add(1);
        self.timestamp = self.timestamp.wrapping_add(160);
    }

    /// Send an RFC 4733 DTMF event packet
    pub async fn send_dtmf_event(&mut self, event_id: u8, end: bool, duration: u16) {
        let remote = self.remote_addr.expect("remote address not set");
        let mut dtmf_payload = vec![0u8; 4];
        dtmf_payload[0] = event_id;
        dtmf_payload[1] = if end { 0x80 } else { 0x00 } | 10; // E bit + volume=10
        dtmf_payload[2] = (duration >> 8) as u8;
        dtmf_payload[3] = duration as u8;

        // RFC 4733: marker only on the FIRST packet of a new event
        let is_new_event = self.last_dtmf_timestamp != Some(self.timestamp);
        self.last_dtmf_timestamp = Some(self.timestamp);
        let marker = is_new_event && !end;

        let packet = build_rtp_packet(
            101, // telephone-event PT
            self.seq_no,
            self.timestamp,
            self.ssrc,
            marker,
            &dtmf_payload,
        );
        self.socket.send_to(&packet, remote).await.unwrap();
        self.seq_no = self.seq_no.wrapping_add(1);
        // Don't advance timestamp for DTMF (same timestamp for entire event)
    }

    /// Send RTP packets with a 440Hz sine tone encoded as PCMU for the given duration
    pub async fn send_tone_for(&mut self, duration: Duration) {
        let ptime = Duration::from_millis(20);
        let mut elapsed = Duration::ZERO;
        while elapsed < duration {
            let payload: Vec<u8> = (0..160)
                .map(|i| {
                    let t = (self.timestamp as f64 + i as f64) / 8000.0;
                    let sample =
                        (f64::sin(2.0 * std::f64::consts::PI * 440.0 * t) * 16000.0) as i16;
                    linear_to_ulaw(sample)
                })
                .collect();
            self.send_pcmu(&payload).await;
            tokio::time::sleep(ptime).await;
            elapsed += ptime;
        }
    }

    /// Start a background receive loop that counts incoming RTP packets
    pub fn start_recv(&mut self) {
        let socket = Arc::clone(&self.socket);
        let counter = Arc::clone(&self.packets_received);
        let last_payload = Arc::clone(&self.last_received_payload);
        let last_raw = Arc::clone(&self.last_received_raw);
        let all_raw = Arc::clone(&self.all_received_raw);
        let handle = tokio::spawn(async move {
            let mut buf = vec![0u8; 2048];
            loop {
                match tokio::time::timeout(Duration::from_secs(10), socket.recv_from(&mut buf))
                    .await
                {
                    Ok(Ok((n, _))) => {
                        if n >= 12 {
                            counter.fetch_add(1, Ordering::Relaxed);
                            let raw = buf[..n].to_vec();
                            let payload = buf[12..n].to_vec();
                            *last_payload.lock().await = payload;
                            *last_raw.lock().await = raw.clone();
                            all_raw.lock().await.push(raw);
                        }
                    }
                    _ => break,
                }
            }
        });
        self.recv_handle = Some(handle);
    }

    pub fn received_count(&self) -> u64 {
        self.packets_received.load(Ordering::Relaxed)
    }

    /// Return the last received RTP payload bytes (after the 12-byte header)
    pub async fn last_payload(&self) -> Vec<u8> {
        self.last_received_payload.lock().await.clone()
    }

    /// Return the last received full raw packet bytes (including RTP header)
    pub async fn last_received_raw(&self) -> Vec<u8> {
        self.last_received_raw.lock().await.clone()
    }

    /// Return all received raw packets (including RTP headers)
    pub async fn all_received_raw(&self) -> Vec<Vec<u8>> {
        self.all_received_raw.lock().await.clone()
    }

    /// Extract RTP timestamps from all received packets.
    /// Useful for verifying monotonicity in mixer tests.
    pub async fn received_timestamps(&self) -> Vec<u32> {
        self.all_received_raw
            .lock()
            .await
            .iter()
            .filter(|pkt| pkt.len() >= 12)
            .map(|pkt| u32::from_be_bytes([pkt[4], pkt[5], pkt[6], pkt[7]]))
            .collect()
    }

    /// Send a single silent RTP packet to activate symmetric RTP on the server.
    /// Must be called after `set_remote()` and before expecting to receive packets.
    /// The server's `write_rtp` guard requires at least one inbound packet to set
    /// `remote_ssrc` before it will send anything back.
    pub async fn activate(&mut self) {
        let silence = vec![0xFFu8; 160]; // mu-law silence
        self.send_pcmu(&silence).await;
    }

    /// Build a plain RTP/AVP SDP offer with G.722 codec
    pub fn make_g722_sdp_offer(&self) -> String {
        format!(
            "v=0\r\n\
             o=- 100 1 IN IP4 {ip}\r\n\
             s=-\r\n\
             c=IN IP4 {ip}\r\n\
             t=0 0\r\n\
             m=audio {port} RTP/AVP 9 101\r\n\
             a=rtpmap:9 G722/8000\r\n\
             a=rtpmap:101 telephone-event/8000\r\n\
             a=fmtp:101 0-16\r\n\
             a=sendrecv\r\n",
            ip = self.local_addr.ip(),
            port = self.local_addr.port(),
        )
    }

    /// Build a plain RTP SDP answer with G.722 codec
    pub fn make_g722_sdp_answer(&self) -> String {
        format!(
            "v=0\r\n\
             o=- 200 1 IN IP4 {ip}\r\n\
             s=-\r\n\
             c=IN IP4 {ip}\r\n\
             t=0 0\r\n\
             m=audio {port} RTP/AVP 9 101\r\n\
             a=rtpmap:9 G722/8000\r\n\
             a=rtpmap:101 telephone-event/8000\r\n\
             a=fmtp:101 0-16\r\n\
             a=sendrecv\r\n",
            ip = self.local_addr.ip(),
            port = self.local_addr.port(),
        )
    }

    /// Send a raw RTP packet with G.722 payload
    pub async fn send_g722(&mut self, payload: &[u8]) {
        let remote = self.remote_addr.expect("remote address not set");
        let packet = build_rtp_packet(9, self.seq_no, self.timestamp, self.ssrc, false, payload);
        self.socket.send_to(&packet, remote).await.unwrap();
        self.seq_no = self.seq_no.wrapping_add(1);
        // G.722 RTP clock rate is 8kHz despite 16kHz audio — 160 ticks per 20ms
        self.timestamp = self.timestamp.wrapping_add(160);
    }

    /// Send RTP packets with a 440Hz sine tone encoded as G.722 for the given duration
    pub async fn send_g722_tone_for(&mut self, duration: Duration) {
        use rtpbridge::media::codec::G722Encoder;
        let mut encoder = G722Encoder::new();
        let ptime = Duration::from_millis(20);
        let mut elapsed = Duration::ZERO;
        let mut sample_offset: u64 = 0;
        while elapsed < duration {
            // G.722 operates at 16kHz, 20ms = 320 samples
            let pcm: Vec<i16> = (0..320)
                .map(|i| {
                    let t = (sample_offset + i as u64) as f64 / 16000.0;
                    (f64::sin(2.0 * std::f64::consts::PI * 440.0 * t) * 16000.0) as i16
                })
                .collect();
            sample_offset += 320;
            let mut encoded = Vec::new();
            rtpbridge::media::codec::AudioEncoder::encode(&mut encoder, &pcm, &mut encoded)
                .unwrap();
            self.send_g722(&encoded).await;
            tokio::time::sleep(ptime).await;
            elapsed += ptime;
        }
    }

    /// Build a plain RTP/AVP SDP offer with a custom telephone-event payload type
    pub fn make_sdp_offer_with_te_pt(&self, te_pt: u8) -> String {
        format!(
            "v=0\r\n\
             o=- 100 1 IN IP4 {ip}\r\n\
             s=-\r\n\
             c=IN IP4 {ip}\r\n\
             t=0 0\r\n\
             m=audio {port} RTP/AVP 0 {te_pt}\r\n\
             a=rtpmap:0 PCMU/8000\r\n\
             a=rtpmap:{te_pt} telephone-event/8000\r\n\
             a=fmtp:{te_pt} 0-16\r\n\
             a=sendrecv\r\n",
            ip = self.local_addr.ip(),
            port = self.local_addr.port(),
            te_pt = te_pt,
        )
    }
}

impl Drop for TestRtpPeer {
    fn drop(&mut self) {
        if let Some(handle) = self.recv_handle.take() {
            handle.abort();
        }
    }
}

/// Build a raw RTP packet
pub fn build_rtp_packet(
    pt: u8,
    seq: u16,
    ts: u32,
    ssrc: u32,
    marker: bool,
    payload: &[u8],
) -> Vec<u8> {
    let mut buf = Vec::with_capacity(12 + payload.len());
    buf.push(0x80); // V=2, P=0, X=0, CC=0
    buf.push(if marker { 0x80 | pt } else { pt });
    buf.extend_from_slice(&seq.to_be_bytes());
    buf.extend_from_slice(&ts.to_be_bytes());
    buf.extend_from_slice(&ssrc.to_be_bytes());
    buf.extend_from_slice(payload);
    buf
}

/// Parse server's SDP to extract the RTP address
pub fn parse_rtp_addr_from_sdp(sdp: &str) -> Option<SocketAddr> {
    let mut ip = String::new();
    let mut port = 0u16;
    for line in sdp.lines() {
        let line = line.trim();
        if let Some(addr) = line.strip_prefix("c=IN IP4 ") {
            ip = addr.to_string();
        }
        if line.starts_with("m=audio ") {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 2 {
                port = parts[1].parse().ok()?;
            }
        }
    }
    if !ip.is_empty() && port > 0 {
        Some(SocketAddr::new(ip.parse().ok()?, port))
    } else {
        None
    }
}

/// Simple linear-to-mu-law encoding (ITU-T G.711)
fn linear_to_ulaw(sample: i16) -> u8 {
    let sign = if sample < 0 { 0x80u8 } else { 0 };
    let mut magnitude = if sample < 0 {
        -(sample as i32)
    } else {
        sample as i32
    };
    magnitude = magnitude.min(0x7FFF);
    magnitude += 0x84; // bias

    let mut exponent = 7u8;
    let mut mask = 0x4000i32;
    while exponent > 0 && (magnitude & mask) == 0 {
        exponent -= 1;
        mask >>= 1;
    }
    let mantissa = ((magnitude >> (exponent as u32 + 3)) & 0x0F) as u8;
    !(sign | (exponent << 4) | mantissa)
}
