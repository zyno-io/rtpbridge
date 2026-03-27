use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{Mutex, Semaphore, broadcast};
use tracing::{debug, info, warn};

/// Manages a cache of downloaded URL files with atime-based TTL.
pub struct FileCache {
    cache_dir: PathBuf,
    state: Arc<Mutex<CacheState>>,
    /// Maximum number of cached entries. 0 = unlimited.
    max_entries: usize,
    /// Limits concurrent HTTP downloads to prevent file descriptor exhaustion.
    download_semaphore: Arc<Semaphore>,
    /// Maximum file download size in bytes.
    max_download_bytes: u64,
    /// Reusable HTTP client for connection pooling across downloads.
    http_client: reqwest::Client,
}

struct CacheState {
    /// URL hash → cache entry
    entries: HashMap<String, CacheEntry>,
    /// In-flight downloads: URL hash → broadcast sender that signals completion.
    /// Concurrent requests for the same URL await the broadcast instead of downloading again.
    inflight: HashMap<String, broadcast::Sender<Result<PathBuf, String>>>,
}

struct CacheEntry {
    path: PathBuf,
    /// When this entry should expire (if no references)
    expires_at: std::time::Instant,
    /// Number of active references (playback endpoints using this file)
    ref_count: u32,
}

impl FileCache {
    /// Convenience constructor with default options (1000 entries, 16 concurrent, 100MB max).
    #[allow(dead_code)]
    pub fn new(cache_dir: PathBuf) -> anyhow::Result<Self> {
        Self::with_options(cache_dir, 1000, 16, 100 * 1024 * 1024)
    }

    pub fn with_options(
        cache_dir: PathBuf,
        max_entries: usize,
        max_concurrent_downloads: usize,
        max_download_bytes: u64,
    ) -> anyhow::Result<Self> {
        // Create cache dir if it doesn't exist — fail loudly so callers
        // get a clear error instead of confusing download failures later.
        std::fs::create_dir_all(&cache_dir).map_err(|e| {
            anyhow::anyhow!(
                "Failed to create cache directory '{}': {e}",
                cache_dir.display()
            )
        })?;

        // Clean up orphaned files from previous runs (crash recovery).
        // Only removes regular files; subdirectories are left untouched.
        if let Ok(entries) = std::fs::read_dir(&cache_dir) {
            for entry in entries.flatten() {
                if entry.path().is_file() {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }

        Ok(Self {
            cache_dir,
            state: Arc::new(Mutex::new(CacheState {
                entries: HashMap::new(),
                inflight: HashMap::new(),
            })),
            max_entries,
            download_semaphore: Arc::new(Semaphore::new(max_concurrent_downloads)),
            max_download_bytes,
            http_client: reqwest::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(10))
                .redirect(reqwest::redirect::Policy::limited(5))
                .build()
                .map_err(|e| anyhow::anyhow!("Failed to build HTTP client: {e}"))?,
        })
    }

    /// Get a cached file path for a URL, downloading if necessary.
    /// Returns the local file path.
    pub async fn get_or_download(
        &self,
        url: &str,
        cache_ttl_secs: u32,
        timeout_ms: u32,
    ) -> anyhow::Result<PathBuf> {
        let key = url_hash(url);

        // Check if already cached or in-flight
        let maybe_rx = {
            let mut state = self.state.lock().await;
            if let Some(entry) = state.entries.get_mut(&key) {
                entry.ref_count += 1;
                entry.expires_at =
                    std::time::Instant::now() + Duration::from_secs(cache_ttl_secs as u64);
                return Ok(entry.path.clone());
            }
            // Check for in-flight download
            if let Some(tx) = state.inflight.get(&key) {
                Some(tx.subscribe())
            } else {
                // Register ourselves as the downloader
                let (tx, _) = broadcast::channel(4);
                state.inflight.insert(key.clone(), tx);
                None
            }
        };

        // If another task is already downloading, wait for it
        if let Some(mut rx) = maybe_rx {
            return match rx.recv().await {
                Ok(Ok(path)) => {
                    let mut state = self.state.lock().await;
                    if let Some(entry) = state.entries.get_mut(&key) {
                        entry.ref_count += 1;
                    } else {
                        // Entry was evicted between broadcast and our lock acquisition.
                        // Re-insert so the file is tracked and won't be cleaned up under us.
                        state.entries.insert(
                            key,
                            CacheEntry {
                                path: path.clone(),
                                expires_at: std::time::Instant::now()
                                    + Duration::from_secs(cache_ttl_secs as u64),
                                ref_count: 1,
                            },
                        );
                    }
                    Ok(path)
                }
                Ok(Err(e)) => Err(anyhow::anyhow!("{e}")),
                Err(_) => Err(anyhow::anyhow!("Download notification channel closed")),
            };
        }

        // We are the downloader — acquire semaphore to limit concurrency
        let _permit = match self.download_semaphore.acquire().await {
            Ok(permit) => permit,
            Err(_) => {
                // Clean up inflight entry so future requests don't hang
                let mut state = self.state.lock().await;
                state.inflight.remove(&key);
                return Err(anyhow::anyhow!("Download semaphore closed"));
            }
        };

        let ext = url_extension(url);
        let filename = format!("{key}{ext}");
        let path = self.cache_dir.join(&filename);

        info!(url = url, path = %path.display(), "downloading file to cache");

        // Wrap the entire download (connect + headers + body) in a single timeout
        // to prevent slow/dribbling servers from blocking indefinitely.
        let max_dl_bytes = self.max_download_bytes;
        let timeout = Duration::from_millis(timeout_ms as u64);
        let result = tokio::time::timeout(timeout, async {
            let mut response = self
                .http_client
                .get(url)
                .send()
                .await
                .map_err(|e| anyhow::anyhow!("Download failed: {e}"))?;

            if !response.status().is_success() {
                anyhow::bail!("Download failed with status {}", response.status());
            }

            // Check content length if available
            let max_size = max_dl_bytes;
            if let Some(len) = response.content_length() {
                if len > max_size {
                    anyhow::bail!("File too large: {len} bytes (max {max_size})");
                }
            }

            // Stream the body with a size limit to prevent OOM when Content-Length is absent.
            // Pre-allocate using Content-Length when available (capped to max_size).
            let initial_capacity = response
                .content_length()
                .map(|len| len.min(max_size) as usize)
                .unwrap_or(0);
            let mut body_bytes = Vec::with_capacity(initial_capacity);
            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(|e| anyhow::anyhow!("Failed to read response body: {e}"))?
            {
                body_bytes.extend_from_slice(&chunk);
                if body_bytes.len() as u64 > max_size {
                    anyhow::bail!("Downloaded file too large (max {max_size} bytes)");
                }
            }

            tokio::fs::write(&path, &body_bytes).await?;
            Ok(path.clone())
        })
        .await
        .unwrap_or_else(|_| Err(anyhow::anyhow!("Download timeout after {timeout_ms}ms")));

        // Clean up orphaned file on failure: if the download wrote a file but
        // then timed out or failed after writing, delete it so it doesn't
        // accumulate outside the cache's tracking.
        if result.is_err() {
            let _ = tokio::fs::remove_file(&path).await;
        }

        // Notify waiters and clean up inflight entry
        {
            let mut state = self.state.lock().await;
            if let Some(tx) = state.inflight.remove(&key) {
                let _ = tx.send(
                    result
                        .as_ref()
                        .map(|p| p.clone())
                        .map_err(|e| e.to_string()),
                );
            }
            if let Ok(ref p) = result {
                state.entries.insert(
                    key,
                    CacheEntry {
                        path: p.clone(),
                        expires_at: std::time::Instant::now()
                            + Duration::from_secs(cache_ttl_secs as u64),
                        ref_count: 1,
                    },
                );
            }
        }

        result
    }

    /// Release a reference to a cached file
    pub async fn release(&self, url: &str) {
        let key = url_hash(url);
        let mut state = self.state.lock().await;
        if let Some(entry) = state.entries.get_mut(&key) {
            entry.ref_count = entry.ref_count.saturating_sub(1);
        }
    }

    /// Run cleanup: remove expired entries with no active references.
    /// Also evicts oldest unreferenced entries if the cache exceeds max_entries.
    /// Call this periodically (e.g., every 5 minutes).
    pub async fn cleanup(&self) {
        // Collect paths to delete while holding the lock, then delete outside it
        // to avoid blocking get_or_download/release during slow filesystem I/O.
        let paths_to_delete = {
            let now = std::time::Instant::now();
            let mut state = self.state.lock().await;
            let mut paths = Vec::new();

            // Remove expired entries
            let expired: Vec<String> = state
                .entries
                .iter()
                .filter(|(_, e)| e.ref_count == 0 && e.expires_at <= now)
                .map(|(k, _)| k.clone())
                .collect();

            for key in expired {
                if let Some(entry) = state.entries.remove(&key) {
                    debug!(path = %entry.path.display(), "removing expired cache entry");
                    paths.push(entry.path);
                }
            }

            // Evict oldest unreferenced entries if over max size
            if self.max_entries > 0 && state.entries.len() > self.max_entries {
                let mut evictable: Vec<(String, std::time::Instant)> = state
                    .entries
                    .iter()
                    .filter(|(_, e)| e.ref_count == 0)
                    .map(|(k, e)| (k.clone(), e.expires_at))
                    .collect();
                evictable.sort_by_key(|(_, exp)| *exp);

                let to_evict = state.entries.len() - self.max_entries;
                for (key, _) in evictable.into_iter().take(to_evict) {
                    if let Some(entry) = state.entries.remove(&key) {
                        debug!(path = %entry.path.display(), "evicting cache entry (over max size)");
                        paths.push(entry.path);
                    }
                }
            }

            paths
        }; // lock dropped here

        for path in paths_to_delete {
            if let Err(e) = tokio::fs::remove_file(&path).await {
                warn!(path = %path.display(), error = %e, "failed to remove cache file");
            }
        }
    }

    /// Start a background cleanup task that stops on shutdown.
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
                        cache.cleanup().await;
                    }
                    _ = shutdown.wait_for_shutdown() => {
                        debug!("file cache cleanup task stopping due to shutdown");
                        break;
                    }
                }
            }
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
    use sha1::Digest;
    let hash = sha1::Sha1::digest(url.as_bytes());
    format!("{hash:040x}")
}

fn url_extension(url: &str) -> String {
    // Extract extension from URL path (before query params)
    let path = url.split(['?', '#']).next().unwrap_or(url);
    Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| format!(".{e}"))
        .unwrap_or_else(|| ".bin".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(h1.len(), 40);
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

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
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
        let cache = FileCache::new(cache_dir.clone()).unwrap();
        let url = format!("http://{}/test.wav", server_addr);

        // Spawn multiple concurrent downloads for the same URL
        let cache = Arc::new(cache);
        let mut handles = Vec::new();
        for _ in 0..5 {
            let c = Arc::clone(&cache);
            let u = url.clone();
            handles.push(tokio::spawn(async move {
                c.get_or_download(&u, 60, 5000).await
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
        let cache = FileCache::new(cache_dir.clone()).unwrap();

        // Insert an entry pointing to a file that doesn't exist
        {
            let mut state = cache.state.lock().await;
            state.entries.insert(
                "ghost_key".to_string(),
                CacheEntry {
                    path: cache_dir.join("nonexistent.wav"),
                    expires_at: std::time::Instant::now() - Duration::from_secs(1),
                    ref_count: 0,
                },
            );
            // Also insert a valid entry that should survive
            let valid_path = cache_dir.join("valid.wav");
            std::fs::write(&valid_path, b"audio").unwrap();
            state.entries.insert(
                "valid_key".to_string(),
                CacheEntry {
                    path: valid_path,
                    expires_at: std::time::Instant::now() + Duration::from_secs(3600),
                    ref_count: 0,
                },
            );
        }

        // Cleanup should not panic even though the file is missing
        cache.cleanup().await;

        let state = cache.state.lock().await;
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

            let mut state = cache.state.lock().await;
            state.entries.insert(
                "expired_key".to_string(),
                CacheEntry {
                    path: fake_path.clone(),
                    expires_at: std::time::Instant::now() - Duration::from_secs(60),
                    ref_count: 0,
                },
            );
        }

        cache.cleanup().await;

        // Verify the entry is removed from the cache state
        let state = cache.state.lock().await;
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
        assert_eq!(hash.len(), 40, "hash should be 40 hex characters");
        assert!(
            hash.chars().all(|c| c.is_ascii_hexdigit()),
            "hash should only contain hex digits: {hash}"
        );
    }

    #[tokio::test]
    async fn test_download_http_404() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
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
        let cache = FileCache::new(cache_dir.clone()).unwrap();
        let url = format!("http://{}/missing.wav", addr);

        let result = cache.get_or_download(&url, 60, 5000).await;
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
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
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
        let cache = FileCache::new(cache_dir.clone()).unwrap();
        let url = format!("http://{}/slow.wav", addr);

        let result = cache.get_or_download(&url, 60, 500).await; // 500ms timeout
        assert!(result.is_err(), "timeout should return error");

        server.abort();
        let _ = std::fs::remove_dir_all(&cache_dir);
    }

    #[tokio::test]
    async fn test_download_oversized_content_length() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
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
        let cache = FileCache::new(cache_dir.clone()).unwrap();
        let url = format!("http://{}/huge.wav", addr);

        let result = cache.get_or_download(&url, 60, 5000).await;
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
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
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
                c.get_or_download(&u, 60, 5000).await
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
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
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
        let cache = FileCache::new(cache_dir.clone()).unwrap();
        let url = format!("http://{}/error.wav", addr);

        let result = cache.get_or_download(&url, 60, 5000).await;
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
        assert_eq!(hash.len(), 40, "hash should be 40 hex characters");
    }
}
