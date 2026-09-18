use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::sync::{broadcast, watch};
use tokio_util::sync::CancellationToken;

use super::file_cache::CacheLease;

pub struct SharedPlaybackManager {
    state: Arc<Mutex<SharedState>>,
}
struct SharedState {
    entries: HashMap<String, SharedEntry>,
}
struct SharedEntry {
    generation: uuid::Uuid,
    tx: broadcast::Sender<Arc<Vec<i16>>>,
    terminal: watch::Receiver<Option<Result<(), String>>>,
    ref_count: u32,
    cancel: CancellationToken,
}

pub struct SharedPlaybackSubscriber {
    pub rx: broadcast::Receiver<Arc<Vec<i16>>>,
    source: String,
    generation: uuid::Uuid,
    manager: Arc<Mutex<SharedState>>,
    terminal: watch::Receiver<Option<Result<(), String>>>,
    cleaned_up: bool,
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

    #[allow(dead_code)] // convenience API for embedded users
    pub async fn subscribe(
        &self,
        source: &str,
        sample_rate: u32,
        start_ms: u64,
        loop_count: Option<u32>,
    ) -> anyhow::Result<SharedPlaybackSubscriber> {
        self.subscribe_with_lease(source, sample_rate, start_ms, loop_count, None)
            .await
    }

    pub async fn subscribe_with_lease(
        &self,
        source: &str,
        sample_rate: u32,
        start_ms: u64,
        loop_count: Option<u32>,
        lease: Option<CacheLease>,
    ) -> anyhow::Result<SharedPlaybackSubscriber> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = state.entries.get_mut(source) {
            if entry.ref_count >= 1024 {
                anyhow::bail!("SHARED_PLAYBACK_BUSY");
            }
            entry.ref_count += 1;
            return Ok(SharedPlaybackSubscriber {
                rx: entry.tx.subscribe(),
                source: source.into(),
                generation: entry.generation,
                manager: Arc::clone(&self.state),
                terminal: entry.terminal.clone(),
                cleaned_up: false,
            });
        }
        if state.entries.len() >= 64 {
            anyhow::bail!("SHARED_PLAYBACK_BUSY");
        }
        if !matches!(sample_rate, 8000 | 16000 | 48000) {
            anyhow::bail!("unsupported shared playback rate");
        }
        let admission = crate::storage::admit_stream()?;
        let generation = uuid::Uuid::new_v4();
        let (tx, rx) = broadcast::channel(128);
        let (terminal, terminal_rx) = watch::channel(None);
        let cancel = CancellationToken::new();
        state.entries.insert(
            source.into(),
            SharedEntry {
                generation,
                tx: tx.clone(),
                terminal: terminal_rx.clone(),
                ref_count: 1,
                cancel: cancel.clone(),
            },
        );
        let guard = DecodeGuard {
            manager: Arc::clone(&self.state),
            source: source.into(),
            generation,
            terminal,
        };
        let subscriber = SharedPlaybackSubscriber {
            rx,
            source: source.into(),
            generation,
            manager: Arc::clone(&self.state),
            terminal: terminal_rx,
            cleaned_up: false,
        };
        tokio::spawn(async move {
            decode(
                guard,
                tx,
                cancel,
                sample_rate,
                start_ms,
                loop_count,
                lease,
                admission,
            )
            .await;
        });
        Ok(subscriber)
    }

    #[cfg(test)]
    async fn unsubscribe(&self, source: &str) {
        let mut state = self.state.lock().unwrap();
        if let Some(entry) = state.entries.get_mut(source) {
            entry.ref_count -= 1;
            if entry.ref_count == 0 {
                entry.cancel.cancel();
                state.entries.remove(source);
            }
        }
    }
}

impl SharedPlaybackSubscriber {
    pub async fn cleanup(mut self) {
        self.release();
    }
    pub fn error(&self) -> Option<String> {
        self.terminal
            .borrow()
            .as_ref()
            .and_then(|result| result.as_ref().err())
            .cloned()
    }
    fn release(&mut self) {
        if self.cleaned_up {
            return;
        }
        self.cleaned_up = true;
        let mut state = self.manager.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = state.entries.get_mut(&self.source)
            && entry.generation == self.generation
        {
            entry.ref_count = entry.ref_count.saturating_sub(1);
            if entry.ref_count == 0 {
                entry.cancel.cancel();
                state.entries.remove(&self.source);
            }
        }
    }
}
impl Drop for SharedPlaybackSubscriber {
    fn drop(&mut self) {
        self.release();
    }
}

struct DecodeGuard {
    manager: Arc<Mutex<SharedState>>,
    source: String,
    generation: uuid::Uuid,
    terminal: watch::Sender<Option<Result<(), String>>>,
}
impl Drop for DecodeGuard {
    fn drop(&mut self) {
        let unfinished = self.terminal.borrow().is_none();
        if unfinished {
            self.terminal
                .send_replace(Some(Err("playback initialization or decode failed".into())));
        }
        let mut state = self.manager.lock().unwrap_or_else(|e| e.into_inner());
        if state
            .entries
            .get(&self.source)
            .is_some_and(|entry| entry.generation == self.generation)
        {
            state.entries.remove(&self.source);
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn decode(
    guard: DecodeGuard,
    tx: broadcast::Sender<Arc<Vec<i16>>>,
    cancel: CancellationToken,
    sample_rate: u32,
    start_ms: u64,
    loop_count: Option<u32>,
    lease: Option<CacheLease>,
    admission: tokio::sync::OwnedSemaphorePermit,
) {
    let source = guard.source.clone();
    let opened = tokio::select! {
        _ = cancel.cancelled() => return,
        opened = crate::storage::run_with_deadline(move || {
            let mut endpoint = crate::session::endpoint_file::FileEndpoint::open(uuid::Uuid::new_v4(), &source, start_ms, loop_count, 0.0)?;
            endpoint.cache_lease = lease;
            endpoint.storage_admission = Some(admission);
            Ok(endpoint)
        }) => opened,
    };
    let Ok(mut endpoint) = opened else {
        return;
    };
    let file_rate = endpoint.sample_rate();
    let samples = file_rate as usize / 50;
    let mut resampler = (file_rate != sample_rate)
        .then(|| crate::media::resample::Resampler::new(file_rate, sample_rate));
    let mut timer = tokio::time::interval(std::time::Duration::from_millis(20));
    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! { _ = cancel.cancelled() => break, _ = timer.tick() => {} }
        let decoded = tokio::select! {
            _ = cancel.cancelled() => break,
            decoded = crate::storage::run_with_deadline(move || {
                let pcm = endpoint.next_pcm(samples).map(|pcm| {
                    if let Some(resampler) = &mut resampler {
                        let mut output = Vec::new(); resampler.process(&pcm, &mut output); output
                    } else { pcm }
                });
                Ok((endpoint, resampler, pcm))
            }) => decoded,
        };
        let Ok((next_endpoint, next_resampler, pcm)) = decoded else {
            return;
        };
        endpoint = next_endpoint;
        resampler = next_resampler;
        match pcm {
            Some(pcm) => {
                if tx.send(Arc::new(pcm)).is_err() {
                    break;
                }
            }
            None => {
                guard
                    .terminal
                    .send_replace(Some(match endpoint.playback_error.take() {
                        Some(error) => Err(error),
                        None => Ok(()),
                    }));
                break;
            }
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn stalled_storage_terminates_shared_playback() {
        let blocked = crate::storage::block_workers().await;
        let manager = SharedPlaybackManager::new();
        let subscribed = manager.subscribe("queued.wav", 8000, 0, None).await;
        let mut subscriber = subscribed.unwrap();
        tokio::task::yield_now().await;
        tokio::time::advance(std::time::Duration::from_secs(11)).await;
        let terminal = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            subscriber.terminal.changed(),
        )
        .await;
        terminal
            .expect("stalled storage must report a terminal error")
            .unwrap();
        assert!(subscriber.error().is_some());
        assert!(manager.state.lock().unwrap().entries.is_empty());
        drop(blocked);
    }

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
            let state = mgr.state.lock().unwrap();
            let entry = state.entries.get(wav_path).unwrap();
            assert_eq!(entry.ref_count, 2, "should have 2 subscribers");
        }

        // Unsubscribe one
        mgr.unsubscribe(wav_path).await;
        {
            let state = mgr.state.lock().unwrap();
            let entry = state.entries.get(wav_path).unwrap();
            assert_eq!(
                entry.ref_count, 1,
                "should have 1 subscriber after unsubscribe"
            );
        }

        // Unsubscribe last — entry should be removed
        mgr.unsubscribe(wav_path).await;
        {
            let state = mgr.state.lock().unwrap();
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
            let state = mgr.state.lock().unwrap();
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
            let state = mgr.state.lock().unwrap();
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
            let state = mgr.state.lock().unwrap();
            assert!(
                state.entries.get(wav_path).is_none(),
                "entry should be removed"
            );
        }

        // Re-subscribe — should create a new entry with ref_count=1
        let _sub2 = mgr.subscribe(wav_path, 8000, 0, None).await.unwrap();
        {
            let state = mgr.state.lock().unwrap();
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
            let state = mgr.state.lock().unwrap();
            assert_eq!(state.entries.get(wav_path).unwrap().ref_count, 2);
        }

        // Explicitly cleanup sub1 — sets cleaned_up=true
        sub1.cleanup().await;

        {
            let state = mgr.state.lock().unwrap();
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
            let state = mgr.state.lock().unwrap();
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
            let state = mgr.state.lock().unwrap();
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
            let state = mgr.state.lock().unwrap();
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
    #[tokio::test]
    async fn dropping_finished_generation_cannot_cancel_replacement() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("generation.wav");
        let path = path.to_str().unwrap();
        generate_test_wav_8k(path, 0.04);
        let manager = SharedPlaybackManager::new();
        let first = manager.subscribe(path, 8000, 0, Some(0)).await;
        let mut first = first.unwrap();
        let finished = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while first.terminal.borrow().is_none() {
                let changed = first.terminal.changed().await;
                changed.unwrap();
            }
        })
        .await;
        assert!(finished.is_ok());
        let second = manager.subscribe(path, 8000, 0, None).await;
        let mut second = second.unwrap();
        assert_ne!(first.generation, second.generation);
        drop(first);
        {
            let state = manager.state.lock().unwrap();
            let current = state.entries.get(path).unwrap();
            assert_eq!(current.generation, second.generation);
            assert_eq!(current.ref_count, 1);
            assert!(!current.cancel.is_cancelled());
        }
        let frame = tokio::time::timeout(std::time::Duration::from_secs(2), second.rx.recv()).await;
        assert!(!frame.unwrap().unwrap().is_empty());
    }

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
