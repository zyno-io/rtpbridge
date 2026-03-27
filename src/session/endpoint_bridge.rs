use tokio::sync::mpsc;
use tracing::trace;

use super::endpoint::{EndpointConfig, InboundPacket, RoutedRtpPacket};
use super::stats::EndpointStats;
use crate::control::protocol::{EndpointDirection, EndpointId, EndpointState, SessionId};
use crate::session::media_session::SessionCommand;

/// A bridge endpoint that forwards audio between sessions via in-process channels.
/// Audio is carried as raw PCM (L16 at 48kHz) to avoid quality loss from double-encoding.
pub struct BridgeEndpoint {
    pub id: EndpointId,
    pub config: EndpointConfig,
    pub state: EndpointState,
    pub stats: EndpointStats,
    /// Channel to send packets to the paired session's inbound packet queue
    pub remote_packet_tx: mpsc::Sender<InboundPacket>,
    /// Endpoint ID of the paired bridge in the other session
    pub paired_endpoint_id: EndpointId,
    /// Session ID of the paired bridge's session
    pub paired_session_id: SessionId,
    /// Command channel to the paired session (for auto-remove on disconnect)
    pub paired_cmd_tx: mpsc::Sender<SessionCommand>,
}

impl BridgeEndpoint {
    pub fn new(
        id: EndpointId,
        direction: EndpointDirection,
        remote_packet_tx: mpsc::Sender<InboundPacket>,
        paired_endpoint_id: EndpointId,
        paired_session_id: SessionId,
        paired_cmd_tx: mpsc::Sender<SessionCommand>,
    ) -> Self {
        Self {
            id,
            config: EndpointConfig { direction },
            state: EndpointState::Connected,
            stats: EndpointStats::new(),
            remote_packet_tx,
            paired_endpoint_id,
            paired_session_id,
            paired_cmd_tx,
        }
    }

    /// Write an RTP packet through the bridge to the paired session.
    /// The packet is re-wrapped as an InboundPacket from the paired bridge endpoint.
    pub async fn write_rtp(&mut self, packet: &RoutedRtpPacket) -> anyhow::Result<()> {
        // Construct an InboundPacket that will appear as if it came from
        // the paired bridge endpoint in the other session
        let inbound = InboundPacket {
            endpoint_id: self.paired_endpoint_id,
            source: "0.0.0.0:0".parse().unwrap(), // bridge packets have no real source addr
            data: packet.payload.clone(),
            is_rtcp: false,
        };

        // Non-blocking send — drop on backpressure rather than blocking the routing loop
        match self.remote_packet_tx.try_send(inbound) {
            Ok(()) => {
                self.stats.record_outbound(packet.payload.len());
            }
            Err(mpsc::error::TrySendError::Full(_)) => {
                trace!(bridge_id = %self.id, "bridge packet dropped (backpressure)");
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                trace!(bridge_id = %self.id, "bridge channel closed");
            }
        }
        Ok(())
    }
}

impl std::fmt::Debug for BridgeEndpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BridgeEndpoint")
            .field("id", &self.id)
            .field("paired_endpoint_id", &self.paired_endpoint_id)
            .field("paired_session_id", &self.paired_session_id)
            .finish_non_exhaustive()
    }
}
