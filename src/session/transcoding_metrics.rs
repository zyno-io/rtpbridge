use std::collections::HashMap;
use std::sync::Arc;

use super::endpoint_enum::{Endpoint, endpoint_audio_codec};
use super::routing::RoutingTable;
use crate::control::protocol::{EndpointId, SessionId};
use crate::metrics::Metrics;

/// Session-owned contributions, independent of encoder cache retention and
/// packet activity. Drop also balances gauges on task cancellation or panic.
pub(super) struct TranscodingMetrics {
    metrics: Arc<Metrics>,
    counted_session: bool,
    active_session: bool,
    file_routes: i64,
}

impl TranscodingMetrics {
    pub(super) fn new(metrics: Arc<Metrics>) -> Self {
        Self {
            metrics,
            counted_session: false,
            active_session: false,
            file_routes: 0,
        }
    }

    pub(super) fn update(
        &mut self,
        session_id: SessionId,
        endpoints: &HashMap<EndpointId, Endpoint>,
        routing: &RoutingTable,
    ) {
        let mut peer_mismatch = None;
        let mut file_routes = 0;
        for (source_id, source) in endpoints {
            let source_is_peer = matches!(source, Endpoint::Rtp(_) | Endpoint::WebRtc(_));
            let source_is_file = matches!(source, Endpoint::File(_));
            if !source_is_peer && !source_is_file {
                continue;
            }
            let Some(source_codec) = endpoint_audio_codec(source) else {
                continue;
            };
            for destination_id in routing.destinations(source_id).into_iter().flatten() {
                let Some(destination) = endpoints.get(destination_id) else {
                    continue;
                };
                let Some(destination_codec) = endpoint_audio_codec(destination) else {
                    continue;
                };
                if source_codec == destination_codec {
                    continue;
                }
                if source_is_file {
                    file_routes += 1;
                } else if matches!(destination, Endpoint::Rtp(_) | Endpoint::WebRtc(_)) {
                    peer_mismatch =
                        Some((source_id, destination_id, source_codec, destination_codec));
                }
            }
        }

        let active_session = peer_mismatch.is_some();
        if let Some((source_id, destination_id, source_codec, destination_codec)) = peer_mismatch
            && !self.counted_session
        {
            self.counted_session = true;
            self.metrics.transcoding_sessions_total.inc();
            tracing::warn!(
                %session_id,
                source_endpoint_id = %source_id,
                destination_endpoint_id = %destination_id,
                ?source_codec,
                ?destination_codec,
                "session requires peer codec transcoding"
            );
        }
        self.metrics
            .transcoding_sessions_active
            .inc_by(i64::from(active_session) - i64::from(self.active_session));
        self.metrics
            .file_transcodings_active
            .inc_by(file_routes - self.file_routes);
        self.active_session = active_session;
        self.file_routes = file_routes;
    }
}

impl Drop for TranscodingMetrics {
    fn drop(&mut self) {
        self.metrics
            .transcoding_sessions_active
            .dec_by(i64::from(self.active_session));
        self.metrics
            .file_transcodings_active
            .dec_by(self.file_routes);
    }
}
