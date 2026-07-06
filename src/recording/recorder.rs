use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::{Instant, SystemTime};

use tokio::sync::mpsc;
use tracing::{debug, error, warn};

use super::meta::StreamDescriptor;
use super::pcap_writer::{self, RecordPacket};
use crate::control::protocol::{EndpointId, RecordingId};

/// Cached per-endpoint descriptor: the latest `StreamDescriptor`, its encoded
/// bytes, a monotonically-bumped version, and the frame `(src,dst)` it was last
/// noted with (so a recording started mid-call can replay it without a live packet).
struct DescriptorState {
    desc: StreamDescriptor,
    payload: Vec<u8>,
    version: u64,
    src: SocketAddr,
    dst: SocketAddr,
}

/// Info about a recording that was stopped implicitly (not via recording.stop).
pub struct StoppedRecordingInfo {
    pub recording_id: RecordingId,
    pub file_path: String,
    pub duration_ms: u64,
    pub packets: u64,
    pub dropped_packets: u64,
}

/// Manages all recordings for a session
pub struct RecordingManager {
    recordings: HashMap<RecordingId, Recording>,
    /// Maps endpoint_id → list of recording IDs capturing that endpoint
    endpoint_recordings: HashMap<EndpointId, Vec<RecordingId>>,
    /// Synthetic address index counter (saturates at u16::MAX - 1 to avoid collision with bridge marker 0xFFFF)
    next_endpoint_index: u16,
    /// Maps endpoint_id → synthetic PCAP address index
    endpoint_indices: HashMap<EndpointId, u16>,
    /// Recycled indices from removed endpoints, reused before incrementing the counter
    free_indices: Vec<u16>,
    /// Maximum concurrent recordings allowed
    max_recordings: usize,
    /// Seconds to wait for recording tasks to flush before aborting
    flush_timeout_secs: u64,
    /// Channel buffer size for packets between session task and writer task
    channel_size: usize,
    /// Latest codec descriptor per endpoint (for prepending ahead of media and
    /// replaying into newly-started recordings).
    latest_descriptors: HashMap<EndpointId, DescriptorState>,
}

struct Recording {
    pub id: RecordingId,
    pub endpoint_id: Option<EndpointId>, // None = all legs
    pub file_path: String,
    pub started_at: Instant,
    pub packet_count: u64,
    pub dropped_packet_count: u64,
    pub tx: mpsc::Sender<RecordPacket>,
    pub task: tokio::task::JoinHandle<()>,
    /// Highest descriptor version already written into this recording, per
    /// endpoint. Guarantees descriptor-before-media ordering on the channel.
    pub last_written_version: HashMap<EndpointId, u64>,
}

impl Default for RecordingManager {
    fn default() -> Self {
        Self::new()
    }
}

impl RecordingManager {
    pub fn new() -> Self {
        Self::with_max(100)
    }

    pub fn with_max(max_recordings: usize) -> Self {
        Self::with_max_and_timeout(max_recordings, 10)
    }

    pub fn with_max_and_timeout(max_recordings: usize, flush_timeout_secs: u64) -> Self {
        Self::with_config(max_recordings, flush_timeout_secs, 1000)
    }

    pub fn with_config(
        max_recordings: usize,
        flush_timeout_secs: u64,
        channel_size: usize,
    ) -> Self {
        Self {
            recordings: HashMap::new(),
            endpoint_recordings: HashMap::new(),
            next_endpoint_index: 0,
            endpoint_indices: HashMap::new(),
            free_indices: Vec::new(),
            max_recordings,
            flush_timeout_secs,
            channel_size,
            latest_descriptors: HashMap::new(),
        }
    }

    /// Start a new recording. Returns the recording ID. Validates the path and
    /// that the PCAP file can be created before returning success. Recording is
    /// one-directional: it captures what each source *produces* (inbound RTP/RTCP
    /// from real peers, and the RTP that internal generators emit).
    pub async fn start(
        &mut self,
        endpoint_id: Option<EndpointId>,
        file_path: String,
    ) -> anyhow::Result<RecordingId> {
        if self.recordings.len() >= self.max_recordings {
            let max = self.max_recordings;
            anyhow::bail!("Maximum concurrent recordings ({max}) reached");
        }

        // Defense-in-depth: validate path even though the handler also checks.
        let path = std::path::Path::new(&file_path);
        if !path.is_absolute() {
            anyhow::bail!("recording file_path must be absolute");
        }
        for component in path.components() {
            if matches!(component, std::path::Component::ParentDir) {
                anyhow::bail!("recording file_path must not contain '..' components");
            }
        }

        let id = RecordingId::new_v4();

        // Validate file creation upfront so we don't return success
        // for a recording that will silently fail in the background.
        let path = PathBuf::from(&file_path);
        let path_for_open = path.clone();
        let file_path_for_err = file_path.clone();
        let file = tokio::task::spawn_blocking(move || {
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path_for_open)
        })
        .await
        .map_err(|e| anyhow::anyhow!("Failed to create recording file: {e}"))?
        .map_err(|e| anyhow::anyhow!("Cannot create recording file '{file_path_for_err}': {e}"))?;

        let (tx, rx) = mpsc::channel::<RecordPacket>(self.channel_size);
        let task = tokio::spawn(recording_task(rx, file, path));

        let recording = Recording {
            id,
            endpoint_id,
            file_path,
            started_at: Instant::now(),
            packet_count: 0,
            dropped_packet_count: 0,
            tx,
            task,
            last_written_version: HashMap::new(),
        };

        self.recordings.insert(id, recording);

        // Track endpoint → recording mapping
        if let Some(eid) = endpoint_id {
            self.endpoint_recordings.entry(eid).or_default().push(id);
        }

        // Replay cached descriptors so a recording started mid-call is
        // self-describing from byte 0 (and the first media packet won't re-prepend).
        let to_replay: Vec<(EndpointId, u64, Vec<u8>, SocketAddr, SocketAddr)> = self
            .latest_descriptors
            .iter()
            .filter(|(eid, _)| endpoint_id.is_none_or(|e| e == **eid))
            .map(|(eid, d)| (*eid, d.version, d.payload.clone(), d.src, d.dst))
            .collect();
        if let Some(rec) = self.recordings.get_mut(&id) {
            for (eid, version, payload, src, dst) in to_replay {
                let pkt = RecordPacket {
                    src_addr: src,
                    dst_addr: dst,
                    payload,
                    timestamp: SystemTime::now(),
                };
                if rec.tx.try_send(pkt).is_ok() {
                    rec.packet_count += 1;
                    rec.last_written_version.insert(eid, version);
                }
            }
        }

        Ok(id)
    }

    /// Stop a recording. Returns (file_path, duration_ms, packets).
    /// Drops the sender channel and spawns a background task to wait for
    /// the recording task to finish flushing, with a timeout to avoid hanging.
    pub fn stop(&mut self, recording_id: &RecordingId) -> anyhow::Result<(String, u64, u64, u64)> {
        let recording = self
            .recordings
            .remove(recording_id)
            .ok_or_else(|| anyhow::anyhow!("Recording not found"))?;

        // Remove from endpoint tracking
        if let Some(eid) = recording.endpoint_id
            && let Some(recs) = self.endpoint_recordings.get_mut(&eid)
        {
            recs.retain(|id| id != recording_id);
        }

        // Drop sender to signal the task to drain remaining packets and finish.
        // Do NOT abort — let the task flush the PCAP file cleanly.
        drop(recording.tx);

        // Spawn a background waiter that gives the task time to flush.
        // The task now explicitly flushes its BufWriter, so the configured
        // timeout should be ample. If it still hasn't finished, abort as a
        // last resort — but the explicit flush means data loss is unlikely.
        let flush_timeout = std::time::Duration::from_secs(self.flush_timeout_secs);
        tokio::spawn(async move {
            let task = recording.task;
            tokio::pin!(task);
            if tokio::time::timeout(flush_timeout, &mut task)
                .await
                .is_err()
            {
                warn!(
                    timeout_secs = flush_timeout.as_secs(),
                    "recording task did not finish within flush timeout; \
                     waiting up to 2x for hard abort"
                );
                if tokio::time::timeout(flush_timeout, &mut task)
                    .await
                    .is_err()
                {
                    warn!(
                        timeout_secs = flush_timeout.as_secs() * 2,
                        "recording task exceeded hard abort deadline; aborting"
                    );
                    task.abort();
                }
            }
        });

        let duration_ms = recording.started_at.elapsed().as_millis() as u64;
        Ok((
            recording.file_path,
            duration_ms,
            recording.packet_count,
            recording.dropped_packet_count,
        ))
    }

    /// Returns true if there are any active recordings.
    pub fn is_recording(&self) -> bool {
        !self.recordings.is_empty()
    }

    /// Test-only convenience: record with synthetic per-endpoint addresses (no
    /// real socket). The media path uses [`Self::record_packet_addr`] so the PCAP
    /// carries the real remote IP:port.
    #[cfg(test)]
    pub fn record_packet(
        &mut self,
        endpoint_id: &EndpointId,
        payload: &[u8],
    ) -> Vec<StoppedRecordingInfo> {
        self.record_packet_addr(endpoint_id, payload, None, None)
    }

    /// Frame an endpoint's packet as `source -> us`: `(src = remote, dst = local)`
    /// with the real socket addresses when both are known, else synthetic
    /// per-endpoint `10.x` markers (`src = endpoint marker`, `dst = bridge marker`).
    /// Descriptor packets and media MUST share this helper so they carry identical
    /// `(src,dst)` and the decoder binds them to one channel.
    fn frame(
        &mut self,
        endpoint_id: &EndpointId,
        local: Option<SocketAddr>,
        remote: Option<SocketAddr>,
    ) -> (SocketAddr, SocketAddr) {
        match (local, remote) {
            (Some(local), Some(remote)) => (remote, local),
            _ => {
                let ep = pcap_writer::synthetic_addr(self.get_endpoint_index(endpoint_id));
                let bridge = pcap_writer::synthetic_addr(0xFFFF);
                (ep, bridge)
            }
        }
    }

    /// Cache/update the codec descriptor for an endpoint. The descriptor is framed
    /// with the same `frame()` helper as media, so it carries identical `(src,dst)`.
    /// Bumps the version only when the descriptor content actually changes (codec or
    /// address) — a no-op otherwise, so callers may over-call cheaply.
    pub fn note_descriptor(
        &mut self,
        endpoint_id: &EndpointId,
        desc: &StreamDescriptor,
        local: Option<SocketAddr>,
        remote: Option<SocketAddr>,
    ) {
        if let Some(existing) = self.latest_descriptors.get(endpoint_id)
            && &existing.desc == desc
        {
            return; // unchanged
        }
        let (src, dst) = self.frame(endpoint_id, local, remote);
        let version = self
            .latest_descriptors
            .get(endpoint_id)
            .map(|d| d.version + 1)
            .unwrap_or(1);
        let payload = desc.encode();
        self.latest_descriptors.insert(
            *endpoint_id,
            DescriptorState {
                desc: desc.clone(),
                payload,
                version,
                src,
                dst,
            },
        );
    }

    /// Forget an endpoint's cached descriptor (on endpoint teardown) so a later
    /// recording doesn't replay a dead endpoint.
    pub fn forget_endpoint(&mut self, endpoint_id: &EndpointId) {
        self.latest_descriptors.remove(endpoint_id);
    }

    /// Record an audio RTP packet for the endpoint. Prepends the endpoint's current
    /// codec descriptor into any recording that hasn't seen this version yet, so the
    /// decoder always sees the descriptor before the media it describes.
    pub fn record_packet_addr(
        &mut self,
        endpoint_id: &EndpointId,
        payload: &[u8],
        local: Option<SocketAddr>,
        remote: Option<SocketAddr>,
    ) -> Vec<StoppedRecordingInfo> {
        let (src, dst) = self.frame(endpoint_id, local, remote);
        self.deliver(endpoint_id, src, dst, payload, true)
    }

    /// Record an auxiliary packet (e.g. RTCP) for the endpoint WITHOUT a descriptor.
    /// RTCP is framed on its own port (a different `(src,dst)` than the media), and
    /// the decoder skips it, so it must not carry/advance the media descriptor.
    pub fn record_rtcp(
        &mut self,
        endpoint_id: &EndpointId,
        payload: &[u8],
        local: Option<SocketAddr>,
        remote: Option<SocketAddr>,
    ) -> Vec<StoppedRecordingInfo> {
        let (src, dst) = self.frame(endpoint_id, local, remote);
        self.deliver(endpoint_id, src, dst, payload, false)
    }

    /// Deliver one packet (framed `src -> dst`) to every recording covering the
    /// endpoint (endpoint-specific + session-wide). When `with_descriptor` and the
    /// cached descriptor version is ahead of what a recording has seen, the
    /// descriptor is enqueued FIRST; if that enqueue is dropped (channel full), the
    /// media is dropped too and the version is not advanced — so the decoder never
    /// sees media before its descriptor.
    fn deliver(
        &mut self,
        endpoint_id: &EndpointId,
        src: SocketAddr,
        dst: SocketAddr,
        payload: &[u8],
        with_descriptor: bool,
    ) -> Vec<StoppedRecordingInfo> {
        if self.recordings.is_empty() {
            return Vec::new();
        }

        let desc = if with_descriptor {
            self.latest_descriptors
                .get(endpoint_id)
                .map(|d| (d.version, d.payload.clone()))
        } else {
            None
        };
        let now = SystemTime::now();
        let media = RecordPacket {
            src_addr: src,
            dst_addr: dst,
            payload: payload.to_vec(),
            timestamp: now,
        };

        // Target recordings: endpoint-specific + session-wide. A session-wide
        // recording has `endpoint_id = None` so it is never in `endpoint_recordings`,
        // hence no duplicates.
        let mut targets: Vec<RecordingId> = Vec::new();
        if let Some(ids) = self.endpoint_recordings.get(endpoint_id) {
            targets.extend(ids.iter().copied());
        }
        for (id, rec) in self.recordings.iter() {
            if rec.endpoint_id.is_none() {
                targets.push(*id);
            }
        }

        let mut dead_recordings = Vec::new();
        for rec_id in targets {
            let Some(rec) = self.recordings.get_mut(&rec_id) else {
                continue;
            };

            // Prepend the descriptor if this recording is behind the cached version.
            if let Some((version, ref dpayload)) = desc {
                let seen = rec
                    .last_written_version
                    .get(endpoint_id)
                    .copied()
                    .unwrap_or(0);
                if version > seen {
                    let dpkt = RecordPacket {
                        src_addr: src,
                        dst_addr: dst,
                        payload: dpayload.clone(),
                        timestamp: now,
                    };
                    match rec.tx.try_send(dpkt) {
                        Ok(()) => {
                            rec.packet_count += 1;
                            rec.last_written_version.insert(*endpoint_id, version);
                        }
                        Err(mpsc::error::TrySendError::Closed(_)) => {
                            dead_recordings.push(rec_id);
                            continue;
                        }
                        Err(mpsc::error::TrySendError::Full(_)) => {
                            // Drop the media too so it never precedes its descriptor;
                            // both are retried on the next packet.
                            rec.dropped_packet_count += 1;
                            warn!(recording_id = %rec_id, "recording channel full, dropping descriptor+media");
                            continue;
                        }
                    }
                }
            }

            match rec.tx.try_send(media.clone_packet()) {
                Ok(()) => rec.packet_count += 1,
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    dead_recordings.push(rec_id);
                }
                Err(mpsc::error::TrySendError::Full(_)) => {
                    rec.dropped_packet_count += 1;
                    warn!(recording_id = %rec_id, "recording channel full, dropping packet");
                }
            }
        }

        // Clean up dead recordings and collect info for event emission
        let mut stopped = Vec::new();
        for rec_id in dead_recordings {
            if let Some(recording) = self.recordings.remove(&rec_id) {
                warn!(recording_id = %rec_id, path = %recording.file_path, "recording task died (write error), removing");
                if let Some(eid) = recording.endpoint_id
                    && let Some(recs) = self.endpoint_recordings.get_mut(&eid)
                {
                    recs.retain(|id| *id != rec_id);
                }
                stopped.push(StoppedRecordingInfo {
                    recording_id: rec_id,
                    file_path: recording.file_path,
                    duration_ms: recording.started_at.elapsed().as_millis() as u64,
                    packets: recording.packet_count,
                    dropped_packets: recording.dropped_packet_count,
                });
            }
        }
        stopped
    }

    /// Stop all recordings targeting a specific endpoint.
    /// Called when an endpoint is removed to avoid recording nothing.
    /// Returns info about each stopped recording for event emission.
    pub fn stop_endpoint_recordings(
        &mut self,
        endpoint_id: &EndpointId,
    ) -> Vec<StoppedRecordingInfo> {
        let mut stopped = Vec::new();
        let flush_timeout = std::time::Duration::from_secs(self.flush_timeout_secs);
        if let Some(rec_ids) = self.endpoint_recordings.remove(endpoint_id) {
            for rec_id in rec_ids {
                if let Some(recording) = self.recordings.remove(&rec_id) {
                    let info = StoppedRecordingInfo {
                        recording_id: rec_id,
                        file_path: recording.file_path.clone(),
                        duration_ms: recording.started_at.elapsed().as_millis() as u64,
                        packets: recording.packet_count,
                        dropped_packets: recording.dropped_packet_count,
                    };
                    // Drop sender to signal the task to drain and finish
                    drop(recording.tx);
                    // Spawn a waiter to observe slow flushes (same pattern as stop())
                    let task = recording.task;
                    tokio::spawn(async move {
                        tokio::pin!(task);
                        if tokio::time::timeout(flush_timeout, &mut task)
                            .await
                            .is_err()
                        {
                            warn!(
                                timeout_secs = flush_timeout.as_secs(),
                                "recording task did not finish within flush timeout; \
                                 waiting up to 2x for hard abort"
                            );
                            if tokio::time::timeout(flush_timeout, &mut task)
                                .await
                                .is_err()
                            {
                                warn!(
                                    timeout_secs = flush_timeout.as_secs() * 2,
                                    "recording task exceeded hard abort deadline; aborting"
                                );
                                task.abort();
                            }
                        }
                    });
                    stopped.push(info);
                }
            }
        }
        // Recycle the synthetic PCAP address index for this endpoint
        if let Some(idx) = self.endpoint_indices.remove(endpoint_id) {
            self.free_indices.push(idx);
        }
        stopped
    }

    fn get_endpoint_index(&mut self, endpoint_id: &EndpointId) -> u16 {
        *self
            .endpoint_indices
            .entry(*endpoint_id)
            .or_insert_with(|| {
                // Reuse a recycled index if available
                if let Some(idx) = self.free_indices.pop() {
                    return idx;
                }
                let idx = self.next_endpoint_index;
                if idx >= 0xFFFE {
                    tracing::warn!("Recording endpoint index saturated — new endpoints will share PCAP addresses");
                }
                // Saturate at 0xFFFE to avoid colliding with the 0xFFFF bridge marker
                self.next_endpoint_index = self.next_endpoint_index.saturating_add(1).min(0xFFFE);
                idx
            })
    }

    /// Stop all recordings. Returns info about each stopped recording for event emission.
    pub fn stop_all(&mut self) -> Vec<StoppedRecordingInfo> {
        let mut stopped = Vec::new();
        let ids: Vec<_> = self.recordings.keys().cloned().collect();
        for id in ids {
            if let Ok((file_path, duration_ms, packets, dropped_packets)) = self.stop(&id) {
                stopped.push(StoppedRecordingInfo {
                    recording_id: id,
                    file_path,
                    duration_ms,
                    packets,
                    dropped_packets,
                });
            }
        }
        stopped
    }

    pub fn active_recordings(&self) -> Vec<crate::control::protocol::RecordingInfo> {
        self.recordings
            .values()
            .map(|r| crate::control::protocol::RecordingInfo {
                recording_id: r.id,
                endpoint_id: r.endpoint_id,
                file_path: r.file_path.clone(),
                state: crate::control::protocol::RecordingState::Active,
            })
            .collect()
    }
}

impl RecordPacket {
    fn clone_packet(&self) -> RecordPacket {
        RecordPacket {
            src_addr: self.src_addr,
            dst_addr: self.dst_addr,
            payload: self.payload.clone(),
            timestamp: self.timestamp,
        }
    }
}

/// Background task that writes packets to a PCAP file.
/// Note: PCAP writes go through BufWriter, so individual writes are fast (in-memory).
/// The buffered data is flushed on channel close, which is the only potentially slow I/O.
async fn recording_task(mut rx: mpsc::Receiver<RecordPacket>, file: std::fs::File, path: PathBuf) {
    let mut writer = match pcap_writer::create_pcap_writer(std::io::BufWriter::new(file)) {
        Ok(w) => w,
        Err(e) => {
            warn!(path = %path.display(), error = %e, "failed to initialize PCAP writer");
            return;
        }
    };

    while let Some(pkt) = rx.recv().await {
        if let Err(e) = pcap_writer::write_record_packet(&mut writer, &pkt) {
            error!(path = %path.display(), error = %e, "PCAP write failed, stopping recording — packets may be lost");
            break;
        }
    }

    // Explicitly flush the BufWriter so data isn't lost if we're being
    // shut down under a timeout (Drop doesn't propagate flush errors).
    use std::io::Write;
    let mut buf_writer = writer.into_writer();
    if let Err(e) = buf_writer.flush() {
        error!(path = %path.display(), error = %e, "failed to flush PCAP file — recording may be incomplete");
    }

    debug!(path = %path.display(), "recording task finished");
}

#[cfg(test)]
#[path = "recorder_tests.rs"]
mod tests;
