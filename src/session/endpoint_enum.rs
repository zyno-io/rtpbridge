use tokio::sync::mpsc;

use super::endpoint::InboundPacket;
use super::endpoint_bridge::BridgeEndpoint;
use super::endpoint_file::FileEndpoint;
use super::endpoint_rtp::RtpEndpoint;
use super::endpoint_tone::ToneEndpoint;
use super::endpoint_webrtc::WebRtcEndpoint;
use super::endpoint_websocket::WebSocketEndpoint;
use super::stats::EndpointStats;
use crate::control::protocol::{EndpointDirection, EndpointId, EndpointState};
use crate::media::codec::AudioCodec;
use crate::recording::meta::StreamDescriptor;
use std::net::SocketAddr;

/// Unified endpoint wrapping WebRTC, plain RTP, file playback, and bridge
pub enum Endpoint {
    WebRtc(Box<WebRtcEndpoint>),
    Rtp(Box<RtpEndpoint>),
    File(Box<FileEndpoint>),
    Tone(Box<ToneEndpoint>),
    Bridge(Box<BridgeEndpoint>),
    WebSocket(Box<WebSocketEndpoint>),
}

impl Endpoint {
    pub fn id(&self) -> EndpointId {
        match self {
            Endpoint::WebRtc(ep) => ep.id,
            Endpoint::Rtp(ep) => ep.id,
            Endpoint::File(ep) => ep.id,
            Endpoint::Tone(ep) => ep.id,
            Endpoint::Bridge(ep) => ep.id,
            Endpoint::WebSocket(ep) => ep.id,
        }
    }

    pub fn direction(&self) -> EndpointDirection {
        match self {
            Endpoint::WebRtc(ep) => ep.config.direction,
            Endpoint::Rtp(ep) => ep.config.direction,
            Endpoint::File(ep) => ep.config.direction,
            Endpoint::Tone(ep) => ep.config.direction,
            Endpoint::Bridge(ep) => ep.config.direction,
            Endpoint::WebSocket(ep) => ep.config.direction,
        }
    }

    pub fn state(&self) -> EndpointState {
        match self {
            Endpoint::WebRtc(ep) => ep.state,
            Endpoint::Rtp(ep) => ep.state,
            Endpoint::File(ep) => ep.state,
            Endpoint::Tone(ep) => ep.state,
            Endpoint::Bridge(ep) => ep.state,
            Endpoint::WebSocket(ep) => ep.state,
        }
    }

    pub fn stats(&self) -> &EndpointStats {
        match self {
            Endpoint::WebRtc(ep) => &ep.stats,
            Endpoint::Rtp(ep) => &ep.stats,
            Endpoint::File(ep) => &ep.stats,
            Endpoint::Tone(ep) => &ep.stats,
            Endpoint::Bridge(ep) => &ep.stats,
            Endpoint::WebSocket(ep) => &ep.stats,
        }
    }

    pub fn accept_answer(&mut self, sdp: &str) -> anyhow::Result<()> {
        match self {
            Endpoint::WebRtc(ep) => ep.accept_answer(sdp),
            Endpoint::Rtp(ep) => ep.accept_answer(sdp),
            Endpoint::File(_)
            | Endpoint::Tone(_)
            | Endpoint::Bridge(_)
            | Endpoint::WebSocket(_) => Err(anyhow::anyhow!(
                "This endpoint type doesn't accept SDP answers"
            )),
        }
    }

    pub fn accept_offer(&mut self, sdp: &str) -> anyhow::Result<String> {
        match self {
            Endpoint::WebRtc(ep) => ep.accept_offer(sdp),
            _ => anyhow::bail!("accept_offer only supported on WebRTC endpoints"),
        }
    }

    pub fn update_remote_sdp(&mut self, sdp: &str) -> anyhow::Result<String> {
        match self {
            Endpoint::Rtp(ep) => ep.update_remote_sdp(sdp),
            _ => anyhow::bail!("update_remote_sdp only supported on RTP endpoints"),
        }
    }

    pub fn telephone_event_pt(&self) -> Option<u8> {
        match self {
            Endpoint::WebRtc(_) => Some(101),
            Endpoint::Rtp(ep) => ep.telephone_event_pt,
            Endpoint::File(_)
            | Endpoint::Tone(_)
            | Endpoint::Bridge(_)
            | Endpoint::WebSocket(_) => None,
        }
    }

    /// Codec name for stats (e.g. "PCMU", "opus")
    pub fn codec_name(&self) -> String {
        match self {
            Endpoint::Rtp(ep) => ep
                .send_codec
                .as_ref()
                .map(|c| c.name.to_string())
                .unwrap_or_default(),
            Endpoint::WebRtc(ep) => ep
                .negotiated_codec()
                .map(|c| c.name.to_string())
                .unwrap_or_else(|| "opus".to_string()),
            Endpoint::File(ep) => format!("L16/{}", ep.sample_rate()),
            Endpoint::Tone(ep) => format!("tone/{}Hz", ep.sample_rate()),
            Endpoint::Bridge(_) => "L16/48000".to_string(),
            Endpoint::WebSocket(ep) => ep.codec_label(),
        }
    }

    /// Packets lost (from RTCP stats). Available for any endpoint with an
    /// inbound RTP stream we track (plain RTP and WebRTC).
    pub fn packets_lost(&self) -> u64 {
        match self {
            Endpoint::Rtp(ep) => ep.rtcp_stats.cumulative_lost() as u64,
            Endpoint::WebRtc(ep) => ep.rtcp_stats.cumulative_lost() as u64,
            _ => 0,
        }
    }

    /// Jitter in milliseconds (from RTCP stats). Available for any endpoint
    /// with an inbound RTP stream we track (plain RTP and WebRTC).
    pub fn jitter_ms(&self) -> f64 {
        match self {
            // Jitter is accumulated in microseconds — convert to ms
            Endpoint::Rtp(ep) => ep.rtcp_stats.jitter as f64 / 1000.0,
            Endpoint::WebRtc(ep) => ep.rtcp_stats.jitter as f64 / 1000.0,
            _ => 0.0,
        }
    }

    /// RTT in milliseconds (computed from remote's RR via RFC 3550 §6.4.1).
    pub fn rtt_ms(&self) -> Option<f64> {
        match self {
            Endpoint::Rtp(ep) => ep.rtcp_stats.rtt_ms,
            _ => None,
        }
    }

    /// Wire-level inbound datagram count: ALL UDP datagrams received on the
    /// endpoint's socket(s) before any demux/parse — STUN/ICE bindings, DTLS,
    /// RTCP, RTP, and malformed junk. `None` for endpoint types with no UDP
    /// socket (file/tone/bridge/websocket).
    ///
    /// This diverging from `stats().inbound_packets` (validated media only)
    /// while it keeps climbing means the remote path is alive but sending no
    /// media; both flat means the path is dead — the remote-network-failure
    /// signal that media-only counters cannot distinguish from peer silence.
    pub fn raw_recv_packets(&self) -> Option<u64> {
        match self {
            Endpoint::WebRtc(ep) => Some(ep.raw_recv.packets()),
            Endpoint::Rtp(ep) => Some(ep.raw_recv.packets()),
            _ => None,
        }
    }

    /// Wire-level inbound byte count, paired with [`Self::raw_recv_packets`].
    pub fn raw_recv_bytes(&self) -> Option<u64> {
        match self {
            Endpoint::WebRtc(ep) => Some(ep.raw_recv.bytes()),
            Endpoint::Rtp(ep) => Some(ep.raw_recv.bytes()),
            _ => None,
        }
    }

    /// str0m ICE connection state (WebRTC only), lowercased: `"new"`,
    /// `"checking"`, `"connected"`, `"completed"`, `"disconnected"`. `None` for
    /// non-WebRTC endpoints or before the first ICE transition. `"disconnected"`
    /// is str0m's RFC 7675 consent-freshness verdict — the canonical
    /// remote-path-lost signal.
    pub fn ice_state(&self) -> Option<&'static str> {
        match self {
            Endpoint::WebRtc(ep) => ep
                .ice_connection_state
                .map(super::endpoint_webrtc::ice_state_str),
            _ => None,
        }
    }

    /// Whether this endpoint is a bridge
    pub fn is_bridge(&self) -> bool {
        matches!(self, Endpoint::Bridge(_))
    }

    /// Stop recv tasks for transfer. Cancels tokens, awaits handles, resets for restart.
    pub async fn stop_recv_tasks(&mut self) {
        match self {
            Endpoint::WebRtc(ep) => ep.stop_recv_tasks().await,
            Endpoint::Rtp(ep) => ep.stop_recv_tasks().await,
            Endpoint::WebSocket(ep) => ep.stop_io_task().await,
            Endpoint::File(_) | Endpoint::Tone(_) | Endpoint::Bridge(_) => {} // no recv tasks
        }
    }

    /// Restart recv tasks with a new session's packet_tx (after transfer).
    pub fn restart_recv_tasks(&mut self, packet_tx: mpsc::Sender<InboundPacket>) {
        match self {
            Endpoint::WebRtc(ep) => ep.restart_recv_tasks(packet_tx),
            Endpoint::Rtp(ep) => ep.restart_recv_tasks(packet_tx),
            // WebSocket endpoints can't be transferred, so this is never reached for them.
            Endpoint::File(_)
            | Endpoint::Tone(_)
            | Endpoint::Bridge(_)
            | Endpoint::WebSocket(_) => {}
        }
    }
}

/// Determine the audio codec for an endpoint (for transcoding decisions)
pub fn endpoint_audio_codec(ep: &Endpoint) -> Option<AudioCodec> {
    match ep {
        Endpoint::Rtp(rep) => rep
            .send_codec
            .as_ref()
            .and_then(|c| AudioCodec::from_name(c.name)),
        Endpoint::WebRtc(_) => Some(AudioCodec::Opus),
        // File emits native-rate L16 (PT 127) so the per-edge transcode/mixer
        // encodes at each destination's full quality rather than 8 kHz µ-law.
        Endpoint::File(ep) => Some(AudioCodec::L16 {
            sample_rate: ep.sample_rate(),
        }),
        Endpoint::Tone(_) => Some(AudioCodec::Pcmu),
        Endpoint::Bridge(_) => Some(AudioCodec::L16 { sample_rate: 48000 }),
        // WebSocket runs internally at its wire rate (no 48 kHz pivot); the
        // session's per-edge resampling handles any rate difference to peers.
        Endpoint::WebSocket(ep) => Some(AudioCodec::L16 {
            sample_rate: ep.sample_rate,
        }),
    }
}

/// Get the send payload type for an endpoint
pub fn endpoint_send_pt(ep: &Endpoint) -> Option<u8> {
    match ep {
        Endpoint::Rtp(rep) => rep.send_codec.as_ref().map(|c| c.pt),
        Endpoint::WebRtc(ep) => Some(ep.negotiated_codec().map(|c| c.pt).unwrap_or(111)),
        Endpoint::File(_) => Some(127), // L16
        Endpoint::Tone(_) => Some(0),
        Endpoint::Bridge(_) => Some(127),
        Endpoint::WebSocket(_) => Some(127),
    }
}

/// Build the recording stream descriptor for an endpoint, framed with the given
/// real socket addresses (or `None` → synthetic markers for internal sources).
/// Returns `None` when the endpoint isn't ready to be described (e.g. a plain-RTP
/// endpoint whose codec hasn't been negotiated yet).
pub fn endpoint_stream_descriptor(
    ep: &Endpoint,
    local: Option<SocketAddr>,
    remote: Option<SocketAddr>,
) -> Option<StreamDescriptor> {
    // WebRTC builds its own from str0m's negotiated codec.
    if let Endpoint::WebRtc(w) = ep {
        return w.stream_descriptor(local, remote);
    }

    let addr = |a: Option<SocketAddr>| a.map(|x| x.to_string()).unwrap_or_else(|| "-".to_string());
    let (role, ep_type, codec, pt, clock_rate, channels, endian) = match ep {
        Endpoint::Rtp(rep) => {
            let c = rep.send_codec.as_ref()?;
            (
                "remote",
                "rtp",
                c.name.to_string(),
                c.pt,
                c.clock_rate,
                c.channels.unwrap_or(1),
                None,
            )
        }
        // Internal generators. File emits native-rate L16 (little-endian); tone is
        // synthesized 8 kHz µ-law; bridge/websocket are L16 at their wire rate.
        Endpoint::File(f) => (
            "internal",
            "file",
            "L16".to_string(),
            127,
            f.sample_rate(),
            1,
            Some("le".to_string()),
        ),
        Endpoint::Tone(_) => ("internal", "tone", "PCMU".to_string(), 0, 8000, 1, None),
        Endpoint::Bridge(_) => (
            "internal",
            "bridge",
            "L16".to_string(),
            127,
            48000,
            1,
            Some("le".to_string()),
        ),
        Endpoint::WebSocket(w) => (
            "internal",
            "websocket",
            "L16".to_string(),
            127,
            w.sample_rate,
            1,
            Some("le".to_string()),
        ),
        Endpoint::WebRtc(_) => unreachable!("handled above"),
    };

    Some(StreamDescriptor {
        v: crate::recording::meta::VERSION,
        endpoint_id: ep.id().to_string(),
        role: role.to_string(),
        ep_type: ep_type.to_string(),
        codec,
        pt,
        clock_rate,
        channels,
        endian,
        ssrc: None,
        local: addr(local),
        remote: addr(remote),
    })
}

/// Get the RTP clock rate for an endpoint's send codec
pub fn endpoint_rtp_clock_rate(ep: &Endpoint) -> u32 {
    match ep {
        Endpoint::Rtp(rep) => rep
            .send_codec
            .as_ref()
            .map(|c| c.clock_rate)
            .unwrap_or(8000),
        Endpoint::WebRtc(_) => 48000,
        Endpoint::File(ep) => ep.sample_rate(),
        Endpoint::Tone(_) => 8000,
        Endpoint::Bridge(_) => 48000,
        Endpoint::WebSocket(ep) => ep.sample_rate,
    }
}

/// Get the last outbound RTP timestamp from an endpoint (if available).
/// Used to seed mixer timestamps for seamless transitions.
pub fn endpoint_last_rtp_timestamp(ep: &Endpoint) -> Option<u32> {
    match ep {
        Endpoint::Rtp(rep) if rep.last_rtp_timestamp != 0 => Some(rep.last_rtp_timestamp),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::protocol::{EndpointDirection, EndpointId, EndpointState};
    use crate::net::socket_pool::SocketPool;
    use crate::session::endpoint_file::FileEndpoint;
    use crate::session::endpoint_rtp::RtpEndpoint;
    use std::io::Write;

    /// Generate a minimal WAV file with a sine tone
    fn test_wav(path: &str, duration_secs: f64) {
        let sample_rate: u32 = 8000;
        let num_samples = (sample_rate as f64 * duration_secs) as usize;
        let data_size = (num_samples * 2) as u32;

        let mut f = std::fs::File::create(path).unwrap();
        f.write_all(b"RIFF").unwrap();
        f.write_all(&(36 + data_size).to_le_bytes()).unwrap();
        f.write_all(b"WAVE").unwrap();
        f.write_all(b"fmt ").unwrap();
        f.write_all(&16u32.to_le_bytes()).unwrap();
        f.write_all(&1u16.to_le_bytes()).unwrap(); // PCM
        f.write_all(&1u16.to_le_bytes()).unwrap(); // mono
        f.write_all(&sample_rate.to_le_bytes()).unwrap();
        f.write_all(&(sample_rate * 2).to_le_bytes()).unwrap();
        f.write_all(&2u16.to_le_bytes()).unwrap();
        f.write_all(&16u16.to_le_bytes()).unwrap();
        f.write_all(b"data").unwrap();
        f.write_all(&data_size.to_le_bytes()).unwrap();
        for i in 0..num_samples {
            let t = i as f64 / sample_rate as f64;
            let sample = (f64::sin(2.0 * std::f64::consts::PI * 440.0 * t) * 16000.0) as i16;
            f.write_all(&sample.to_le_bytes()).unwrap();
        }
    }

    /// Helper: create an RtpEndpoint wrapped in Endpoint::Rtp with the given direction
    async fn make_rtp_endpoint(direction: EndpointDirection) -> (Endpoint, EndpointId) {
        let pool = SocketPool::new("127.0.0.1".parse().unwrap(), 52000, 52100).unwrap();
        let pair = pool.allocate_pair().await.unwrap();
        let id = EndpointId::new_v4();
        let ep = RtpEndpoint::new(id, direction, pair);
        (Endpoint::Rtp(Box::new(ep)), id)
    }

    /// Helper: create a FileEndpoint wrapped in Endpoint::File
    fn make_file_endpoint(path: &str) -> (Endpoint, EndpointId) {
        test_wav(path, 0.5);
        let id = EndpointId::new_v4();
        let ep = FileEndpoint::open(id, path, 0, None, 0.0).unwrap();
        (Endpoint::File(Box::new(ep)), id)
    }

    // ── id() tests ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_id_returns_correct_id_for_rtp() {
        let (ep, expected_id) = make_rtp_endpoint(EndpointDirection::SendRecv).await;
        assert_eq!(ep.id(), expected_id);
    }

    #[test]
    fn test_id_returns_correct_id_for_file() {
        let path = "/tmp/rtpbridge-test-enum-id-file.wav";
        let (ep, expected_id) = make_file_endpoint(path);
        assert_eq!(ep.id(), expected_id);
        std::fs::remove_file(path).ok();
    }

    // ── direction() tests ───────────────────────────────────────────────

    #[tokio::test]
    async fn test_direction_returns_sendrecv_for_rtp() {
        let (ep, _) = make_rtp_endpoint(EndpointDirection::SendRecv).await;
        assert_eq!(ep.direction(), EndpointDirection::SendRecv);
    }

    #[tokio::test]
    async fn test_direction_returns_recvonly_for_rtp() {
        let (ep, _) = make_rtp_endpoint(EndpointDirection::RecvOnly).await;
        assert_eq!(ep.direction(), EndpointDirection::RecvOnly);
    }

    #[tokio::test]
    async fn test_direction_returns_sendonly_for_rtp() {
        let (ep, _) = make_rtp_endpoint(EndpointDirection::SendOnly).await;
        assert_eq!(ep.direction(), EndpointDirection::SendOnly);
    }

    #[test]
    fn test_direction_returns_sendonly_for_file() {
        let path = "/tmp/rtpbridge-test-enum-dir-file.wav";
        let (ep, _) = make_file_endpoint(path);
        assert_eq!(ep.direction(), EndpointDirection::SendOnly);
        std::fs::remove_file(path).ok();
    }

    // ── state() tests ───────────────────────────────────────────────────

    #[tokio::test]
    async fn test_state_returns_new_for_rtp() {
        let (ep, _) = make_rtp_endpoint(EndpointDirection::SendRecv).await;
        assert_eq!(ep.state(), EndpointState::New);
    }

    #[test]
    fn test_state_returns_playing_for_file() {
        let path = "/tmp/rtpbridge-test-enum-state-file.wav";
        let (ep, _) = make_file_endpoint(path);
        assert_eq!(ep.state(), EndpointState::Playing);
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn test_state_returns_buffering_for_buffering_file() {
        let id = EndpointId::new_v4();
        let ep = Endpoint::File(Box::new(FileEndpoint::new_buffering(id, 0.0)));
        assert_eq!(ep.state(), EndpointState::Buffering);
    }

    // ── raw_recv / ice_state accessor tests ─────────────────────────────

    #[tokio::test]
    async fn test_raw_recv_present_for_rtp_absent_for_file() {
        // RTP has a UDP socket: wire-level counters exist and start at zero.
        let (rtp_ep, _) = make_rtp_endpoint(EndpointDirection::SendRecv).await;
        assert_eq!(rtp_ep.raw_recv_packets(), Some(0));
        assert_eq!(rtp_ep.raw_recv_bytes(), Some(0));
        // Plain RTP has no ICE.
        assert_eq!(rtp_ep.ice_state(), None);

        // File has no UDP socket: no wire-level counter, no ICE.
        let path = "/tmp/rtpbridge-test-enum-rawrecv-file.wav";
        let (file_ep, _) = make_file_endpoint(path);
        assert_eq!(file_ep.raw_recv_packets(), None);
        assert_eq!(file_ep.raw_recv_bytes(), None);
        assert_eq!(file_ep.ice_state(), None);
        std::fs::remove_file(path).ok();
    }

    // ── accept_answer() tests ───────────────────────────────────────────

    #[test]
    fn test_accept_answer_returns_error_for_file() {
        let path = "/tmp/rtpbridge-test-enum-answer-file.wav";
        test_wav(path, 0.5);
        let id = EndpointId::new_v4();
        let file_ep = FileEndpoint::open(id, path, 0, None, 0.0).unwrap();
        let mut ep = Endpoint::File(Box::new(file_ep));

        let result = ep.accept_answer("v=0\r\no=- 0 0 IN IP4 0.0.0.0\r\ns=-\r\n");
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("doesn't accept SDP answers"),
            "error message should indicate SDP answers are not accepted"
        );

        std::fs::remove_file(path).ok();
    }

    #[tokio::test]
    async fn test_accept_answer_succeeds_for_rtp() {
        let (mut ep, _) = make_rtp_endpoint(EndpointDirection::SendRecv).await;

        let answer_sdp = "\
            v=0\r\n\
            o=- 123 1 IN IP4 10.0.0.1\r\n\
            s=-\r\n\
            c=IN IP4 10.0.0.1\r\n\
            t=0 0\r\n\
            m=audio 20000 RTP/AVP 0 101\r\n\
            a=rtpmap:0 PCMU/8000\r\n\
            a=rtpmap:101 telephone-event/8000\r\n\
            a=sendrecv\r\n";

        let result = ep.accept_answer(answer_sdp);
        assert!(
            result.is_ok(),
            "accept_answer should succeed for RTP endpoint"
        );
    }

    #[test]
    fn test_update_remote_sdp_returns_error_for_file() {
        let path = "/tmp/rtpbridge-test-enum-update-remote-file.wav";
        test_wav(path, 0.5);
        let id = EndpointId::new_v4();
        let file_ep = FileEndpoint::open(id, path, 0, None, 0.0).unwrap();
        let mut ep = Endpoint::File(Box::new(file_ep));

        let result = ep.update_remote_sdp("v=0\r\no=- 0 0 IN IP4 0.0.0.0\r\ns=-\r\n");
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("only supported on RTP endpoints"),
            "error message should indicate only RTP endpoints are supported"
        );

        std::fs::remove_file(path).ok();
    }

    #[tokio::test]
    async fn test_update_remote_sdp_succeeds_for_rtp() {
        let (mut ep, _) = make_rtp_endpoint(EndpointDirection::SendRecv).await;

        // First establish an initial answer
        let answer_sdp = "\
            v=0\r\n\
            o=- 123 1 IN IP4 10.0.0.1\r\n\
            s=-\r\n\
            c=IN IP4 10.0.0.1\r\n\
            t=0 0\r\n\
            m=audio 20000 RTP/AVP 0 101\r\n\
            a=rtpmap:0 PCMU/8000\r\n\
            a=rtpmap:101 telephone-event/8000\r\n\
            a=sendrecv\r\n";
        ep.accept_answer(answer_sdp).unwrap();

        // Now update with a re-INVITE SDP
        let reinvite_sdp = "\
            v=0\r\n\
            o=- 123 2 IN IP4 10.0.0.1\r\n\
            s=-\r\n\
            c=IN IP4 10.0.0.1\r\n\
            t=0 0\r\n\
            m=audio 30000 RTP/AVP 0 101\r\n\
            a=rtpmap:0 PCMU/8000\r\n\
            a=rtpmap:101 telephone-event/8000\r\n\
            a=sendrecv\r\n";

        let result = ep.update_remote_sdp(reinvite_sdp);
        assert!(
            result.is_ok(),
            "update_remote_sdp should succeed for RTP endpoint: {:?}",
            result
        );
    }
}
