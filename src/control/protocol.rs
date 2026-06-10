use serde::{Deserialize, Serialize};
use tracing::error;
use uuid::Uuid;

// ── ID Types ────────────────────────────────────────────────────────────

pub type SessionId = Uuid;
pub type EndpointId = Uuid;
pub type RecordingId = Uuid;

// ── Wire Format ─────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct Request {
    pub id: String,
    pub method: String,
    #[serde(default)]
    pub params: serde_json::Value,
}

#[derive(Debug, Serialize)]
pub struct Response {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorInfo>,
}

#[derive(Debug, Serialize)]
pub struct ErrorInfo {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Serialize)]
pub struct Event {
    pub event: String,
    pub data: serde_json::Value,
}

impl Response {
    pub fn ok(id: String, result: impl Serialize) -> Self {
        match serde_json::to_value(result) {
            Ok(val) => Self {
                id,
                result: Some(val),
                error: None,
            },
            Err(e) => {
                error!(error = %e, "Response serialization failed");
                Self::err(id, "INTERNAL_ERROR", "Failed to serialize response")
            }
        }
    }

    pub fn err(id: String, code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            id,
            result: None,
            error: Some(ErrorInfo {
                code: code.into(),
                message: message.into(),
            }),
        }
    }
}

impl Event {
    pub fn new(event: impl Into<String>, data: impl Serialize) -> Self {
        let data = match serde_json::to_value(data) {
            Ok(val) => val,
            Err(e) => {
                error!(error = %e, "Event serialization failed");
                serde_json::json!({"error": "serialization_failed"})
            }
        };
        Self {
            event: event.into(),
            data,
        }
    }
}

// ── Session Lifecycle ───────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct SessionCreateParams {}

#[derive(Debug, Serialize)]
pub struct SessionCreateResult {
    pub session_id: SessionId,
}

#[derive(Debug, Deserialize)]
pub struct SessionAttachParams {
    pub session_id: SessionId,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct SessionDestroyParams {}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct SessionInfoParams {}

#[derive(Debug, Serialize)]
pub struct SessionInfoResult {
    pub session_id: SessionId,
    pub state: SessionState,
    pub created_at: String,
    pub endpoints: Vec<EndpointInfo>,
    pub recordings: Vec<RecordingInfo>,
    pub vad_active: Vec<EndpointId>,
    pub fax_detect_active: Vec<EndpointId>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct SessionListParams {}

#[derive(Debug, Serialize)]
pub struct SessionListResult {
    pub sessions: Vec<SessionSummary>,
}

#[derive(Debug, Serialize)]
pub struct SessionSummary {
    pub session_id: SessionId,
    pub state: SessionState,
    pub endpoint_count: usize,
    pub created_at: String,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct ServerInfoParams {}

#[derive(Debug, Serialize)]
pub struct ServerInfoResult {
    pub hostname: String,
    pub media_ip: std::net::IpAddr,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    Active,
    Orphaned,
}

// ── Endpoint Creation ───────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct EndpointCreateFromOfferParams {
    pub sdp: String,
    #[serde(default = "default_direction")]
    pub direction: EndpointDirection,
}

#[derive(Debug, Serialize)]
pub struct EndpointCreateFromOfferResult {
    pub endpoint_id: EndpointId,
    pub sdp_answer: String,
}

#[derive(Debug, Deserialize)]
pub struct EndpointCreateOfferParams {
    #[serde(default = "default_direction")]
    pub direction: EndpointDirection,
    #[serde(rename = "type")]
    pub endpoint_type: EndpointType,
    #[serde(default)]
    pub srtp: bool,
    #[serde(default)]
    pub codecs: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
pub struct WebRtcCreateOfferParams {
    #[serde(default = "default_direction")]
    pub direction: EndpointDirection,
}

#[derive(Debug, Deserialize)]
pub struct RtpCreateOfferParams {
    #[serde(default = "default_direction")]
    pub direction: EndpointDirection,
    #[serde(default)]
    pub srtp: bool,
    #[serde(default)]
    pub codecs: Option<Vec<String>>,
}

#[derive(Debug, Serialize)]
pub struct EndpointCreateOfferResult {
    pub endpoint_id: EndpointId,
    pub sdp_offer: String,
}

#[derive(Debug, Deserialize)]
pub struct EndpointAcceptAnswerParams {
    pub endpoint_id: EndpointId,
    pub sdp: String,
    /// Offer generation this answer is for (from a prior `ice_restart` result).
    /// Optional and absent for the initial answer; when present the session
    /// rejects the answer unless it matches the endpoint's current pending
    /// offer generation. See docs/protocol/endpoints.md.
    #[serde(default)]
    pub offer_generation: Option<u64>,
}

#[derive(Debug, Deserialize)]
pub struct EndpointAcceptOfferParams {
    pub endpoint_id: EndpointId,
    pub sdp: String,
}

#[derive(Debug, Serialize)]
pub struct EndpointAcceptOfferResult {
    pub sdp_answer: String,
}

#[derive(Debug, Deserialize)]
pub struct EndpointCreateWithFileParams {
    pub source: String,
    #[serde(default)]
    pub start_ms: u64,
    #[serde(default = "default_loop_count")]
    pub loop_count: Option<u32>,
    #[serde(default = "default_cache_ttl")]
    pub cache_ttl_secs: u32,
    #[serde(default)]
    pub shared: bool,
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u32,
    #[serde(default)]
    pub headers: Option<std::collections::HashMap<String, String>>,
    /// Audio gain in dB (e.g. -6.0 to halve volume, +6.0 to double). Default 0.0.
    #[serde(default)]
    pub gain_db: f32,
}

#[derive(Debug, Serialize)]
pub struct EndpointCreateWithFileResult {
    pub endpoint_id: EndpointId,
}

#[derive(Debug, Deserialize)]
pub struct EndpointCreateToneParams {
    pub tone: crate::session::endpoint_tone::ToneType,
    pub frequency: Option<f64>,
    #[serde(default)]
    pub duration_ms: Option<u64>,
}

#[derive(Debug, Serialize)]
pub struct EndpointCreateToneResult {
    pub endpoint_id: EndpointId,
    pub tone: crate::session::endpoint_tone::ToneType,
}

#[derive(Debug, Deserialize)]
pub struct EndpointCreateWebSocketParams {
    #[serde(default = "default_direction")]
    pub direction: EndpointDirection,
    /// Wire PCM sample rate in Hz (8000 / 16000 / 48000). Mono, 16-bit LE.
    #[serde(default = "default_ws_sample_rate")]
    pub sample_rate: u32,
    /// Outbound coalescing window in ms (multiple of 20). 0 = passthrough.
    #[serde(default)]
    pub flush_ms: u32,
}

fn default_ws_sample_rate() -> u32 {
    8000
}

#[derive(Debug, Serialize)]
pub struct EndpointCreateWebSocketResult {
    pub endpoint_id: EndpointId,
    /// Single-use token; the audio peer dials in to `/audio/<connect_token>`.
    pub connect_token: String,
}

fn default_timeout_ms() -> u32 {
    10000
}

fn default_loop_count() -> Option<u32> {
    Some(0)
}

fn default_cache_ttl() -> u32 {
    300
}

// ── Endpoint Management ─────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct EndpointRemoveParams {
    pub endpoint_id: EndpointId,
}

#[derive(Debug, Deserialize)]
pub struct EndpointIceRestartParams {
    pub endpoint_id: EndpointId,
}

#[derive(Debug, Serialize)]
pub struct EndpointIceRestartResult {
    pub sdp_offer: String,
    /// Monotonic generation of this offer. Echo back on `accept_answer` as
    /// `offer_generation` so a stale answer for a superseded offer is rejected.
    pub offer_generation: u64,
}

// ── SRTP Rekey ──────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct EndpointSrtpRekeyParams {
    pub endpoint_id: EndpointId,
}

#[derive(Debug, Serialize)]
pub struct EndpointSrtpRekeyResult {
    pub sdp: String,
}

// ── Direction Update ───────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct EndpointUpdateDirectionParams {
    pub endpoint_id: EndpointId,
    pub direction: EndpointDirectionUpdate,
}

// ── Update Remote SDP (address + SRTP only, no codec changes) ─────

#[derive(Debug, Deserialize)]
pub struct EndpointUpdateRemoteSdpParams {
    pub endpoint_id: EndpointId,
    pub sdp: String,
}

// ── File Playback Control ───────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct FileSeekParams {
    pub endpoint_id: EndpointId,
    pub position_ms: u64,
}

#[derive(Debug, Deserialize)]
pub struct FilePauseParams {
    pub endpoint_id: EndpointId,
}

#[derive(Debug, Deserialize)]
pub struct FileResumeParams {
    pub endpoint_id: EndpointId,
}

// ── DTMF ────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct DtmfInjectParams {
    pub endpoint_id: EndpointId,
    pub digit: String,
    #[serde(default = "default_dtmf_duration")]
    pub duration_ms: u32,
    #[serde(default = "default_dtmf_volume")]
    pub volume: u8,
}

fn default_dtmf_duration() -> u32 {
    160
}
fn default_dtmf_volume() -> u8 {
    10
}

// ── Recording ───────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct RecordingStartParams {
    pub endpoint_id: Option<EndpointId>,
    pub file_path: String,
    /// When true, also capture the unencrypted outbound RTP/RTCP packets that
    /// rtpbridge writes toward each endpoint. Default false because outbound
    /// capture roughly doubles per-recording bandwidth and adds a clone of
    /// every wire packet on the hot path.
    #[serde(default)]
    pub record_outbound: bool,
}

#[derive(Debug, Serialize)]
pub struct RecordingStartResult {
    pub recording_id: RecordingId,
}

#[derive(Debug, Deserialize)]
pub struct RecordingStopParams {
    pub recording_id: RecordingId,
}

#[derive(Debug, Serialize)]
pub struct RecordingStopResult {
    pub file_path: String,
    pub duration_ms: u64,
    pub packets: u64,
    pub dropped_packets: u64,
}

#[derive(Debug, Serialize)]
pub struct RecordingInfo {
    pub recording_id: RecordingId,
    pub endpoint_id: Option<EndpointId>,
    pub file_path: String,
    pub state: RecordingState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordingState {
    Active,
    Stopped,
}

// ── VAD ─────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct VadStartParams {
    pub endpoint_id: EndpointId,
    #[serde(default = "default_silence_interval")]
    pub silence_interval_ms: u32,
    #[serde(default = "default_speech_threshold")]
    pub speech_threshold: f32,
}

fn default_silence_interval() -> u32 {
    1000
}
fn default_speech_threshold() -> f32 {
    0.5
}

#[derive(Debug, Deserialize)]
pub struct VadStopParams {
    pub endpoint_id: EndpointId,
}

// ── Fax tone detection ──────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct FaxDetectStartParams {
    pub endpoint_id: EndpointId,
}

#[derive(Debug, Deserialize)]
pub struct FaxDetectStopParams {
    pub endpoint_id: EndpointId,
}

// ── Stats ───────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct StatsSubscribeParams {
    #[serde(default = "default_stats_interval")]
    pub interval_ms: u32,
}

fn default_stats_interval() -> u32 {
    5000
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct StatsUnsubscribeParams {}

#[derive(Debug, Serialize)]
pub struct StatsEvent {
    pub endpoints: Vec<EndpointStats>,
}

#[derive(Debug, Serialize)]
pub struct EndpointStats {
    pub endpoint_id: EndpointId,
    pub inbound: InboundStats,
    pub outbound: OutboundStats,
    pub rtt_ms: Option<f64>,
    pub codec: String,
    pub state: String,
    /// str0m ICE connection state for WebRTC endpoints (`"new"`, `"checking"`,
    /// `"connected"`, `"completed"`, `"disconnected"`); omitted for non-WebRTC
    /// endpoints or before the first ICE transition. `"disconnected"` is ICE
    /// consent loss — a remote network-path failure.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ice_state: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct InboundStats {
    pub packets: u64,
    pub bytes: u64,
    pub packets_lost: u64,
    pub jitter_ms: f64,
    pub last_received_ms_ago: u64,
    /// Wire-level inbound datagrams on the endpoint's UDP socket(s): ALL packets
    /// received before any demux/parse — STUN/ICE bindings, DTLS, RTCP, RTP, and
    /// malformed junk. Omitted for endpoints with no UDP socket (file/tone/
    /// bridge/websocket). Compare against `packets`/`bytes` (validated media
    /// only): raw climbing while media is flat = remote alive but silent; both
    /// flat = remote path dead.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw_packets: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw_bytes: Option<u64>,
}

#[derive(Debug, Serialize)]
pub struct OutboundStats {
    pub packets: u64,
    pub bytes: u64,
}

// ── Shared Enums ────────────────────────────────────────────────────────

/// Media direction of an endpoint, expressed from the **peer's perspective** —
/// identical to the SDP `a=` attribute the peer negotiated. This is the single
/// convention used throughout the media plane (routing, file/tone sources, spy
/// sinks):
/// - `SendOnly` — the peer sends; the endpoint is a routing **source**
///   (rtpbridge receives from the peer and forwards), and rtpbridge does NOT
///   transmit to the peer.
/// - `RecvOnly` — the peer receives; the endpoint is a routing **destination**
///   (rtpbridge transmits to the peer), and its inbound is NOT forwarded.
/// - `SendRecv` — both. `Inactive` — neither.
///
/// `routing.rs` is the source of truth for this convention; the SDP→enum parse
/// (`endpoint_direction_from_sdp`) and `is_sending()` are kept consistent with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EndpointDirection {
    #[serde(rename = "sendrecv")]
    SendRecv,
    #[serde(rename = "recvonly")]
    RecvOnly,
    #[serde(rename = "sendonly")]
    SendOnly,
    #[serde(rename = "inactive")]
    Inactive,
}

impl EndpointDirection {
    /// Whether this direction permits the endpoint to transmit to its peer.
    /// Peer-perspective: rtpbridge transmits to a peer that is willing to
    /// receive, i.e. `SendRecv` or `RecvOnly`.
    pub fn is_sending(self) -> bool {
        matches!(self, Self::SendRecv | Self::RecvOnly)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EndpointDirectionUpdate {
    #[serde(rename = "auto")]
    Auto,
    #[serde(rename = "sendrecv")]
    SendRecv,
    #[serde(rename = "recvonly")]
    RecvOnly,
    #[serde(rename = "sendonly")]
    SendOnly,
    #[serde(rename = "inactive")]
    Inactive,
}

impl EndpointDirectionUpdate {
    pub fn as_direction(self) -> Option<EndpointDirection> {
        match self {
            EndpointDirectionUpdate::Auto => None,
            EndpointDirectionUpdate::SendRecv => Some(EndpointDirection::SendRecv),
            EndpointDirectionUpdate::RecvOnly => Some(EndpointDirection::RecvOnly),
            EndpointDirectionUpdate::SendOnly => Some(EndpointDirection::SendOnly),
            EndpointDirectionUpdate::Inactive => Some(EndpointDirection::Inactive),
        }
    }
}

fn default_direction() -> EndpointDirection {
    EndpointDirection::SendRecv
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndpointType {
    Webrtc,
    Rtp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndpointState {
    New,
    Buffering,
    Connecting,
    Connected,
    Playing,
    Paused,
    Disconnected,
    Finished,
}

#[derive(Debug, Serialize)]
pub struct EndpointInfo {
    pub endpoint_id: EndpointId,
    pub endpoint_type: String,
    pub direction: EndpointDirection,
    pub state: EndpointState,
    pub codec: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shared_playback_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local_rtp_addr: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local_rtcp_addr: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote_rtp_addr: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote_rtcp_addr: Option<String>,
}

// ── Event Data Types ────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct DtmfEventData {
    pub endpoint_id: EndpointId,
    pub digit: String,
    pub duration_ms: u32,
}

#[derive(Debug, Serialize)]
pub struct EndpointStateChangedData {
    pub endpoint_id: EndpointId,
    pub old_state: EndpointState,
    pub new_state: EndpointState,
}

/// `endpoint.ice_state_changed` — emitted when a WebRTC endpoint's str0m ICE
/// connection state transitions. Finer-grained than `endpoint.state_changed`;
/// `ice_state: "disconnected"` is ICE consent loss (RFC 7675), the canonical
/// signal that the remote network path has failed.
#[derive(Debug, Serialize)]
pub struct IceStateChangedData {
    pub endpoint_id: EndpointId,
    pub ice_state: String,
}

#[derive(Debug, Serialize)]
pub struct FileFinishedData {
    pub endpoint_id: EndpointId,
    pub reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ToneFinishedData {
    pub endpoint_id: EndpointId,
}

#[derive(Debug, Serialize)]
pub struct RecordingStoppedData {
    pub recording_id: RecordingId,
    pub file_path: String,
    pub duration_ms: u64,
    pub packets: u64,
    pub dropped_packets: u64,
    pub reason: String,
}

#[derive(Debug, Serialize)]
pub struct SessionOrphanedData {
    pub timeout_remaining_ms: u64,
}

#[derive(Debug, Serialize)]
pub struct MediaTimeoutData {
    pub endpoint_id: EndpointId,
    pub duration_ms: u64,
}

#[derive(Debug, Serialize)]
pub struct VadSpeechStartedData {
    pub endpoint_id: EndpointId,
}

#[derive(Debug, Serialize)]
pub struct VadSilenceData {
    pub endpoint_id: EndpointId,
    pub silence_duration_ms: u64,
}

/// Payload for `fax.cng_detected` and `fax.ced_detected` events.
#[derive(Debug, Serialize)]
pub struct FaxDetectedData {
    pub endpoint_id: EndpointId,
}

/// Payload for `endpoint.ws.connected` (audio socket attached).
#[derive(Debug, Serialize)]
pub struct WsConnectedData {
    pub endpoint_id: EndpointId,
}

/// Payload for `endpoint.ws.disconnected` (audio socket closed).
#[derive(Debug, Serialize)]
pub struct WsDisconnectedData {
    pub endpoint_id: EndpointId,
}

// ── Endpoint Transfer ──────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct EndpointTransferParams {
    pub endpoint_id: EndpointId,
    pub target_session_id: SessionId,
}

#[derive(Debug, Serialize)]
pub struct EndpointTransferResult {
    pub endpoint_id: EndpointId,
    pub target_session_id: SessionId,
}

#[derive(Debug, Serialize)]
pub struct EndpointTransferredOutData {
    pub endpoint_id: EndpointId,
    pub target_session_id: SessionId,
}

#[derive(Debug, Serialize)]
pub struct EndpointTransferredInData {
    pub endpoint_id: EndpointId,
    pub source_session_id: SessionId,
    pub endpoint_type: String,
    pub direction: EndpointDirection,
    pub state: EndpointState,
}

// ── Session Bridge ─────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct SessionBridgeParams {
    pub target_session_id: SessionId,
    #[serde(default = "default_sendrecv")]
    pub direction: EndpointDirection,
}

fn default_sendrecv() -> EndpointDirection {
    EndpointDirection::SendRecv
}

#[derive(Debug, Serialize)]
pub struct SessionBridgeResult {
    pub endpoint_id: EndpointId,
    pub target_endpoint_id: EndpointId,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    // ── Request deserialization ──────────────────────────────────────────

    #[test]
    fn request_valid_with_params() {
        let json = r#"{"id":"1","method":"session.create","params":{"key":"val"}}"#;
        let req: Request = serde_json::from_str(json).unwrap();
        assert_eq!(req.id, "1");
        assert_eq!(req.method, "session.create");
        assert_eq!(req.params["key"], "val");
    }

    #[test]
    fn request_missing_id_fails() {
        let json = r#"{"method":"session.create"}"#;
        let result = serde_json::from_str::<Request>(json);
        assert!(result.is_err());
    }

    #[test]
    fn request_missing_method_fails() {
        let json = r#"{"id":"1"}"#;
        let result = serde_json::from_str::<Request>(json);
        assert!(result.is_err());
    }

    #[test]
    fn request_extra_fields_ignored() {
        let json = r#"{"id":"1","method":"test","params":{},"extra":"ignored"}"#;
        let req: Request = serde_json::from_str(json).unwrap();
        assert_eq!(req.id, "1");
        assert_eq!(req.method, "test");
    }

    #[test]
    fn request_empty_params_default_to_null() {
        let json = r#"{"id":"1","method":"test"}"#;
        let req: Request = serde_json::from_str(json).unwrap();
        assert!(req.params.is_null());
    }

    // ── Response serialization ──────────────────────────────────────────

    #[test]
    fn response_ok_serialization() {
        let resp = Response::ok("42".into(), json!({"session_id": "abc"}));
        let val = serde_json::to_value(&resp).unwrap();
        assert_eq!(val["id"], "42");
        assert_eq!(val["result"]["session_id"], "abc");
        assert!(val.get("error").is_none());
    }

    #[test]
    fn response_err_serialization() {
        let resp = Response::err("42".into(), "NOT_FOUND", "Session not found");
        let val = serde_json::to_value(&resp).unwrap();
        assert_eq!(val["id"], "42");
        assert!(val.get("result").is_none());
        assert_eq!(val["error"]["code"], "NOT_FOUND");
        assert_eq!(val["error"]["message"], "Session not found");
    }

    #[test]
    fn response_ok_json_structure() {
        let resp = Response::ok("1".into(), json!("ok"));
        let serialized = serde_json::to_string(&resp).unwrap();
        let roundtrip: Value = serde_json::from_str(&serialized).unwrap();
        assert!(roundtrip.is_object());
        assert!(roundtrip.get("id").is_some());
        assert!(roundtrip.get("result").is_some());
        // error field omitted via skip_serializing_if
        assert!(!roundtrip.as_object().unwrap().contains_key("error"));
    }

    #[test]
    fn response_err_json_structure() {
        let resp = Response::err("1".into(), "ERR", "msg");
        let serialized = serde_json::to_string(&resp).unwrap();
        let roundtrip: Value = serde_json::from_str(&serialized).unwrap();
        assert!(roundtrip.get("error").is_some());
        // result field omitted via skip_serializing_if
        assert!(!roundtrip.as_object().unwrap().contains_key("result"));
    }

    // ── EndpointCreateWithFileParams defaults ───────────────────────────

    #[test]
    fn file_params_default_cache_ttl() {
        let json = r#"{"source":"test.wav"}"#;
        let params: EndpointCreateWithFileParams = serde_json::from_str(json).unwrap();
        assert_eq!(params.cache_ttl_secs, 300);
    }

    #[test]
    fn file_params_default_timeout_ms() {
        let json = r#"{"source":"test.wav"}"#;
        let params: EndpointCreateWithFileParams = serde_json::from_str(json).unwrap();
        assert_eq!(params.timeout_ms, 10000);
    }

    #[test]
    fn file_params_loop_count_play_once_by_default() {
        let json = r#"{"source":"test.wav"}"#;
        let params: EndpointCreateWithFileParams = serde_json::from_str(json).unwrap();
        assert_eq!(params.loop_count, Some(0));
    }

    #[test]
    fn file_params_loop_count_null_means_infinite() {
        let json = r#"{"source":"test.wav","loop_count":null}"#;
        let params: EndpointCreateWithFileParams = serde_json::from_str(json).unwrap();
        assert!(params.loop_count.is_none());
    }

    #[test]
    fn file_params_start_ms_defaults_zero() {
        let json = r#"{"source":"test.wav"}"#;
        let params: EndpointCreateWithFileParams = serde_json::from_str(json).unwrap();
        assert_eq!(params.start_ms, 0);
    }

    #[test]
    fn file_params_shared_defaults_false() {
        let json = r#"{"source":"test.wav"}"#;
        let params: EndpointCreateWithFileParams = serde_json::from_str(json).unwrap();
        assert!(!params.shared);
    }

    #[test]
    fn file_params_custom_values_override_defaults() {
        let json = r#"{"source":"test.wav","cache_ttl_secs":60,"timeout_ms":5000,"loop_count":3,"start_ms":100,"shared":true}"#;
        let params: EndpointCreateWithFileParams = serde_json::from_str(json).unwrap();
        assert_eq!(params.cache_ttl_secs, 60);
        assert_eq!(params.timeout_ms, 5000);
        assert_eq!(params.loop_count, Some(3));
        assert_eq!(params.start_ms, 100);
        assert!(params.shared);
    }

    // ── DtmfInjectParams defaults ───────────────────────────────────────

    #[test]
    fn dtmf_params_default_duration_ms() {
        let json = json!({
            "endpoint_id": "00000000-0000-0000-0000-000000000001",
            "digit": "5"
        });
        let params: DtmfInjectParams = serde_json::from_value(json).unwrap();
        assert_eq!(params.duration_ms, 160);
    }

    #[test]
    fn dtmf_params_default_volume() {
        let json = json!({
            "endpoint_id": "00000000-0000-0000-0000-000000000001",
            "digit": "5"
        });
        let params: DtmfInjectParams = serde_json::from_value(json).unwrap();
        assert_eq!(params.volume, 10);
    }

    #[test]
    fn dtmf_params_custom_values() {
        let json = json!({
            "endpoint_id": "00000000-0000-0000-0000-000000000001",
            "digit": "#",
            "duration_ms": 320,
            "volume": 5
        });
        let params: DtmfInjectParams = serde_json::from_value(json).unwrap();
        assert_eq!(params.duration_ms, 320);
        assert_eq!(params.volume, 5);
        assert_eq!(params.digit, "#");
    }

    // ── VadStartParams defaults ─────────────────────────────────────────

    #[test]
    fn vad_params_default_speech_threshold() {
        let json = json!({
            "endpoint_id": "00000000-0000-0000-0000-000000000001"
        });
        let params: VadStartParams = serde_json::from_value(json).unwrap();
        assert!((params.speech_threshold - 0.5).abs() < f32::EPSILON);
    }

    #[test]
    fn vad_params_default_silence_interval_ms() {
        let json = json!({
            "endpoint_id": "00000000-0000-0000-0000-000000000001"
        });
        let params: VadStartParams = serde_json::from_value(json).unwrap();
        assert_eq!(params.silence_interval_ms, 1000);
    }

    #[test]
    fn vad_params_custom_values() {
        let json = json!({
            "endpoint_id": "00000000-0000-0000-0000-000000000001",
            "speech_threshold": 0.8,
            "silence_interval_ms": 2000
        });
        let params: VadStartParams = serde_json::from_value(json).unwrap();
        assert!((params.speech_threshold - 0.8).abs() < f32::EPSILON);
        assert_eq!(params.silence_interval_ms, 2000);
    }

    // ── EndpointUpdateDirectionParams parsing ──────────────────────────

    #[test]
    fn update_direction_accepts_auto() {
        let json = json!({
            "endpoint_id": "00000000-0000-0000-0000-000000000001",
            "direction": "auto"
        });
        let params: EndpointUpdateDirectionParams = serde_json::from_value(json).unwrap();
        assert_eq!(params.direction, EndpointDirectionUpdate::Auto);
    }

    #[test]
    fn update_direction_accepts_inactive() {
        let json = json!({
            "endpoint_id": "00000000-0000-0000-0000-000000000001",
            "direction": "inactive"
        });
        let params: EndpointUpdateDirectionParams = serde_json::from_value(json).unwrap();
        assert_eq!(params.direction, EndpointDirectionUpdate::Inactive);
    }

    // ── Event serialization ─────────────────────────────────────────────

    #[test]
    fn event_serializes_with_name_and_data() {
        let evt = Event::new("dtmf", json!({"digit": "1"}));
        let val = serde_json::to_value(&evt).unwrap();
        assert_eq!(val["event"], "dtmf");
        assert_eq!(val["data"]["digit"], "1");
    }

    #[test]
    fn event_serializes_complex_data() {
        let data = DtmfEventData {
            endpoint_id: Uuid::nil(),
            digit: "9".into(),
            duration_ms: 160,
        };
        let evt = Event::new("dtmf", data);
        let val = serde_json::to_value(&evt).unwrap();
        assert_eq!(val["event"], "dtmf");
        assert_eq!(val["data"]["digit"], "9");
        assert_eq!(val["data"]["duration_ms"], 160);
        assert_eq!(
            val["data"]["endpoint_id"],
            "00000000-0000-0000-0000-000000000000"
        );
    }

    #[test]
    fn event_only_has_event_and_data_fields() {
        let evt = Event::new("test", json!(null));
        let val = serde_json::to_value(&evt).unwrap();
        let obj = val.as_object().unwrap();
        assert_eq!(obj.len(), 2);
        assert!(obj.contains_key("event"));
        assert!(obj.contains_key("data"));
    }

    // ── SessionState enum ───────────────────────────────────────────────

    #[test]
    fn session_state_active_serializes() {
        let val = serde_json::to_value(SessionState::Active).unwrap();
        assert_eq!(val, "active");
    }

    #[test]
    fn session_state_orphaned_serializes() {
        let val = serde_json::to_value(SessionState::Orphaned).unwrap();
        assert_eq!(val, "orphaned");
    }

    #[test]
    fn session_state_active_deserializes() {
        let state: SessionState = serde_json::from_str(r#""active""#).unwrap();
        assert_eq!(state, SessionState::Active);
    }

    #[test]
    fn session_state_orphaned_deserializes() {
        let state: SessionState = serde_json::from_str(r#""orphaned""#).unwrap();
        assert_eq!(state, SessionState::Orphaned);
    }

    #[test]
    fn session_state_invalid_fails() {
        let result = serde_json::from_str::<SessionState>(r#""unknown""#);
        assert!(result.is_err());
    }
}
