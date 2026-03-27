use crate::control::protocol::{EndpointDirection, EndpointId};

/// Configuration for an endpoint
#[derive(Debug, Clone)]
pub struct EndpointConfig {
    pub direction: EndpointDirection,
}

/// Inbound packet from an endpoint's recv task
#[derive(Debug)]
pub struct InboundPacket {
    pub endpoint_id: EndpointId,
    pub source: std::net::SocketAddr,
    pub data: Vec<u8>,
    /// True when the packet arrived on the dedicated RTCP socket (not the RTP socket)
    pub is_rtcp: bool,
}

/// An RTP packet routed between endpoints (post-decryption)
#[derive(Debug, Clone)]
pub struct RoutedRtpPacket {
    pub source_endpoint_id: EndpointId,
    pub payload_type: u8,
    pub sequence_number: u16,
    pub timestamp: u32,
    pub ssrc: u32,
    pub marker: bool,
    pub payload: Vec<u8>,
}
