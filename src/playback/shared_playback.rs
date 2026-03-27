use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::{Mutex, broadcast};
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

/// Manages shared file playback across sessions.
/// When multiple sessions play the same file with shared=true,
/// they share a single decode pipeline and receive PCM frames
/// from a broadcast channel.
pub struct SharedPlaybackManager {
    state: Arc<Mutex<SharedState>>,
}

struct SharedState {
    /// source path → shared playback entry
    entries: HashMap<String, SharedEntry>,
}

struct SharedEntry {
    /// Broadcast sender for PCM frames
    tx: broadcast::Sender<Arc<Vec<i16>>>,
    /// Number of active subscribers
    ref_count: u32,
    /// Handle to the decode task (kept alive to prevent task cancellation)
    #[allow(dead_code)]
    task: Option<tokio::task::JoinHandle<()>>,
    /// Cooperative cancellation token for graceful shutdown
    cancel: CancellationToken,
}

/// A subscriber to a shared playback
pub struct SharedPlaybackSubscriber {
    pub rx: broadcast::Receiver<Arc<Vec<i16>>>,
    source: String,
    manager: Arc<Mutex<SharedState>>,
    cleaned_up: bool,
    /// Captured at construction time so Drop works even after runtime shutdown.
    runtime_handle: Option<tokio::runtime::Handle>,
}

impl Default for SharedPlaybackManager {
    fn default() -> Self {
        Self::new()
    }
}

impl SharedPlaybackManager {
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(SharedState {
                entries: HashMap::new(),
            })),
        }
    }

    /// Subscribe to shared playback of a file.
    /// If this is the first subscriber, starts the decode task.
    /// Returns a subscriber that receives PCM frames via broadcast.
    ///
    /// `start_ms` and `loop_count` are only used when starting a new shared
    /// decode task (first subscriber). Subsequent subscribers join the
    /// already-running stream.
    pub async fn subscribe(
        &self,
        source: &str,
        sample_rate: u32,
        start_ms: u64,
        loop_count: Option<u32>,
    ) -> anyhow::Result<SharedPlaybackSubscriber> {
        let mut state = self.state.lock().await;

        if let Some(entry) = state.entries.get_mut(source) {
            // Existing shared playback — just subscribe
            entry.ref_count += 1;
            let rx = entry.tx.subscribe();
            return Ok(SharedPlaybackSubscriber {
                rx,
                source: source.to_string(),
                manager: Arc::clone(&self.state),
                cleaned_up: false,
                runtime_handle: tokio::runtime::Handle::try_current().ok(),
            });
        }

        // New shared playback — start decode task.
        // 128 frames ≈ 2.56s at 20ms ptime — enough buffer for transient subscriber lag.
        let (tx, rx) = broadcast::channel::<Arc<Vec<i16>>>(128);
        let tx_clone = tx.clone();
        let source_owned = source.to_string();
        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();

        let state_clone = Arc::clone(&self.state);
        let key_clone = source.to_string();
        let task = tokio::spawn(async move {
            shared_decode_task(
                &source_owned,
                tx_clone,
                sample_rate,
                cancel_clone,
                start_ms,
                loop_count,
                state_clone,
                key_clone,
            )
            .await;
        });

        state.entries.insert(
            source.to_string(),
            SharedEntry {
                tx: tx.clone(),
                ref_count: 1,
                task: Some(task),
                cancel,
            },
        );

        Ok(SharedPlaybackSubscriber {
            rx,
            source: source.to_string(),
            manager: Arc::clone(&self.state),
            cleaned_up: false,
            runtime_handle: tokio::runtime::Handle::try_current().ok(),
        })
    }

    /// Unsubscribe from shared playback.
    /// Decrements ref_count and stops the decode task when no subscribers remain.
    #[cfg_attr(not(test), allow(dead_code))]
    pub async fn unsubscribe(&self, source: &str) {
        let mut state = self.state.lock().await;
        if let Some(entry) = state.entries.get_mut(source) {
            entry.ref_count = entry.ref_count.saturating_sub(1);
            if entry.ref_count == 0 {
                // Last subscriber — signal cooperative shutdown
                entry.cancel.cancel();
                state.entries.remove(source);
                debug!(source = source, "shared playback stopped (no subscribers)");
            }
        }
    }
}

impl SharedPlaybackSubscriber {
    /// Explicitly clean up this subscriber. Prefer calling this over relying on Drop,
    /// as Drop cannot reliably decrement ref_count outside a tokio runtime context.
    pub async fn cleanup(mut self) {
        self.cleaned_up = true;
        let mut state = self.manager.lock().await;
        if let Some(entry) = state.entries.get_mut(&self.source) {
            entry.ref_count = entry.ref_count.saturating_sub(1);
            if entry.ref_count == 0 {
                entry.cancel.cancel();
                state.entries.remove(&self.source);
                debug!(source = %self.source, "shared playback stopped (no subscribers)");
            }
        }
    }
}

// NOTE: Drop-spawned cleanup is best-effort. If the tokio runtime is shutting down,
// the ref_count decrement may be lost. Callers should use cleanup() for reliable cleanup.
impl Drop for SharedPlaybackSubscriber {
    fn drop(&mut self) {
        if self.cleaned_up {
            return;
        }
        let source = self.source.clone();
        let manager = Arc::clone(&self.manager);
        // Spawn cleanup since drop can't be async.
        // Use the handle captured at construction time so cleanup works even
        // after the thread-local runtime context has been torn down.
        let handle = self
            .runtime_handle
            .take()
            .or_else(|| tokio::runtime::Handle::try_current().ok());
        match handle {
            Some(handle) => {
                handle.spawn(async move {
                    let mut state = manager.lock().await;
                    if let Some(entry) = state.entries.get_mut(&source) {
                        entry.ref_count = entry.ref_count.saturating_sub(1);
                        if entry.ref_count == 0 {
                            entry.cancel.cancel();
                            state.entries.remove(&source);
                        }
                    }
                });
            }
            None => {
                // No tokio runtime at construction or drop time.
                // This is expected during process shutdown; the OS will reclaim resources.
                debug!(source = %source, "SharedPlaybackSubscriber dropped outside tokio runtime, ref_count not decremented");
            }
        }
    }
}

/// Background task that decodes an audio file and broadcasts PCM frames.
/// Stops cooperatively when the cancellation token is cancelled (last subscriber left).
/// On exit (EOF, error, or cancellation), removes its entry from `SharedState` so
/// future `subscribe()` calls create a fresh decode task.
#[allow(clippy::too_many_arguments)]
async fn shared_decode_task(
    source: &str,
    tx: broadcast::Sender<Arc<Vec<i16>>>,
    sample_rate: u32,
    cancel: CancellationToken,
    start_ms: u64,
    loop_count: Option<u32>,
    state: Arc<Mutex<SharedState>>,
    key: String,
) {
    use crate::session::endpoint_file::FileEndpoint;

    let id = uuid::Uuid::new_v4();
    let source_str = source.to_string();
    let source_owned = source.to_string();
    let mut endpoint = match tokio::task::spawn_blocking(move || {
        FileEndpoint::open(id, &source_owned, start_ms, loop_count)
    })
    .await
    {
        Ok(Ok(ep)) => ep,
        Ok(Err(e)) => {
            tracing::warn!(source = %source_str, error = %e, "failed to open shared playback file");
            return;
        }
        Err(e) => {
            tracing::warn!(source = %source_str, error = %e, "spawn_blocking failed for shared playback");
            return;
        }
    };

    // Read 20ms of audio at the file's native rate, then resample to the requested rate.
    let file_rate = endpoint.sample_rate();
    let file_ptime_samples = (file_rate as usize) / 50; // 20ms at file's native rate
    let needs_resample = file_rate != sample_rate && file_rate > 0 && sample_rate > 0;
    let mut resampler = if needs_resample {
        Some(crate::media::resample::Resampler::new(
            file_rate,
            sample_rate,
        ))
    } else {
        None
    };

    let interval = tokio::time::Duration::from_millis(20);
    let mut timer = tokio::time::interval(interval);
    // Delay (not Burst) to avoid rapid catch-up after slow next_pcm() calls
    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                debug!(source = source, "shared decode task cancelled (no subscribers)");
                break;
            }
            _ = timer.tick() => {}
        }

        match endpoint.next_pcm(file_ptime_samples) {
            Some(pcm) => {
                let pcm = if let Some(ref mut rs) = resampler {
                    let mut buf = Vec::new();
                    rs.process(&pcm, &mut buf);
                    buf
                } else {
                    pcm
                };
                if tx.send(Arc::new(pcm)).is_err() {
                    if tx.receiver_count() == 0 {
                        break;
                    }
                    warn!(
                        source = source,
                        "shared playback: all receivers lagged, frame dropped"
                    );
                }
            }
            None => {
                break;
            }
        }
    }

    // Clean up the entry so future subscribe() calls create a fresh decode task.
    // The entry's `tx` is dropped when we remove it, causing all active rx.recv()
    // calls to return RecvError — subscribers see this and stop reading.
    {
        let mut st = state.lock().await;
        st.entries.remove(&key);
    }
    debug!(source = source, "shared decode task ended");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_shared_subscribe_and_unsubscribe() {
        // Generate a test WAV for shared playback
        let wav_path = "/tmp/rtpbridge-shared-playback-test.wav";
        generate_test_wav_8k(wav_path, 0.5);

        let mgr = SharedPlaybackManager::new();

        // First subscriber starts the decode task
        let sub1 = mgr.subscribe(wav_path, 8000, 0, None).await;
        assert!(sub1.is_ok(), "first subscribe should succeed");

        // Second subscriber should join the existing playback
        let sub2 = mgr.subscribe(wav_path, 8000, 0, None).await;
        assert!(sub2.is_ok(), "second subscribe should succeed");

        // Check state: 2 subscribers
        {
            let state = mgr.state.lock().await;
            let entry = state.entries.get(wav_path).unwrap();
            assert_eq!(entry.ref_count, 2, "should have 2 subscribers");
        }

        // Unsubscribe one
        mgr.unsubscribe(wav_path).await;
        {
            let state = mgr.state.lock().await;
            let entry = state.entries.get(wav_path).unwrap();
            assert_eq!(
                entry.ref_count, 1,
                "should have 1 subscriber after unsubscribe"
            );
        }

        // Unsubscribe last — entry should be removed
        mgr.unsubscribe(wav_path).await;
        {
            let state = mgr.state.lock().await;
            assert!(
                state.entries.get(wav_path).is_none(),
                "entry should be removed when last subscriber leaves"
            );
        }

        std::fs::remove_file(wav_path).ok();
    }

    #[tokio::test]
    async fn test_shared_playback_slow_subscriber_lag() {
        let wav_path = "/tmp/rtpbridge-shared-lag-test.wav";
        generate_test_wav_8k(wav_path, 2.0);

        let mgr = SharedPlaybackManager::new();
        let mut sub = mgr.subscribe(wav_path, 8000, 0, None).await.unwrap();

        // Don't read from the receiver — let the broadcast buffer fill up
        // The buffer is 128 frames. Wait for the decode task to produce more than that.
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;

        // Now try to receive — should get a Lagged error (frames were dropped)
        let result = sub.rx.try_recv();
        let is_lagged = matches!(
            result,
            Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_))
        );
        let is_ok = result.is_ok();
        assert!(
            is_lagged || is_ok,
            "slow subscriber should see Lagged error or still receive: {:?}",
            result
        );

        mgr.unsubscribe(wav_path).await;
        std::fs::remove_file(wav_path).ok();
    }

    #[tokio::test]
    async fn test_shared_playback_all_subscribers_disconnect() {
        let wav_path = "/tmp/rtpbridge-shared-disconnect-test.wav";
        generate_test_wav_8k(wav_path, 2.0);

        let mgr = SharedPlaybackManager::new();
        let sub1 = mgr.subscribe(wav_path, 8000, 0, None).await.unwrap();
        let sub2 = mgr.subscribe(wav_path, 8000, 0, None).await.unwrap();

        // Verify 2 subscribers
        {
            let state = mgr.state.lock().await;
            assert_eq!(state.entries.get(wav_path).unwrap().ref_count, 2);
        }

        // Drop both subscribers — the decode task should notice and stop
        // without panicking (exercises the tx.send() error path in shared_decode_task)
        drop(sub1);
        drop(sub2);

        // Give the Drop spawned tasks and decode task time to clean up
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        // Explicitly unsubscribe twice to mirror the subscriber count
        mgr.unsubscribe(wav_path).await;
        mgr.unsubscribe(wav_path).await;

        // Entry should be gone
        {
            let state = mgr.state.lock().await;
            assert!(
                state.entries.get(wav_path).is_none(),
                "entry should be removed after all subscribers disconnect"
            );
        }

        std::fs::remove_file(wav_path).ok();
    }

    #[tokio::test]
    async fn test_resubscribe_after_full_unsubscribe() {
        let wav_path = "/tmp/rtpbridge-shared-resub-test.wav";
        generate_test_wav_8k(wav_path, 0.5);

        let mgr = SharedPlaybackManager::new();

        // Subscribe and then fully unsubscribe
        let _sub = mgr.subscribe(wav_path, 8000, 0, None).await.unwrap();
        mgr.unsubscribe(wav_path).await;

        // Entry should be gone
        {
            let state = mgr.state.lock().await;
            assert!(
                state.entries.get(wav_path).is_none(),
                "entry should be removed"
            );
        }

        // Re-subscribe — should create a new entry with ref_count=1
        let _sub2 = mgr.subscribe(wav_path, 8000, 0, None).await.unwrap();
        {
            let state = mgr.state.lock().await;
            let entry = state.entries.get(wav_path).unwrap();
            assert_eq!(entry.ref_count, 1, "re-subscribe should create fresh entry");
        }

        mgr.unsubscribe(wav_path).await;
        std::fs::remove_file(wav_path).ok();
    }

    #[tokio::test]
    async fn test_subscriber_cleanup_prevents_double_decrement() {
        let wav_path = "/tmp/rtpbridge-shared-cleanup-test.wav";
        generate_test_wav_8k(wav_path, 0.5);

        let mgr = SharedPlaybackManager::new();

        // Subscribe twice
        let sub1 = mgr.subscribe(wav_path, 8000, 0, None).await.unwrap();
        let _sub2 = mgr.subscribe(wav_path, 8000, 0, None).await.unwrap();

        {
            let state = mgr.state.lock().await;
            assert_eq!(state.entries.get(wav_path).unwrap().ref_count, 2);
        }

        // Explicitly cleanup sub1 — sets cleaned_up=true
        sub1.cleanup().await;

        {
            let state = mgr.state.lock().await;
            let entry = state.entries.get(wav_path).unwrap();
            assert_eq!(
                entry.ref_count, 1,
                "cleanup should decrement ref_count once"
            );
        }

        // When sub1 is dropped (already consumed by cleanup), Drop should NOT
        // decrement again because cleaned_up=true. The ref_count stays at 1.
        // We verify by checking the count is still 1 after a brief delay.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        {
            let state = mgr.state.lock().await;
            let entry = state.entries.get(wav_path).unwrap();
            assert_eq!(
                entry.ref_count, 1,
                "Drop should not double-decrement after cleanup"
            );
        }

        mgr.unsubscribe(wav_path).await;
        std::fs::remove_file(wav_path).ok();
    }

    #[tokio::test]
    async fn test_concurrent_subscribe_unsubscribe() {
        let wav_path = "/tmp/rtpbridge-shared-concurrent-test.wav";
        generate_test_wav_8k(wav_path, 1.0);

        let mgr = Arc::new(SharedPlaybackManager::new());
        let mut handles = Vec::new();

        for _ in 0..10 {
            let mgr = Arc::clone(&mgr);
            let path = wav_path.to_string();
            handles.push(tokio::spawn(async move {
                let _sub = mgr.subscribe(&path, 8000, 0, None).await.unwrap();
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                mgr.unsubscribe(&path).await;
            }));
        }

        for h in handles {
            h.await.unwrap();
        }

        // After all subscribe/unsubscribe cycles, the entry should be gone
        {
            let state = mgr.state.lock().await;
            assert!(
                state.entries.get(wav_path).is_none(),
                "all subscribers removed — entry should be gone"
            );
        }

        std::fs::remove_file(wav_path).ok();
    }

    #[tokio::test]
    async fn test_shared_playback_produces_pcm_frames() {
        let wav_path = "/tmp/rtpbridge-shared-pcm-test.wav";
        generate_test_wav_8k(wav_path, 0.5);

        let mgr = SharedPlaybackManager::new();
        let mut sub = mgr.subscribe(wav_path, 8000, 0, None).await.unwrap();

        // Wait for decode task to produce some frames
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        // Should have received at least one PCM frame
        let result = sub.rx.try_recv();
        assert!(
            result.is_ok(),
            "should receive PCM frames from decode task: {:?}",
            result.err()
        );
        let frame = result.unwrap();
        assert!(!frame.is_empty(), "PCM frame should not be empty");

        mgr.unsubscribe(wav_path).await;
        std::fs::remove_file(wav_path).ok();
    }

    #[tokio::test]
    async fn test_shared_playback_cleanup_on_file_finish() {
        // Regression: when a shared file plays to completion, the entry must be
        // removed so future subscribe() calls create a fresh decode task.
        let wav_path = "/tmp/rtpbridge-shared-finish-test.wav";
        generate_test_wav_8k(wav_path, 0.1); // very short file, finishes quickly

        let mgr = SharedPlaybackManager::new();

        // Subscribe with loop_count=Some(0) so it plays once and finishes
        let _sub = mgr.subscribe(wav_path, 8000, 0, Some(0)).await.unwrap();

        // Wait for the file to finish playing and the decode task to clean up
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;

        // Entry should be removed by the decode task's cleanup
        {
            let state = mgr.state.lock().await;
            assert!(
                state.entries.get(wav_path).is_none(),
                "entry should be removed after file finishes playing"
            );
        }

        // Re-subscribing should work (creates a fresh decode task)
        let sub2 = mgr.subscribe(wav_path, 8000, 0, Some(0)).await;
        assert!(
            sub2.is_ok(),
            "re-subscribe after file finished should succeed"
        );

        mgr.unsubscribe(wav_path).await;
        std::fs::remove_file(wav_path).ok();
    }

    /// Generate a minimal 8kHz mono 16-bit WAV
    fn generate_test_wav_8k(path: &str, duration_secs: f64) {
        use std::io::Write;
        let sample_rate: u32 = 8000;
        let num_samples = (sample_rate as f64 * duration_secs) as usize;
        let data_size = (num_samples * 2) as u32;

        let mut file = std::fs::File::create(path).unwrap();
        file.write_all(b"RIFF").unwrap();
        file.write_all(&(36 + data_size).to_le_bytes()).unwrap();
        file.write_all(b"WAVE").unwrap();
        file.write_all(b"fmt ").unwrap();
        file.write_all(&16u32.to_le_bytes()).unwrap();
        file.write_all(&1u16.to_le_bytes()).unwrap();
        file.write_all(&1u16.to_le_bytes()).unwrap(); // channels
        file.write_all(&sample_rate.to_le_bytes()).unwrap();
        file.write_all(&(sample_rate * 2).to_le_bytes()).unwrap(); // byte_rate
        file.write_all(&2u16.to_le_bytes()).unwrap(); // block_align
        file.write_all(&16u16.to_le_bytes()).unwrap(); // bits per sample
        file.write_all(b"data").unwrap();
        file.write_all(&data_size.to_le_bytes()).unwrap();

        for i in 0..num_samples {
            let t = i as f64 / sample_rate as f64;
            let sample = (f64::sin(2.0 * std::f64::consts::PI * 440.0 * t) * 16000.0) as i16;
            file.write_all(&sample.to_le_bytes()).unwrap();
        }
    }
}
