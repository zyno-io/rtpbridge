use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::{OwnedSemaphorePermit, Semaphore, watch};
use tokio_util::sync::CancellationToken;

use super::download_policy::DownloadPolicy;

#[derive(Clone)]
pub struct FileCache {
    cache_dir: PathBuf,
    directory_lock: Arc<std::fs::File>,
    state: Arc<Mutex<CacheState>>,
    max_entries: usize,
    max_inflight: usize,
    download_semaphore: Arc<Semaphore>,
    owners: Arc<Semaphore>,
    max_download_bytes: u64,
    max_cache_bytes: u64,
    disk_bytes: Arc<AtomicU64>,
    policy: DownloadPolicy,
}

#[derive(Default)]
struct CacheState {
    entries: HashMap<String, CacheEntry>,
    inflight: HashMap<String, Arc<Pending>>,
    garbage: Vec<Arc<CachedFile>>,
}

struct CacheEntry {
    file: Arc<CachedFile>,
    expires_at: Instant,
}

#[derive(Debug)]
struct DiskReservation {
    _directory_lock: Arc<std::fs::File>,
    total: Arc<AtomicU64>,
    bytes: AtomicU64,
}
impl DiskReservation {
    fn shrink(&self, bytes: u64) {
        let previous = self.bytes.swap(bytes, Ordering::AcqRel);
        self.total.fetch_sub(previous - bytes, Ordering::AcqRel);
    }
}
impl Drop for DiskReservation {
    fn drop(&mut self) {
        self.total
            .fetch_sub(self.bytes.load(Ordering::Relaxed), Ordering::AcqRel);
    }
}

#[derive(Debug)]
struct CachedFile {
    path: PathBuf,
    key: String,
    reservation: Arc<DiskReservation>,
}

/// Ownership of an exact cache-file instance, independent of URL/header recomputation.
#[derive(Debug, Clone)]
pub struct CacheLease(Arc<CachedFile>);
impl CacheLease {
    pub fn key(&self) -> &str {
        &self.0.key
    }
}
impl std::ops::Deref for CacheLease {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.0.path
    }
}
impl AsRef<Path> for CacheLease {
    fn as_ref(&self) -> &Path {
        &self.0.path
    }
}

struct Pending {
    result: watch::Sender<Option<Result<CacheLease, String>>>,
    cancel: CancellationToken,
    owners: AtomicUsize,
}

/// Admitted before any per-owner task is spawned. Dropping the last owner cancels work.
pub struct DownloadRequest {
    pending: Option<Arc<Pending>>,
    receiver: Option<watch::Receiver<Option<Result<CacheLease, String>>>>,
    ready: Option<CacheLease>,
    deadline: tokio::time::Instant,
    _permit: OwnedSemaphorePermit,
}
impl Drop for DownloadRequest {
    fn drop(&mut self) {
        if let Some(pending) = &self.pending
            && pending.owners.fetch_sub(1, Ordering::AcqRel) == 1
        {
            pending.cancel.cancel();
        }
    }
}
impl DownloadRequest {
    pub async fn wait(mut self) -> anyhow::Result<CacheLease> {
        if let Some(ready) = self.ready.take() {
            return Ok(ready);
        }
        let receiver = self
            .receiver
            .as_mut()
            .expect("pending request has a receiver");
        let result = tokio::time::timeout_at(self.deadline, async {
            loop {
                let result = receiver.borrow_and_update().clone();
                if let Some(result) = result {
                    return result.map_err(anyhow::Error::msg);
                }
                let changed = receiver.changed().await;
                changed.map_err(|_| anyhow::anyhow!("download worker ended"))?;
            }
        })
        .await;
        result.map_err(|_| anyhow::anyhow!("Download timeout including queue time"))?
    }
}

/// Cleanup bookkeeping is synchronous; actual deletion runs on bounded workers.
struct DownloadGuard {
    state: Arc<Mutex<CacheState>>,
    key: String,
    pending: Arc<Pending>,
    unpublished: Vec<Arc<CachedFile>>,
}
impl Drop for DownloadGuard {
    fn drop(&mut self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state
            .inflight
            .get(&self.key)
            .is_some_and(|entry| Arc::ptr_eq(entry, &self.pending))
        {
            state.inflight.remove(&self.key);
        }
        state.garbage.append(&mut self.unpublished);
        let unfinished = self.pending.result.borrow().is_none();
        if unfinished {
            self.pending
                .result
                .send_replace(Some(Err("download cancelled".into())));
        }
    }
}

impl FileCache {
    #[allow(dead_code)] // convenience constructor for embedding
    pub fn new(cache_dir: PathBuf) -> anyhow::Result<Self> {
        Self::with_options(cache_dir, 1000, 16, 100 * 1024 * 1024)
    }

    pub fn with_options(
        cache_dir: PathBuf,
        max_entries: usize,
        max_concurrent_downloads: usize,
        max_download_bytes: u64,
    ) -> anyhow::Result<Self> {
        if max_entries == 0 || max_concurrent_downloads == 0 || max_download_bytes == 0 {
            anyhow::bail!("file cache limits must be nonzero");
        }
        std::fs::create_dir_all(&cache_dir)?;
        let directory_lock = Arc::new(
            std::fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(cache_dir.join(".rtpbridge.lock"))?,
        );
        directory_lock
            .try_lock()
            .map_err(|_| anyhow::anyhow!("cache directory is already in use"))?;
        // Recover only files exclusively named by this implementation.
        for entry in std::fs::read_dir(&cache_dir)? {
            let entry = entry?;
            if entry
                .file_name()
                .to_string_lossy()
                .starts_with("rtpbridge-cache-")
                && entry.file_type()?.is_file()
            {
                // Do not start with unaccounted cache bytes if recovery fails.
                std::fs::remove_file(entry.path())?;
            }
        }
        Ok(Self {
            cache_dir,
            directory_lock,
            state: Arc::new(Mutex::new(CacheState::default())),
            max_entries,
            max_inflight: 64,
            download_semaphore: Arc::new(Semaphore::new(max_concurrent_downloads)),
            owners: Arc::new(Semaphore::new(256)),
            max_download_bytes,
            max_cache_bytes: (1024 * 1024 * 1024).max(max_download_bytes),
            disk_bytes: Arc::new(AtomicU64::new(0)),
            policy: DownloadPolicy::default(),
        })
    }

    #[allow(dead_code)] // explicit policy for embedded/test servers
    pub fn with_policy(mut self, policy: DownloadPolicy) -> Self {
        self.policy = policy;
        self
    }

    pub fn from_config(config: &crate::config::Config) -> anyhow::Result<Self> {
        let mut cache = Self::with_options(
            config.cache_dir.clone(),
            config.max_cache_entries,
            config.max_concurrent_downloads,
            config.max_file_download_bytes,
        )?;
        cache.max_cache_bytes = config.max_cache_bytes;
        cache.max_inflight = config.max_pending_downloads;
        cache.owners = Arc::new(Semaphore::new(config.max_download_owners));
        cache.policy = DownloadPolicy::new(
            &config.file_download_origins,
            config.file_download_networks.clone(),
        )?;
        Ok(cache)
    }

    pub fn start_download(
        &self,
        url: &str,
        cache_ttl_secs: u32,
        timeout_ms: u32,
        headers: Option<&HashMap<String, String>>,
    ) -> anyhow::Result<DownloadRequest> {
        self.policy.validate(url)?;
        if timeout_ms == 0 || timeout_ms > 60_000 {
            anyhow::bail!("download timeout must be 1..60000 ms");
        }
        let deadline = tokio::time::Instant::now() + Duration::from_millis(u64::from(timeout_ms));
        let permit = Arc::clone(&self.owners)
            .try_acquire_owned()
            .map_err(|_| anyhow::anyhow!("DOWNLOAD_BUSY"))?;
        let key = cache_key_hash(url, headers);
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = state.entries.get_mut(&key) {
            entry.expires_at = Instant::now() + Duration::from_secs(u64::from(cache_ttl_secs));
            return Ok(DownloadRequest {
                ready: Some(CacheLease(Arc::clone(&entry.file))),
                pending: None,
                receiver: None,
                deadline,
                _permit: permit,
            });
        }
        if let Some(pending) = state.inflight.get(&key)
            && !pending.cancel.is_cancelled()
        {
            pending.owners.fetch_add(1, Ordering::AcqRel);
            return Ok(DownloadRequest {
                ready: None,
                pending: Some(Arc::clone(pending)),
                receiver: Some(pending.result.subscribe()),
                deadline,
                _permit: permit,
            });
        }
        // A cancelled instance must finish cleanup before the same key is reused.
        if state.inflight.contains_key(&key)
            || state.inflight.len() >= self.max_inflight
            || state.entries.len() + state.inflight.len() + state.garbage.len() >= self.max_entries
        {
            anyhow::bail!("DOWNLOAD_BUSY");
        }
        let (result, receiver) = watch::channel(None);
        let pending = Arc::new(Pending {
            result,
            cancel: CancellationToken::new(),
            owners: AtomicUsize::new(1),
        });
        state.inflight.insert(key.clone(), Arc::clone(&pending));
        drop(state);
        let request = DownloadRequest {
            pending: Some(Arc::clone(&pending)),
            receiver: Some(receiver),
            ready: None,
            deadline,
            _permit: permit,
        };
        let cache = self.clone();
        let url = url.to_string();
        let headers = headers.cloned();
        let guard = DownloadGuard {
            state: Arc::clone(&self.state),
            key,
            pending,
            unpublished: Vec::new(),
        };
        tokio::spawn(async move {
            cache.download(guard, url, headers, cache_ttl_secs).await;
        });
        Ok(request)
    }

    #[allow(dead_code)] // convenience API; sessions reserve before spawning
    pub async fn get_or_download(
        &self,
        url: &str,
        cache_ttl_secs: u32,
        timeout_ms: u32,
        headers: Option<&HashMap<String, String>>,
    ) -> anyhow::Result<CacheLease> {
        let request = self.start_download(url, cache_ttl_secs, timeout_ms, headers)?;
        request.wait().await
    }

    fn reserve_disk(&self) -> anyhow::Result<Arc<DiskReservation>> {
        self.disk_bytes
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |bytes| {
                bytes
                    .checked_add(self.max_download_bytes)
                    .filter(|next| *next <= self.max_cache_bytes)
            })
            .map_err(|_| anyhow::anyhow!("CACHE_FULL"))?;
        Ok(Arc::new(DiskReservation {
            _directory_lock: Arc::clone(&self.directory_lock),
            total: Arc::clone(&self.disk_bytes),
            bytes: AtomicU64::new(self.max_download_bytes),
        }))
    }

    async fn download(
        &self,
        mut guard: DownloadGuard,
        url: String,
        headers: Option<HashMap<String, String>>,
        ttl: u32,
    ) {
        let cancel = guard.pending.cancel.clone();
        let result = tokio::select! {
            _ = cancel.cancelled() => Err(anyhow::anyhow!("download cancelled")),
            result = tokio::time::timeout(Duration::from_secs(60), self.download_file(&mut guard, &url, headers.as_ref())) => {
                result.unwrap_or_else(|_| Err(anyhow::anyhow!("Download timeout including queue time")))
            }
        };
        match result {
            Ok(file) => {
                let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
                if !cancel.is_cancelled() {
                    state.entries.insert(
                        guard.key.clone(),
                        CacheEntry {
                            file: Arc::clone(&file),
                            expires_at: Instant::now() + Duration::from_secs(u64::from(ttl)),
                        },
                    );
                    guard.unpublished.clear();
                    guard
                        .pending
                        .result
                        .send_replace(Some(Ok(CacheLease(file))));
                }
            }
            Err(error) => {
                guard
                    .pending
                    .result
                    .send_replace(Some(Err(error.to_string())));
            }
        }
        drop(guard);
        self.cleanup().await;
    }

    async fn download_file(
        &self,
        guard: &mut DownloadGuard,
        url: &str,
        headers: Option<&HashMap<String, String>>,
    ) -> anyhow::Result<Arc<CachedFile>> {
        let acquired = self.download_semaphore.acquire().await;
        let _active = acquired.map_err(|_| anyhow::anyhow!("download service stopped"))?;
        let reservation = self.reserve_disk()?;
        let path = self.cache_dir.join(format!(
            "rtpbridge-cache-{}{}",
            uuid::Uuid::new_v4(),
            url_extension(url)
        ));
        let cached = Arc::new(CachedFile {
            path: path.clone(),
            key: guard.key.clone(),
            reservation,
        });
        let temporary = Arc::new(CachedFile {
            path: path.with_extension("part"),
            key: guard.key.clone(),
            reservation: Arc::clone(&cached.reservation),
        });
        guard.unpublished = vec![Arc::clone(&temporary), Arc::clone(&cached)];
        let held = Arc::clone(&temporary);
        let opened = crate::storage::run(move || {
            let file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&held.path)?;
            Ok(file)
        })
        .await;
        let mut file = opened?;
        let received = self.policy.response(url, headers).await;
        let mut response = received?;
        if response
            .content_length()
            .is_some_and(|bytes| bytes > self.max_download_bytes)
        {
            anyhow::bail!("File too large");
        }
        let mut bytes = 0u64;
        loop {
            let received = response.chunk().await;
            let Some(chunk) =
                received.map_err(|_| anyhow::anyhow!("failed to read playback body"))?
            else {
                break;
            };
            bytes = bytes
                .checked_add(chunk.len() as u64)
                .filter(|bytes| *bytes <= self.max_download_bytes)
                .ok_or_else(|| anyhow::anyhow!("Downloaded file too large"))?;
            let held = Arc::clone(&temporary);
            let written = crate::storage::run(move || {
                let _held = held; // retain the disk reservation through a cancelled/stalled write
                file.write_all(&chunk)?;
                Ok(file)
            })
            .await;
            file = written?;
        }
        let held = Arc::clone(&temporary);
        let flushed = crate::storage::run(move || {
            let _held = held;
            file.flush()?;
            Ok(())
        })
        .await;
        flushed?;
        let from = Arc::clone(&temporary);
        let to = Arc::clone(&cached);
        let published = crate::storage::run(move || {
            std::fs::rename(&from.path, &to.path)?;
            Ok(())
        })
        .await;
        published?;
        cached.reservation.shrink(bytes);
        Ok(cached)
    }

    pub async fn cleanup(&self) {
        let garbage = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            let now = Instant::now();
            let keys: Vec<_> = state
                .entries
                .iter()
                .filter(|(_, entry)| entry.expires_at <= now && Arc::strong_count(&entry.file) == 1)
                .map(|(key, _)| key.clone())
                .collect();
            for key in keys {
                if let Some(entry) = state.entries.remove(&key) {
                    state.garbage.push(entry.file);
                }
            }
            // Keep every file accounted until deletion is confirmed. Cloning
            // claims prevents concurrent cleanup from selecting the same file;
            // cancelling this future only drops claims, never cache ownership.
            state
                .garbage
                .iter()
                .filter(|file| Arc::strong_count(file) == 1)
                .cloned()
                .collect::<Vec<_>>()
        };
        for file in garbage {
            let held = Arc::clone(&file);
            let removed = crate::storage::run(move || match std::fs::remove_file(&held.path) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(error.into()),
            })
            .await;
            if removed.is_ok() {
                self.state
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .garbage
                    .retain(|entry| !Arc::ptr_eq(entry, &file));
            }
        }
    }

    pub fn start_cleanup_task(
        self: &Arc<Self>,
        interval_secs: u64,
        shutdown: crate::shutdown::ShutdownCoordinator,
    ) -> tokio::task::JoinHandle<()> {
        let cache = Arc::clone(self);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(interval_secs));
            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        tokio::select! {
                            _ = cache.cleanup() => {},
                            _ = shutdown.wait_for_shutdown() => break,
                        }
                    }
                    _ = shutdown.wait_for_shutdown() => break,
                }
            }
            for pending in cache
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .inflight
                .values()
            {
                pending.cancel.cancel();
            }
            let cleanup = async {
                loop {
                    let done = cache
                        .state
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .inflight
                        .is_empty();
                    cache.cleanup().await;
                    if done {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            };
            let _ = tokio::time::timeout(Duration::from_secs(2), cleanup).await;
        })
    }
}
/// Check if a source string is a URL (vs local file path)
pub fn is_url(source: &str) -> bool {
    source.starts_with("http://") || source.starts_with("https://")
}

/// Returns a deterministic cache key for a URL (used for shared playback IDs).
pub fn cache_key(url: &str) -> String {
    url_hash(url)
}

fn url_hash(url: &str) -> String {
    cache_key_hash(url, None)
}

fn hex_lower(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        write!(s, "{b:02x}").unwrap();
    }
    s
}

/// Cache key that incorporates both URL and optional headers.
/// When headers are present (e.g. Authorization), requests with different
/// headers are cached separately to avoid serving wrong content.
fn cache_key_hash(
    url: &str,
    headers: Option<&std::collections::HashMap<String, String>>,
) -> String {
    use sha2::Digest;
    let mut hasher = sha2::Sha256::new();
    hasher.update((url.len() as u64).to_be_bytes());
    hasher.update(url.as_bytes());
    hasher.update((headers.map_or(0, |headers| headers.len()) as u64).to_be_bytes());
    if let Some(hdrs) = headers {
        // Sort keys for deterministic hashing
        let mut pairs: Vec<_> = hdrs.iter().collect();
        pairs.sort_by_key(|(k, _)| *k);
        for (k, v) in pairs {
            hasher.update((k.len() as u64).to_be_bytes());
            hasher.update(k.as_bytes());
            hasher.update((v.len() as u64).to_be_bytes());
            hasher.update(v.as_bytes());
        }
    }
    hex_lower(&hasher.finalize())
}

fn url_extension(url: &str) -> String {
    // Extract extension from URL path (before query params)
    let path = url.split(['?', '#']).next().unwrap_or(url);
    Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .filter(|e| {
            matches!(
                *e,
                "wav" | "mp3" | "ogg" | "oga" | "opus" | "flac" | "aiff" | "aif"
            )
        })
        .map(|e| format!(".{e}"))
        .unwrap_or_else(|| ".bin".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn shutdown_interrupts_a_stalled_periodic_cleanup() {
        let directory = tempfile::tempdir().unwrap();
        let cache = Arc::new(FileCache::new(directory.path().into()).unwrap());
        let path = directory.path().join("expired.wav");
        std::fs::write(&path, b"data").unwrap();
        cache
            .state
            .lock()
            .unwrap()
            .garbage
            .push(fake_file(&cache, path.clone()));
        let blocked = crate::storage::block_workers().await;
        let shutdown = crate::shutdown::ShutdownCoordinator::new();
        let cleanup = cache.start_cleanup_task(60, shutdown.clone());
        tokio::task::yield_now().await;
        shutdown.initiate_shutdown();
        let stopped = tokio::time::timeout(Duration::from_secs(3), cleanup).await;
        let stopped = stopped.expect("periodic cleanup must observe shutdown during storage IO");
        stopped.unwrap();
        assert!(path.exists());
        assert_eq!(cache.state.lock().unwrap().garbage.len(), 1);
        drop(blocked);
    }

    #[tokio::test]
    async fn cancelled_cleanup_keeps_files_and_budgets_tracked() {
        let directory = tempfile::tempdir().unwrap();
        let cache = Arc::new(FileCache::new(directory.path().into()).unwrap());
        let path = directory.path().join("expired.wav");
        std::fs::write(&path, b"data").unwrap();
        let file = fake_file(&cache, path.clone());
        file.reservation.bytes.store(4, Ordering::Relaxed);
        cache.disk_bytes.store(4, Ordering::Relaxed);
        cache.state.lock().unwrap().garbage.push(file);

        let blocked = crate::storage::block_workers().await;
        let held = Arc::clone(&cache);
        let cleanup = tokio::spawn(async move { held.cleanup().await });
        tokio::task::yield_now().await;
        assert!(!cleanup.is_finished());
        cleanup.abort();
        let cancelled = cleanup.await;
        assert!(cancelled.unwrap_err().is_cancelled());
        assert!(path.exists());
        assert_eq!(cache.state.lock().unwrap().garbage.len(), 1);
        assert_eq!(cache.disk_bytes.load(Ordering::Relaxed), 4);

        drop(blocked);
        let retried = tokio::time::timeout(Duration::from_secs(2), async {
            while path.exists() || cache.disk_bytes.load(Ordering::Relaxed) != 0 {
                cache.cleanup().await;
                tokio::task::yield_now().await;
            }
        })
        .await;
        retried.unwrap();
        assert!(cache.state.lock().unwrap().garbage.is_empty());
    }

    fn fake_file(cache: &FileCache, path: PathBuf) -> Arc<CachedFile> {
        Arc::new(CachedFile {
            path,
            key: "fixture".into(),
            reservation: Arc::new(DiskReservation {
                _directory_lock: Arc::clone(&cache.directory_lock),
                total: Arc::clone(&cache.disk_bytes),
                bytes: AtomicU64::new(0),
            }),
        })
    }

    #[test]
    fn cache_key_fields_have_unambiguous_boundaries() {
        let headers = HashMap::from([("Authorization".into(), "secret".into())]);
        assert_ne!(
            cache_key_hash("https://media.test/file", Some(&headers)),
            cache_key_hash("https://media.test/file\0Authorization\0secret", None)
        );
    }

    #[test]
    fn test_is_url() {
        assert!(is_url("https://example.com/file.wav"));
        assert!(is_url("http://example.com/file.mp3"));
        assert!(!is_url("/path/to/file.wav"));
        assert!(!is_url("relative/path.wav"));
    }

    #[test]
    fn test_url_extension() {
        assert_eq!(url_extension("https://example.com/audio.wav"), ".wav");
        assert_eq!(
            url_extension("https://example.com/audio.mp3?token=abc"),
            ".mp3"
        );
        assert_eq!(url_extension("https://example.com/file"), ".bin");
    }

    #[test]
    fn test_url_hash_deterministic() {
        let h1 = url_hash("https://example.com/test.wav");
        let h2 = url_hash("https://example.com/test.wav");
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 64);
    }

    #[test]
    fn test_url_hash_unique_per_url() {
        let h1 = url_hash("https://example.com/a.wav");
        let h2 = url_hash("https://example.com/b.wav");
        assert_ne!(h1, h2, "different URLs should produce different hashes");
    }

    #[tokio::test]
    async fn test_concurrent_download_dedup() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicU32, Ordering};

        // Start a tiny HTTP server that counts requests
        let request_count = Arc::new(AtomicU32::new(0));
        let count_clone = Arc::clone(&request_count);

        let bound = tokio::net::TcpListener::bind("127.0.0.1:0").await;
        let listener = bound.unwrap();
        let server_addr = listener.local_addr().unwrap();

        let server_handle = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let count = Arc::clone(&count_clone);
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut buf = vec![0u8; 4096];
                    let _ = stream.read(&mut buf).await;
                    count.fetch_add(1, Ordering::Relaxed);
                    // Simulate slow download
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    let body = b"fake audio data";
                    let response =
                        format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len());
                    let _ = stream.write_all(response.as_bytes()).await;
                    let _ = stream.write_all(body).await;
                });
            }
        });

        let cache_dir = PathBuf::from("/tmp/rtpbridge-cache-test");
        let _ = std::fs::remove_dir_all(&cache_dir);
        let cache = FileCache::new(cache_dir.clone())
            .unwrap()
            .with_policy(DownloadPolicy::loopback_only());
        let url = format!("http://{}/test.wav", server_addr);

        // Spawn multiple concurrent downloads for the same URL
        let cache = Arc::new(cache);
        let mut handles = Vec::new();
        for _ in 0..5 {
            let c = Arc::clone(&cache);
            let u = url.clone();
            handles.push(tokio::spawn(async move {
                c.get_or_download(&u, 60, 5000, None).await
            }));
        }

        // Wait for all to finish
        let mut successes = 0;
        for h in handles {
            if h.await.unwrap().is_ok() {
                successes += 1;
            }
        }

        // All 5 should have succeeded
        assert_eq!(successes, 5, "all concurrent requests should succeed");

        // Only 1 actual HTTP request should have been made (dedup)
        let actual_requests = request_count.load(Ordering::Relaxed);
        assert_eq!(
            actual_requests, 1,
            "concurrent requests for same URL should result in only 1 download, got {actual_requests}"
        );

        // Cleanup
        server_handle.abort();
        let _ = std::fs::remove_dir_all(&cache_dir);
    }

    #[tokio::test]
    async fn test_cache_cleanup_nonexistent_file_doesnt_corrupt_state() {
        // Simulate a scenario where the cached file was externally deleted
        // (e.g., another process or manual cleanup). Cleanup should still
        // remove the entry from the map without panicking.
        let cache_dir = PathBuf::from("/tmp/rtpbridge-cache-cleanup-missing-test");
        let _ = std::fs::remove_dir_all(&cache_dir);
        std::fs::create_dir_all(&cache_dir).unwrap();
        let cache = FileCache::new(cache_dir.clone())
            .unwrap()
            .with_policy(DownloadPolicy::loopback_only());

        // Insert an entry pointing to a file that doesn't exist
        {
            let mut state = cache.state.lock().unwrap();
            state.entries.insert(
                "ghost_key".to_string(),
                CacheEntry {
                    file: fake_file(&cache, cache_dir.join("nonexistent.wav")),
                    expires_at: std::time::Instant::now() - Duration::from_secs(1),
                },
            );
            // Also insert a valid entry that should survive
            let valid_path = cache_dir.join("valid.wav");
            std::fs::write(&valid_path, b"audio").unwrap();
            state.entries.insert(
                "valid_key".to_string(),
                CacheEntry {
                    file: fake_file(&cache, valid_path),
                    expires_at: std::time::Instant::now() + Duration::from_secs(3600),
                },
            );
        }

        // Cleanup should not panic even though the file is missing
        cache.cleanup().await;

        let state = cache.state.lock().unwrap();
        assert!(
            state.entries.get("ghost_key").is_none(),
            "expired entry with missing file should be removed"
        );
        assert!(
            state.entries.get("valid_key").is_some(),
            "valid non-expired entry should survive"
        );

        drop(state);
        let _ = std::fs::remove_dir_all(&cache_dir);
    }

    #[tokio::test]
    async fn test_cache_cleanup_removes_expired() {
        let cache_dir = PathBuf::from("/tmp/rtpbridge-cache-cleanup-expired-test");
        let _ = std::fs::remove_dir_all(&cache_dir);
        let cache =
            FileCache::with_options(cache_dir.clone(), 1000, 16, 100 * 1024 * 1024).unwrap();

        let fake_path = cache_dir.join("expired_audio.wav");

        // Manually insert a file into the cache dir and a corresponding entry
        // with an already-expired `expires_at`.
        {
            std::fs::write(&fake_path, b"fake audio content").unwrap();
            assert!(fake_path.exists(), "file should exist before cleanup");

            let mut state = cache.state.lock().unwrap();
            state.entries.insert(
                "expired_key".to_string(),
                CacheEntry {
                    file: fake_file(&cache, fake_path.clone()),
                    expires_at: std::time::Instant::now() - Duration::from_secs(60),
                },
            );
        }

        cache.cleanup().await;

        // Verify the entry is removed from the cache state
        let state = cache.state.lock().unwrap();
        assert!(
            state.entries.get("expired_key").is_none(),
            "expired entry should be removed from cache state"
        );
        drop(state);

        // Verify the file is removed from disk
        assert!(
            !fake_path.exists(),
            "expired file should be deleted from disk"
        );

        let _ = std::fs::remove_dir_all(&cache_dir);
    }

    #[test]
    fn test_url_with_path_traversal() {
        // A malicious URL with path traversal should produce a safe filename
        let url = "http://evil.com/../../../etc/passwd";
        let hash = url_hash(url);
        let ext = url_extension(url);
        let filename = format!("{hash}{ext}");

        // The hash-based filename must not contain path separators or traversal sequences
        assert!(
            !filename.contains(".."),
            "filename should not contain '..': {filename}"
        );
        assert!(
            !filename.contains('/'),
            "filename should not contain '/': {filename}"
        );
        assert!(
            !filename.contains('\\'),
            "filename should not contain '\\': {filename}"
        );
        // Hash should be a 16-char hex string
        assert_eq!(hash.len(), 64, "hash should be 40 hex characters");
        assert!(
            hash.chars().all(|c| c.is_ascii_hexdigit()),
            "hash should only contain hex digits: {hash}"
        );
    }

    #[tokio::test]
    async fn test_download_http_404() {
        let bound = tokio::net::TcpListener::bind("127.0.0.1:0").await;
        let listener = bound.unwrap();
        let addr = listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            if let Ok((mut stream, _)) = listener.accept().await {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = vec![0u8; 4096];
                let _ = stream.read(&mut buf).await;
                let response = "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n";
                let _ = stream.write_all(response.as_bytes()).await;
            }
        });

        let cache_dir = PathBuf::from("/tmp/rtpbridge-cache-404-test");
        let _ = std::fs::remove_dir_all(&cache_dir);
        let cache = FileCache::new(cache_dir.clone())
            .unwrap()
            .with_policy(DownloadPolicy::loopback_only());
        let url = format!("http://{}/missing.wav", addr);

        let result = cache.get_or_download(&url, 60, 5000, None).await;
        assert!(result.is_err(), "HTTP 404 should return error");
        assert!(
            result.unwrap_err().to_string().contains("404"),
            "error should mention 404 status"
        );

        server.abort();
        let _ = std::fs::remove_dir_all(&cache_dir);
    }

    #[tokio::test]
    async fn test_download_timeout() {
        let bound = tokio::net::TcpListener::bind("127.0.0.1:0").await;
        let listener = bound.unwrap();
        let addr = listener.local_addr().unwrap();

        // Server accepts but never responds
        let server = tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                // Hold the connection open without responding
                tokio::time::sleep(Duration::from_secs(30)).await;
                drop(stream);
            }
        });

        let cache_dir = PathBuf::from("/tmp/rtpbridge-cache-timeout-test");
        let _ = std::fs::remove_dir_all(&cache_dir);
        let cache = FileCache::new(cache_dir.clone())
            .unwrap()
            .with_policy(DownloadPolicy::loopback_only());
        let url = format!("http://{}/slow.wav", addr);

        let result = cache.get_or_download(&url, 60, 500, None).await; // 500ms timeout
        assert!(result.is_err(), "timeout should return error");

        server.abort();
        let _ = std::fs::remove_dir_all(&cache_dir);
    }

    #[tokio::test]
    async fn test_download_oversized_content_length() {
        let bound = tokio::net::TcpListener::bind("127.0.0.1:0").await;
        let listener = bound.unwrap();
        let addr = listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            if let Ok((mut stream, _)) = listener.accept().await {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = vec![0u8; 4096];
                let _ = stream.read(&mut buf).await;
                // Claim 200MB content length (exceeds 100MB limit)
                let response = "HTTP/1.1 200 OK\r\nContent-Length: 209715200\r\n\r\n";
                let _ = stream.write_all(response.as_bytes()).await;
            }
        });

        let cache_dir = PathBuf::from("/tmp/rtpbridge-cache-oversize-test");
        let _ = std::fs::remove_dir_all(&cache_dir);
        let cache = FileCache::new(cache_dir.clone())
            .unwrap()
            .with_policy(DownloadPolicy::loopback_only());
        let url = format!("http://{}/huge.wav", addr);

        let result = cache.get_or_download(&url, 60, 5000, None).await;
        assert!(result.is_err(), "oversized file should return error");
        assert!(
            result.unwrap_err().to_string().contains("too large"),
            "error should mention file too large"
        );

        server.abort();
        let _ = std::fs::remove_dir_all(&cache_dir);
    }

    #[tokio::test]
    async fn test_concurrent_download_dedup_failure() {
        // When multiple callers request the same URL and the download fails,
        // ALL waiters should receive the error — not hang indefinitely.
        let bound = tokio::net::TcpListener::bind("127.0.0.1:0").await;
        let listener = bound.unwrap();
        let server_addr = listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            if let Ok((mut stream, _)) = listener.accept().await {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = vec![0u8; 4096];
                let _ = stream.read(&mut buf).await;
                // Simulate slow then fail
                tokio::time::sleep(Duration::from_millis(100)).await;
                let response = "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n";
                let _ = stream.write_all(response.as_bytes()).await;
            }
        });

        let cache_dir = PathBuf::from("/tmp/rtpbridge-cache-dedup-fail-test");
        let _ = std::fs::remove_dir_all(&cache_dir);
        let cache = Arc::new(FileCache::new(cache_dir.clone()).unwrap());
        let url = format!("http://{}/fail.wav", server_addr);

        // Spawn 5 concurrent requests for the same failing URL
        let mut handles = Vec::new();
        for _ in 0..5 {
            let c = Arc::clone(&cache);
            let u = url.clone();
            handles.push(tokio::spawn(async move {
                c.get_or_download(&u, 60, 5000, None).await
            }));
        }

        let mut errors = 0;
        for h in handles {
            if h.await.unwrap().is_err() {
                errors += 1;
            }
        }

        assert_eq!(
            errors, 5,
            "all concurrent requests should receive the error, got {} errors",
            errors
        );

        server.abort();
        let _ = std::fs::remove_dir_all(&cache_dir);
    }

    #[tokio::test]
    async fn test_download_http_500() {
        let bound = tokio::net::TcpListener::bind("127.0.0.1:0").await;
        let listener = bound.unwrap();
        let addr = listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            if let Ok((mut stream, _)) = listener.accept().await {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = vec![0u8; 4096];
                let _ = stream.read(&mut buf).await;
                let response = "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n";
                let _ = stream.write_all(response.as_bytes()).await;
            }
        });

        let cache_dir = PathBuf::from("/tmp/rtpbridge-cache-500-test");
        let _ = std::fs::remove_dir_all(&cache_dir);
        let cache = FileCache::new(cache_dir.clone())
            .unwrap()
            .with_policy(DownloadPolicy::loopback_only());
        let url = format!("http://{}/error.wav", addr);

        let result = cache.get_or_download(&url, 60, 5000, None).await;
        assert!(result.is_err(), "HTTP 500 should return error");
        assert!(
            result.unwrap_err().to_string().contains("500"),
            "error should mention 500 status"
        );

        server.abort();
        let _ = std::fs::remove_dir_all(&cache_dir);
    }

    #[test]
    fn test_url_with_no_extension() {
        // A URL with no file extension should get a default extension
        let url = "http://example.com/audio";
        let hash = url_hash(url);
        let ext = url_extension(url);
        let filename = format!("{hash}{ext}");

        // Should produce a valid filename (no empty extension)
        assert!(!filename.is_empty(), "filename should not be empty");
        // When there's no extension, url_extension returns ".bin" as a fallback
        assert!(
            filename.ends_with(".bin"),
            "URL with no extension should get .bin fallback: {filename}"
        );
        assert_eq!(hash.len(), 64, "hash should be 40 hex characters");
    }

    async fn fixture_server() -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
        let bound = tokio::net::TcpListener::bind("127.0.0.1:0").await;
        let listener = bound.unwrap();
        let address = listener.local_addr().unwrap();
        let requests = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&requests);
        let task = tokio::spawn(async move {
            loop {
                let accepted = listener.accept().await;
                let Ok((mut socket, _)) = accepted else {
                    break;
                };
                count.fetch_add(1, Ordering::Relaxed);
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut request = [0; 4096];
                    let _ = socket.read(&mut request).await;
                    let _ = socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\ndata").await;
                });
            }
        });
        (format!("http://{address}/audio.wav"), requests, task)
    }

    #[tokio::test]
    async fn leases_release_exact_header_variants_and_do_not_delete_live_files() {
        let (url, requests, server) = fixture_server().await;
        let directory = tempfile::tempdir().unwrap();
        let cache = FileCache::new(directory.path().into())
            .unwrap()
            .with_policy(DownloadPolicy::loopback_only());
        let headers = HashMap::from([("Authorization".into(), "Bearer fixture".into())]);
        let downloaded = cache.get_or_download(&url, 0, 5000, None).await;
        let plain = downloaded.unwrap();
        let downloaded = cache.get_or_download(&url, 0, 5000, Some(&headers)).await;
        let authenticated = downloaded.unwrap();
        assert_ne!(plain.key(), authenticated.key());
        assert_eq!(requests.load(Ordering::Relaxed), 2);
        let plain_path = plain.to_path_buf();
        let auth_path = authenticated.to_path_buf();
        cache.cleanup().await;
        assert!(plain_path.exists() && auth_path.exists());
        drop(authenticated);
        cache.cleanup().await;
        assert!(plain_path.exists());
        assert!(!auth_path.exists());
        drop(plain);
        cache.cleanup().await;
        assert!(!plain_path.exists());
        assert_eq!(cache.disk_bytes.load(Ordering::Acquire), 0);
        server.abort();
    }

    #[tokio::test]
    async fn cancelled_owner_does_not_cancel_a_shared_download() {
        let (url, requests, server) = fixture_server().await;
        let directory = tempfile::tempdir().unwrap();
        let cache = FileCache::new(directory.path().into())
            .unwrap()
            .with_policy(DownloadPolicy::loopback_only());
        let first = cache.start_download(&url, 0, 5000, None).unwrap();
        let second = cache.start_download(&url, 0, 5000, None).unwrap();
        drop(first);
        let completed = second.wait().await;
        let lease = completed.unwrap();
        assert!(lease.exists());
        assert_eq!(requests.load(Ordering::Relaxed), 1);
        drop(lease);
        cache.cleanup().await;
        assert_eq!(cache.disk_bytes.load(Ordering::Acquire), 0);
        server.abort();
    }

    #[tokio::test]
    async fn admission_and_last_owner_cancellation_bound_queued_work() {
        let directory = tempfile::tempdir().unwrap();
        let mut cache = FileCache::new(directory.path().into())
            .unwrap()
            .with_policy(DownloadPolicy::loopback_only());
        cache.owners = Arc::new(Semaphore::new(1));
        let acquired = cache.download_semaphore.acquire_many(16).await;
        let active = acquired.unwrap();
        let request = cache
            .start_download("http://127.0.0.1:9/audio.wav", 0, 20, None)
            .unwrap();
        assert!(
            cache
                .start_download("http://127.0.0.1:9/other.wav", 0, 20, None)
                .is_err()
        );
        let result = request.wait().await;
        assert!(result.unwrap_err().to_string().contains("timeout"));
        let cancelled = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if cache.state.lock().unwrap().inflight.is_empty() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await;
        assert!(cancelled.is_ok());
        assert_eq!(cache.owners.available_permits(), 1);
        assert_eq!(cache.disk_bytes.load(Ordering::Acquire), 0);
        drop(active);
    }
}
