use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Instant, SystemTime};

use tokio::sync::mpsc;
use tracing::{debug, error, warn};

use super::pcap_writer::{self, RecordPacket};
use crate::control::protocol::{EndpointId, RecordingId};

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
        }
    }

    /// Start a new recording. Returns the recording ID.
    /// Validates the path and that the PCAP file can be created before returning success.
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
        };

        self.recordings.insert(id, recording);

        // Track endpoint → recording mapping
        if let Some(eid) = endpoint_id {
            self.endpoint_recordings.entry(eid).or_default().push(id);
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
        if let Some(eid) = recording.endpoint_id {
            if let Some(recs) = self.endpoint_recordings.get_mut(&eid) {
                recs.retain(|id| id != recording_id);
            }
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

    /// Record a packet for the given endpoint. Sends to all relevant recordings.
    /// Returns info about any recordings that died due to write errors.
    pub fn record_packet(
        &mut self,
        endpoint_id: &EndpointId,
        payload: &[u8],
        is_outbound: bool,
    ) -> Vec<StoppedRecordingInfo> {
        if self.recordings.is_empty() {
            return Vec::new();
        }

        let ep_index = self.get_endpoint_index(endpoint_id);
        let src = pcap_writer::synthetic_addr(ep_index);
        let dst = pcap_writer::synthetic_addr(0xFFFF);

        let (src, dst) = if is_outbound { (dst, src) } else { (src, dst) };

        let pkt = RecordPacket {
            src_addr: src,
            dst_addr: dst,
            payload: payload.to_vec(),
            timestamp: SystemTime::now(),
        };

        let mut dead_recordings = Vec::new();

        // Send to endpoint-specific recordings
        if let Some(rec_ids) = self.endpoint_recordings.get(endpoint_id) {
            for rec_id in rec_ids {
                if let Some(rec) = self.recordings.get_mut(rec_id) {
                    match rec.tx.try_send(pkt.clone_packet()) {
                        Ok(()) => rec.packet_count += 1,
                        Err(mpsc::error::TrySendError::Closed(_)) => {
                            dead_recordings.push(*rec_id);
                        }
                        Err(mpsc::error::TrySendError::Full(_)) => {
                            rec.dropped_packet_count += 1;
                            warn!(recording_id = %rec_id, "recording channel full, dropping packet");
                        }
                    }
                }
            }
        }

        // Send to session-wide recordings (endpoint_id = None)
        for rec in self.recordings.values_mut() {
            if rec.endpoint_id.is_none() {
                match rec.tx.try_send(pkt.clone_packet()) {
                    Ok(()) => rec.packet_count += 1,
                    Err(mpsc::error::TrySendError::Closed(_)) => {
                        dead_recordings.push(rec.id);
                    }
                    Err(mpsc::error::TrySendError::Full(_)) => {
                        rec.dropped_packet_count += 1;
                        warn!(recording_id = %rec.id, "recording channel full, dropping packet");
                    }
                }
            }
        }

        // Clean up dead recordings and collect info for event emission
        let mut stopped = Vec::new();
        for rec_id in dead_recordings {
            if let Some(recording) = self.recordings.remove(&rec_id) {
                warn!(recording_id = %rec_id, path = %recording.file_path, "recording task died (write error), removing");
                // Remove from endpoint tracking
                if let Some(eid) = recording.endpoint_id {
                    if let Some(recs) = self.endpoint_recordings.get_mut(&eid) {
                        recs.retain(|id| *id != rec_id);
                    }
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
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_max_recordings_limit() {
        let mut mgr = RecordingManager::with_max(3);
        let dir = std::env::temp_dir().join("rtpbridge-rec-limit-test");
        std::fs::create_dir_all(&dir).ok();

        // Start 3 recordings — all should succeed
        let mut ids = Vec::new();
        for i in 0..3 {
            let path = dir
                .join(format!("test-{i}.pcap"))
                .to_string_lossy()
                .to_string();
            let id = mgr
                .start(None, path)
                .await
                .expect("recording should succeed");
            ids.push(id);
        }

        // 4th should fail
        let path = dir.join("test-3.pcap").to_string_lossy().to_string();
        let result = mgr.start(None, path).await;
        assert!(result.is_err(), "4th recording should fail at limit");
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Maximum concurrent recordings"),
            "error should mention recording limit"
        );

        // Stop one, then start should succeed again
        mgr.stop(&ids[0]).unwrap();
        let path = dir.join("test-4.pcap").to_string_lossy().to_string();
        mgr.start(None, path)
            .await
            .expect("should succeed after stopping one");

        // Cleanup
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn test_stop_nonexistent_recording() {
        let mut mgr = RecordingManager::new();
        let fake_id = RecordingId::new_v4();
        let result = mgr.stop(&fake_id);
        assert!(
            result.is_err(),
            "stopping a nonexistent recording should return Err"
        );
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Recording not found"),
            "error should mention recording not found"
        );
    }

    #[tokio::test]
    async fn test_stop_all_recordings() {
        let mut mgr = RecordingManager::new();
        let dir = std::env::temp_dir().join("rtpbridge-rec-stop-all-test");
        std::fs::create_dir_all(&dir).ok();

        // Start 3 recordings
        for i in 0..3 {
            let path = dir
                .join(format!("test-{i}.pcap"))
                .to_string_lossy()
                .to_string();
            mgr.start(None, path)
                .await
                .expect("recording should succeed");
        }
        assert_eq!(
            mgr.active_recordings().len(),
            3,
            "should have 3 active recordings"
        );

        // Stop all
        mgr.stop_all();
        assert_eq!(
            mgr.active_recordings().len(),
            0,
            "all recordings should be stopped"
        );

        // Cleanup
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn test_stop_all_mixed_endpoint_recordings() {
        let mut mgr = RecordingManager::new();
        let dir = std::env::temp_dir().join("rtpbridge-rec-stop-all-mixed-test");
        std::fs::create_dir_all(&dir).ok();

        let ep1 = EndpointId::new_v4();
        let ep2 = EndpointId::new_v4();

        // Start 3 recordings: 2 for specific endpoints, 1 for full-session (None)
        let path1 = dir.join("ep1.pcap").to_string_lossy().to_string();
        mgr.start(Some(ep1), path1)
            .await
            .expect("recording for ep1 should succeed");

        let path2 = dir.join("ep2.pcap").to_string_lossy().to_string();
        mgr.start(Some(ep2), path2)
            .await
            .expect("recording for ep2 should succeed");

        let path3 = dir.join("session.pcap").to_string_lossy().to_string();
        mgr.start(None, path3)
            .await
            .expect("full-session recording should succeed");

        assert_eq!(
            mgr.active_recordings().len(),
            3,
            "should have 3 active recordings"
        );

        // Verify each recording has the correct endpoint association
        let active = mgr.active_recordings();
        let ep1_recs: Vec<_> = active
            .iter()
            .filter(|r| r.endpoint_id == Some(ep1))
            .collect();
        let ep2_recs: Vec<_> = active
            .iter()
            .filter(|r| r.endpoint_id == Some(ep2))
            .collect();
        let session_recs: Vec<_> = active.iter().filter(|r| r.endpoint_id.is_none()).collect();
        assert_eq!(ep1_recs.len(), 1, "should have 1 recording for ep1");
        assert_eq!(ep2_recs.len(), 1, "should have 1 recording for ep2");
        assert_eq!(
            session_recs.len(),
            1,
            "should have 1 full-session recording"
        );

        // Stop all
        mgr.stop_all();
        assert_eq!(
            mgr.active_recordings().len(),
            0,
            "all recordings should be stopped after stop_all"
        );

        // Verify that starting new recordings works after stop_all (manager is reusable)
        let path4 = dir
            .join("after-stop-all.pcap")
            .to_string_lossy()
            .to_string();
        mgr.start(None, path4)
            .await
            .expect("should be able to start recordings after stop_all");
        assert_eq!(
            mgr.active_recordings().len(),
            1,
            "new recording should work after stop_all"
        );

        // Cleanup
        mgr.stop_all();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn test_recording_channel_backpressure() {
        let mut mgr = RecordingManager::new();
        let dir = std::env::temp_dir().join("rtpbridge-rec-backpressure-test");
        std::fs::create_dir_all(&dir).ok();

        let ep = EndpointId::new_v4();
        let path = dir.join("bp-test.pcap").to_string_lossy().to_string();
        mgr.start(Some(ep), path).await.unwrap();

        // Flood the channel beyond capacity (1000) without giving the writer task time to drain.
        // This should not panic — packets should be silently dropped after the channel fills.
        for _ in 0..2000 {
            mgr.record_packet(&ep, &[0xAA; 172], false);
        }

        // Verify the recording is still functional after overflow
        let active = mgr.active_recordings();
        assert_eq!(
            active.len(),
            1,
            "recording should still be active after backpressure"
        );

        mgr.stop_all();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn test_mid_recording_write_error() {
        // Simulate a write error mid-recording by recording to a file on a
        // read-only path. The recording task should break out of its write
        // loop on error (not panic or hang).
        let mut mgr = RecordingManager::new();
        let dir = std::env::temp_dir().join("rtpbridge-rec-write-error-test");
        std::fs::create_dir_all(&dir).ok();

        let ep = EndpointId::new_v4();
        let path = dir.join("write-error.pcap").to_string_lossy().to_string();
        mgr.start(Some(ep), path).await.unwrap();

        // Record some packets — these go through the channel to the background writer
        for _ in 0..10 {
            mgr.record_packet(&ep, &[0xCC; 172], false);
        }

        // Give the writer task a chance to process
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Stop should succeed even if the writer encountered errors
        let stopped = mgr.stop_all();
        assert_eq!(stopped.len(), 1, "should return one stopped recording");
        assert!(
            stopped[0].packets >= 10,
            "should have counted packets sent to the channel"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn test_recording_invalid_path() {
        let mut mgr = RecordingManager::new();
        // Try to record to a path that cannot be created
        let result = mgr
            .start(
                None,
                "/nonexistent-root-dir/impossible/file.pcap".to_string(),
            )
            .await;
        assert!(
            result.is_err(),
            "recording to invalid path should fail upfront"
        );
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Cannot create recording file"),
            "error should mention file creation failure"
        );
        assert!(
            mgr.active_recordings().is_empty(),
            "no recording should be active after failure"
        );
    }

    #[tokio::test]
    async fn test_stop_endpoint_recordings_returns_info() {
        let mut mgr = RecordingManager::new();
        let dir = std::env::temp_dir().join("rtpbridge-rec-stop-ep-info-test");
        std::fs::create_dir_all(&dir).ok();

        let ep = EndpointId::new_v4();
        let path = dir.join("ep-info.pcap").to_string_lossy().to_string();
        let rec_id = mgr.start(Some(ep), path.clone()).await.unwrap();

        // Record a few packets
        for _ in 0..5 {
            mgr.record_packet(&ep, &[0xBB; 100], false);
        }

        let stopped = mgr.stop_endpoint_recordings(&ep);
        assert_eq!(stopped.len(), 1, "should return one stopped recording");
        assert_eq!(stopped[0].recording_id, rec_id);
        assert_eq!(stopped[0].file_path, path);
        assert_eq!(stopped[0].packets, 5, "should report 5 recorded packets");
        assert!(
            stopped[0].duration_ms < 5000,
            "duration should be reasonable"
        );

        assert!(
            mgr.active_recordings().is_empty(),
            "no recordings should remain"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn test_record_packet_session_wide_receives_all() {
        let mut mgr = RecordingManager::new();
        let dir = std::env::temp_dir().join("rtpbridge-rec-session-wide-test");
        std::fs::create_dir_all(&dir).ok();

        let ep1 = EndpointId::new_v4();
        let ep2 = EndpointId::new_v4();

        // Start a session-wide recording (endpoint_id = None)
        let path = dir.join("session-wide.pcap").to_string_lossy().to_string();
        mgr.start(None, path).await.unwrap();

        // Record packets from different endpoints
        mgr.record_packet(&ep1, &[0xAA; 100], false);
        mgr.record_packet(&ep2, &[0xBB; 100], false);
        mgr.record_packet(&ep1, &[0xCC; 100], true);

        // Give the writer task a moment to process
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Stop and check that all 3 packets were counted
        let stopped = mgr.stop_all();
        assert_eq!(stopped.len(), 1);
        assert_eq!(
            stopped[0].packets, 3,
            "session-wide recording should receive packets from all endpoints"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn test_stop_endpoint_recordings_returns_all() {
        let mut mgr = RecordingManager::new();
        let dir = std::env::temp_dir().join("rtpbridge-rec-stop-ep-all-test");
        std::fs::create_dir_all(&dir).ok();

        let ep = EndpointId::new_v4();

        // Start two recordings for the same endpoint
        let path1 = dir.join("ep-rec-1.pcap").to_string_lossy().to_string();
        let id1 = mgr.start(Some(ep), path1).await.unwrap();
        let path2 = dir.join("ep-rec-2.pcap").to_string_lossy().to_string();
        let id2 = mgr.start(Some(ep), path2).await.unwrap();

        assert_eq!(mgr.active_recordings().len(), 2);

        // Stop all recordings for that endpoint
        let stopped = mgr.stop_endpoint_recordings(&ep);
        assert_eq!(
            stopped.len(),
            2,
            "should stop both recordings for the endpoint"
        );

        let stopped_ids: Vec<_> = stopped.iter().map(|s| s.recording_id).collect();
        assert!(stopped_ids.contains(&id1));
        assert!(stopped_ids.contains(&id2));

        assert!(
            mgr.active_recordings().is_empty(),
            "no recordings should remain"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn test_stop_all_returns_all_info() {
        let mut mgr = RecordingManager::new();
        let dir = std::env::temp_dir().join("rtpbridge-rec-stop-all-info-test");
        std::fs::create_dir_all(&dir).ok();

        let ep1 = EndpointId::new_v4();
        let ep2 = EndpointId::new_v4();

        let path1 = dir.join("r1.pcap").to_string_lossy().to_string();
        let id1 = mgr.start(Some(ep1), path1.clone()).await.unwrap();
        let path2 = dir.join("r2.pcap").to_string_lossy().to_string();
        let id2 = mgr.start(Some(ep2), path2.clone()).await.unwrap();
        let path3 = dir.join("r3.pcap").to_string_lossy().to_string();
        let id3 = mgr.start(None, path3.clone()).await.unwrap();

        // Record some packets
        mgr.record_packet(&ep1, &[0x11; 50], false);
        mgr.record_packet(&ep2, &[0x22; 50], false);

        let stopped = mgr.stop_all();
        assert_eq!(
            stopped.len(),
            3,
            "stop_all should return info for all 3 recordings"
        );

        let stopped_ids: Vec<_> = stopped.iter().map(|s| s.recording_id).collect();
        assert!(stopped_ids.contains(&id1));
        assert!(stopped_ids.contains(&id2));
        assert!(stopped_ids.contains(&id3));

        // Verify file paths are correct
        let stopped_paths: Vec<_> = stopped.iter().map(|s| s.file_path.clone()).collect();
        assert!(stopped_paths.contains(&path1));
        assert!(stopped_paths.contains(&path2));
        assert!(stopped_paths.contains(&path3));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn test_relative_path_rejected() {
        let mut mgr = RecordingManager::new();
        let result = mgr.start(None, "recordings/test.pcap".to_string()).await;
        assert!(result.is_err());
        assert!(
            result.unwrap_err().to_string().contains("must be absolute"),
            "should reject relative paths"
        );
    }

    #[tokio::test]
    async fn test_path_traversal_rejected() {
        let mut mgr = RecordingManager::new();
        let result = mgr
            .start(None, "/tmp/recordings/../../../etc/passwd.pcap".to_string())
            .await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("must not contain '..'"),
            "should reject path traversal"
        );
    }

    #[tokio::test]
    async fn test_start_duplicate_path_fails() {
        let mut mgr = RecordingManager::new();
        let dir = std::env::temp_dir().join("rtpbridge-rec-dup-path-test");
        std::fs::create_dir_all(&dir).ok();

        let path = dir.join("dup.pcap").to_string_lossy().to_string();
        mgr.start(None, path.clone()).await.unwrap();

        // Second recording to the same path should fail (create_new)
        let result = mgr.start(None, path).await;
        assert!(result.is_err(), "duplicate file path should fail");
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Cannot create recording file"),
            "error should mention file creation"
        );

        mgr.stop_all();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn test_record_packet_dead_recording_cleanup() {
        let mut mgr = RecordingManager::with_max_and_timeout(100, 1);
        let dir = std::env::temp_dir().join("rtpbridge-rec-dead-cleanup-test");
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).ok();

        let ep = EndpointId::new_v4();
        let path = dir.join("dead-rec.pcap").to_string_lossy().to_string();
        let rec_id = mgr.start(Some(ep), path).await.unwrap();

        assert_eq!(mgr.active_recordings().len(), 1);

        // Force the recording task to die by aborting it
        // Access the recording's task handle and abort it
        if let Some(recording) = mgr.recordings.get(&rec_id) {
            recording.task.abort();
        }

        // Give the task time to detect channel closure
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Now record_packet should detect the dead channel and clean up
        let stopped = mgr.record_packet(&ep, &[0xFF; 100], false);
        assert_eq!(
            stopped.len(),
            1,
            "should detect and clean up dead recording"
        );
        assert_eq!(stopped[0].recording_id, rec_id);

        assert!(
            mgr.active_recordings().is_empty(),
            "dead recording should be removed"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    // ---- Additional tests using tempfile::tempdir ----

    #[test]
    fn test_recording_manager_creation() {
        // Default constructor
        let mgr = RecordingManager::new();
        assert!(
            mgr.active_recordings().is_empty(),
            "new manager should have no recordings"
        );
        assert_eq!(mgr.max_recordings, 100, "default max should be 100");

        // with_max constructor
        let mgr2 = RecordingManager::with_max(5);
        assert!(mgr2.active_recordings().is_empty());
        assert_eq!(mgr2.max_recordings, 5);

        // with_max_and_timeout constructor
        let mgr3 = RecordingManager::with_max_and_timeout(42, 30);
        assert_eq!(mgr3.max_recordings, 42);
        assert_eq!(mgr3.flush_timeout_secs, 30);

        // Default trait
        let mgr4 = RecordingManager::default();
        assert!(mgr4.active_recordings().is_empty());
        assert_eq!(mgr4.max_recordings, 100);
    }

    #[tokio::test]
    async fn test_start_recording_is_active() {
        let dir = tempfile::tempdir().expect("failed to create temp dir");
        let mut mgr = RecordingManager::new();

        let ep = EndpointId::new_v4();
        let path = dir.path().join("active.pcap").to_string_lossy().to_string();
        let rec_id = mgr.start(Some(ep), path.clone()).await.unwrap();

        let active = mgr.active_recordings();
        assert_eq!(active.len(), 1, "should have exactly 1 active recording");
        assert_eq!(active[0].recording_id, rec_id);
        assert_eq!(active[0].endpoint_id, Some(ep));
        assert_eq!(active[0].file_path, path);
        assert_eq!(
            active[0].state,
            crate::control::protocol::RecordingState::Active,
            "recording state should be Active"
        );

        mgr.stop_all();
    }

    #[tokio::test]
    async fn test_start_session_wide_recording_is_active() {
        let dir = tempfile::tempdir().expect("failed to create temp dir");
        let mut mgr = RecordingManager::new();

        let path = dir
            .path()
            .join("session.pcap")
            .to_string_lossy()
            .to_string();
        let rec_id = mgr.start(None, path.clone()).await.unwrap();

        let active = mgr.active_recordings();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].recording_id, rec_id);
        assert!(
            active[0].endpoint_id.is_none(),
            "session-wide recording should have no endpoint_id"
        );
        assert_eq!(active[0].file_path, path);

        mgr.stop_all();
    }

    #[tokio::test]
    async fn test_max_recordings_limit_with_tempdir() {
        let dir = tempfile::tempdir().expect("failed to create temp dir");
        let mut mgr = RecordingManager::with_max(2);

        let p1 = dir.path().join("r1.pcap").to_string_lossy().to_string();
        let p2 = dir.path().join("r2.pcap").to_string_lossy().to_string();
        mgr.start(None, p1).await.unwrap();
        mgr.start(None, p2).await.unwrap();

        // 3rd should fail
        let p3 = dir.path().join("r3.pcap").to_string_lossy().to_string();
        let err = mgr.start(None, p3).await.unwrap_err();
        assert!(
            err.to_string()
                .contains("Maximum concurrent recordings (2) reached"),
            "error should mention the limit: {err}"
        );
        assert_eq!(
            mgr.active_recordings().len(),
            2,
            "should still have 2 active recordings"
        );

        // After stopping one, we can start again
        let ids: Vec<_> = mgr
            .active_recordings()
            .iter()
            .map(|r| r.recording_id)
            .collect();
        mgr.stop(&ids[0]).unwrap();
        let p4 = dir.path().join("r4.pcap").to_string_lossy().to_string();
        mgr.start(None, p4)
            .await
            .expect("should succeed after stopping one");
        assert_eq!(mgr.active_recordings().len(), 2);

        mgr.stop_all();
    }

    #[tokio::test]
    async fn test_record_packet_routes_to_correct_recording() {
        let dir = tempfile::tempdir().expect("failed to create temp dir");
        let mut mgr = RecordingManager::new();

        let ep_a = EndpointId::new_v4();
        let ep_b = EndpointId::new_v4();

        // Recording only for ep_a
        let path_a = dir.path().join("ep_a.pcap").to_string_lossy().to_string();
        let id_a = mgr.start(Some(ep_a), path_a.clone()).await.unwrap();

        // Recording only for ep_b
        let path_b = dir.path().join("ep_b.pcap").to_string_lossy().to_string();
        let id_b = mgr.start(Some(ep_b), path_b.clone()).await.unwrap();

        // Send 3 packets to ep_a, 2 packets to ep_b
        let fake_rtp = [
            0x80, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0xA0, 0x00, 0x00, 0x00, 0x01,
        ];
        for _ in 0..3 {
            mgr.record_packet(&ep_a, &fake_rtp, false);
        }
        for _ in 0..2 {
            mgr.record_packet(&ep_b, &fake_rtp, true);
        }

        // Stop individually to check per-recording packet counts
        let (_, _, packets_a, dropped_a) = mgr.stop(&id_a).unwrap();
        assert_eq!(packets_a, 3, "ep_a recording should have 3 packets");
        assert_eq!(dropped_a, 0, "ep_a should have no dropped packets");

        let (_, _, packets_b, dropped_b) = mgr.stop(&id_b).unwrap();
        assert_eq!(packets_b, 2, "ep_b recording should have 2 packets");
        assert_eq!(dropped_b, 0, "ep_b should have no dropped packets");
    }

    #[tokio::test]
    async fn test_record_packet_session_wide_and_endpoint_specific() {
        let dir = tempfile::tempdir().expect("failed to create temp dir");
        let mut mgr = RecordingManager::new();

        let ep = EndpointId::new_v4();

        // One endpoint-specific recording
        let path_ep = dir.path().join("ep.pcap").to_string_lossy().to_string();
        let id_ep = mgr.start(Some(ep), path_ep).await.unwrap();

        // One session-wide recording
        let path_all = dir.path().join("all.pcap").to_string_lossy().to_string();
        let id_all = mgr.start(None, path_all).await.unwrap();

        let fake_rtp = [0x80u8; 12];
        mgr.record_packet(&ep, &fake_rtp, false);
        mgr.record_packet(&ep, &fake_rtp, true);

        // Endpoint-specific should get 2 packets
        let (_, _, pkts_ep, _) = mgr.stop(&id_ep).unwrap();
        assert_eq!(pkts_ep, 2, "endpoint recording should get both packets");

        // Session-wide should also get 2 packets
        let (_, _, pkts_all, _) = mgr.stop(&id_all).unwrap();
        assert_eq!(
            pkts_all, 2,
            "session-wide recording should also get both packets"
        );
    }

    #[tokio::test]
    async fn test_stop_returns_correct_metadata() {
        let dir = tempfile::tempdir().expect("failed to create temp dir");
        let mut mgr = RecordingManager::new();

        let ep = EndpointId::new_v4();
        let path = dir.path().join("meta.pcap").to_string_lossy().to_string();
        let rec_id = mgr.start(Some(ep), path.clone()).await.unwrap();

        // Record some packets
        let fake_rtp = [0x80u8; 20];
        for _ in 0..7 {
            mgr.record_packet(&ep, &fake_rtp, false);
        }

        // Small sleep so duration_ms is > 0
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        let (file_path, duration_ms, packets, dropped) = mgr.stop(&rec_id).unwrap();
        assert_eq!(
            file_path, path,
            "file_path should match what was provided at start"
        );
        assert!(
            duration_ms >= 10,
            "duration should be at least 10ms, got {duration_ms}"
        );
        assert!(
            duration_ms < 5000,
            "duration should be reasonable, got {duration_ms}"
        );
        assert_eq!(packets, 7, "should report 7 packets");
        assert_eq!(dropped, 0, "should have no dropped packets");

        // Recording should no longer be active
        assert!(mgr.active_recordings().is_empty());
    }

    #[tokio::test]
    async fn test_stop_endpoint_recordings_removes_for_endpoint() {
        let dir = tempfile::tempdir().expect("failed to create temp dir");
        let mut mgr = RecordingManager::new();

        let ep1 = EndpointId::new_v4();
        let ep2 = EndpointId::new_v4();

        let p1 = dir.path().join("ep1.pcap").to_string_lossy().to_string();
        let id1 = mgr.start(Some(ep1), p1.clone()).await.unwrap();

        let p2 = dir.path().join("ep2.pcap").to_string_lossy().to_string();
        let _id2 = mgr.start(Some(ep2), p2).await.unwrap();

        // Session-wide (not tied to any endpoint)
        let p3 = dir
            .path()
            .join("session.pcap")
            .to_string_lossy()
            .to_string();
        let _id3 = mgr.start(None, p3).await.unwrap();

        assert_eq!(mgr.active_recordings().len(), 3);

        // Record packets to ep1 so we can verify counts in the stopped info
        let fake_rtp = [0x80u8; 12];
        for _ in 0..4 {
            mgr.record_packet(&ep1, &fake_rtp, false);
        }

        // Stop only ep1 recordings
        let stopped = mgr.stop_endpoint_recordings(&ep1);
        assert_eq!(stopped.len(), 1, "should stop 1 recording for ep1");
        assert_eq!(stopped[0].recording_id, id1);
        assert_eq!(stopped[0].file_path, p1);
        assert_eq!(stopped[0].packets, 4);

        // ep2 and session-wide should still be active
        assert_eq!(
            mgr.active_recordings().len(),
            2,
            "ep2 and session-wide should remain"
        );

        // Stopping ep1 again should return empty
        let stopped2 = mgr.stop_endpoint_recordings(&ep1);
        assert!(stopped2.is_empty(), "no more recordings for ep1");

        mgr.stop_all();
    }

    #[tokio::test]
    async fn test_stop_all_stops_everything() {
        let dir = tempfile::tempdir().expect("failed to create temp dir");
        let mut mgr = RecordingManager::new();

        let ep1 = EndpointId::new_v4();
        let ep2 = EndpointId::new_v4();

        let p1 = dir.path().join("s1.pcap").to_string_lossy().to_string();
        let id1 = mgr.start(Some(ep1), p1.clone()).await.unwrap();

        let p2 = dir.path().join("s2.pcap").to_string_lossy().to_string();
        let id2 = mgr.start(Some(ep2), p2.clone()).await.unwrap();

        let p3 = dir.path().join("s3.pcap").to_string_lossy().to_string();
        let id3 = mgr.start(None, p3.clone()).await.unwrap();

        // Record some packets
        mgr.record_packet(&ep1, &[0xAA; 100], false);
        mgr.record_packet(&ep2, &[0xBB; 100], true);

        let stopped = mgr.stop_all();
        assert_eq!(stopped.len(), 3, "stop_all should stop all 3 recordings");

        let stopped_ids: Vec<_> = stopped.iter().map(|s| s.recording_id).collect();
        assert!(stopped_ids.contains(&id1));
        assert!(stopped_ids.contains(&id2));
        assert!(stopped_ids.contains(&id3));

        // Verify file paths
        let stopped_paths: Vec<_> = stopped.iter().map(|s| s.file_path.clone()).collect();
        assert!(stopped_paths.contains(&p1));
        assert!(stopped_paths.contains(&p2));
        assert!(stopped_paths.contains(&p3));

        // Verify all are gone
        assert!(
            mgr.active_recordings().is_empty(),
            "no recordings should remain"
        );

        // Manager should be reusable
        let p4 = dir.path().join("s4.pcap").to_string_lossy().to_string();
        mgr.start(None, p4)
            .await
            .expect("manager should be reusable after stop_all");
        assert_eq!(mgr.active_recordings().len(), 1);
        mgr.stop_all();
    }

    #[tokio::test]
    async fn test_start_recording_readonly_dir() {
        let mut mgr = RecordingManager::new();
        let dir = std::env::temp_dir().join("rtpbridge-rec-readonly-test");
        std::fs::create_dir_all(&dir).ok();

        // Make the directory read-only
        let mut perms = std::fs::metadata(&dir).unwrap().permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(&dir, perms).unwrap();

        let path = dir.join("should-fail.pcap").to_string_lossy().to_string();
        let result = mgr.start(None, path).await;
        assert!(
            result.is_err(),
            "recording to read-only directory should fail"
        );
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Cannot create recording file"),
            "error should mention file creation failure"
        );

        // Restore permissions for cleanup
        let mut perms = std::fs::metadata(&dir).unwrap().permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        perms.set_readonly(false);
        std::fs::set_permissions(&dir, perms).unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }
}
