#![allow(dead_code)]

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;

use rtpbridge::config::Config;
use rtpbridge::metrics::Metrics;
use rtpbridge::playback::file_cache::FileCache;
use rtpbridge::session::SessionManager;
use rtpbridge::shutdown::ShutdownCoordinator;
use tempfile::TempDir;

/// In-process test server — no `cargo run` subprocess needed.
/// Starts a real rtpbridge WebSocket server on a random port within the
/// current tokio runtime. Supports parallel test execution.
pub struct TestServer {
    pub addr: String,
    pub media_dir: Option<String>,
    pub recording_dir: String,
    shutdown: ShutdownCoordinator,
    server_task: tokio::task::JoinHandle<()>,
    /// Temp dirs kept alive for the server's lifetime; cleaned up on drop.
    _temp_dirs: Vec<TempDir>,
}

/// Builder for configuring a TestServer before starting it.
pub struct TestServerBuilder {
    media_dir: Option<String>,
    recording_dir: Option<String>,
    max_sessions: usize,
    max_endpoints_per_session: usize,
    disconnect_timeout_secs: u64,
    rtp_port_range: (u16, u16),
    session_idle_timeout_secs: u64,
    max_recordings_per_session: usize,
    max_connections: usize,
    ws_ping_interval_secs: u64,
    max_file_download_bytes: u64,
    empty_session_timeout_secs: u64,
}

impl Default for TestServerBuilder {
    fn default() -> Self {
        Self {
            media_dir: None,
            recording_dir: None, // auto-creates a tempdir
            max_sessions: 0,
            max_endpoints_per_session: 0,
            disconnect_timeout_secs: 30,
            rtp_port_range: (30000, 39999),
            session_idle_timeout_secs: 0,
            max_recordings_per_session: 100,
            max_connections: 1000,
            ws_ping_interval_secs: 30,
            max_file_download_bytes: 100 * 1024 * 1024,
            empty_session_timeout_secs: 0,
        }
    }
}

impl TestServerBuilder {
    pub fn media_dir(mut self, dir: &str) -> Self {
        std::fs::create_dir_all(dir).ok();
        self.media_dir = Some(dir.to_string());
        self
    }

    pub fn recording_dir(mut self, dir: &str) -> Self {
        self.recording_dir = Some(dir.to_string());
        self
    }

    pub fn max_sessions(mut self, n: usize) -> Self {
        self.max_sessions = n;
        self
    }

    pub fn max_endpoints_per_session(mut self, n: usize) -> Self {
        self.max_endpoints_per_session = n;
        self
    }

    pub fn disconnect_timeout_secs(mut self, secs: u64) -> Self {
        self.disconnect_timeout_secs = secs;
        self
    }

    pub fn rtp_port_range(mut self, start: u16, end: u16) -> Self {
        self.rtp_port_range = (start, end);
        self
    }

    pub fn session_idle_timeout_secs(mut self, secs: u64) -> Self {
        self.session_idle_timeout_secs = secs;
        self
    }

    pub fn max_recordings_per_session(mut self, n: usize) -> Self {
        self.max_recordings_per_session = n;
        self
    }

    pub fn max_connections(mut self, n: usize) -> Self {
        self.max_connections = n;
        self
    }

    pub fn ws_ping_interval_secs(mut self, secs: u64) -> Self {
        self.ws_ping_interval_secs = secs;
        self
    }

    pub fn max_file_download_bytes(mut self, bytes: u64) -> Self {
        self.max_file_download_bytes = bytes;
        self
    }

    pub fn empty_session_timeout_secs(mut self, secs: u64) -> Self {
        self.empty_session_timeout_secs = secs;
        self
    }

    pub async fn start(self) -> TestServer {
        // Extract builder fields so they can be reused across retry attempts
        let media_dir = self.media_dir;
        let explicit_recording_dir = self.recording_dir;
        let rtp_port_range = self.rtp_port_range;
        let disconnect_timeout_secs = self.disconnect_timeout_secs;
        let max_sessions = self.max_sessions;
        let max_endpoints_per_session = self.max_endpoints_per_session;
        let max_recordings_per_session = self.max_recordings_per_session;
        let session_idle_timeout_secs = self.session_idle_timeout_secs;
        let empty_session_timeout_secs = self.empty_session_timeout_secs;
        let builder_max_connections = self.max_connections;
        let builder_ws_ping_interval_secs = self.ws_ping_interval_secs;
        let builder_max_file_download_bytes = self.max_file_download_bytes;

        const MAX_ATTEMPTS: usize = 3;
        for attempt in 0..MAX_ATTEMPTS {
            let port = portpicker::pick_unused_port().expect("no free port");
            let listen_addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
            let addr = format!("127.0.0.1:{port}");

            let mut temp_dirs = Vec::new();

            // Recording dir: use explicit value or auto-create a tempdir
            let recording_dir = match explicit_recording_dir {
                Some(ref dir) => dir.clone(),
                None => {
                    let td = TempDir::new().expect("failed to create temp recording dir");
                    let path = td.path().to_str().unwrap().to_string();
                    temp_dirs.push(td);
                    path
                }
            };

            // Cache dir: always a fresh tempdir
            let cache_td = TempDir::new().expect("failed to create temp cache dir");
            let cache_dir = cache_td.path().to_path_buf();
            temp_dirs.push(cache_td);

            let config = Config {
                listen: vec![listen_addr],
                media_ip: IpAddr::V4(Ipv4Addr::LOCALHOST),
                rtp_port_range,
                disconnect_timeout_secs,
                shutdown_max_wait_secs: 86400,
                media_dir: media_dir.as_ref().map(PathBuf::from),
                recording_dir: PathBuf::from(&recording_dir),
                cache_dir,
                cache_cleanup_interval_secs: 300,
                max_concurrent_downloads: 16,
                max_sessions,
                max_endpoints_per_session,
                max_recordings_per_session,
                recording_flush_timeout_secs: 10,
                ws_max_message_size_kb: 256,
                max_sdp_size_kb: 64,
                session_idle_timeout_secs,
                empty_session_timeout_secs,
                media_timeout_secs: 5,
                max_connections: builder_max_connections,
                ws_ping_interval_secs: builder_ws_ping_interval_secs,
                event_channel_size: 256,
                critical_event_channel_size: 64,
                transcode_cache_size: 64,
                max_file_download_bytes: builder_max_file_download_bytes,
                max_recording_download_bytes: 512 * 1024 * 1024,
                recording_channel_size: 1000,
                log_level: "warn".to_string(),
            };

            let shutdown = ShutdownCoordinator::new();
            let metrics = Arc::new(Metrics::new());
            let file_cache = Arc::new(FileCache::new(config.cache_dir.clone()).unwrap());
            file_cache.start_cleanup_task(config.cache_cleanup_interval_secs, shutdown.clone());

            let manager = SessionManager::new(
                shutdown.clone(),
                config.disconnect_timeout_secs,
                config.media_ip,
                config.rtp_port_range,
                config.max_sessions,
                config.max_endpoints_per_session,
                config.max_recordings_per_session,
                config.recording_flush_timeout_secs,
                config.recording_channel_size,
                config.max_sdp_size_kb,
                config.session_idle_timeout_secs,
                config.empty_session_timeout_secs,
                config.media_timeout_secs,
                config.transcode_cache_size,
                config.media_dir.clone(),
                file_cache,
                Arc::clone(&metrics),
                config.recording_dir.clone(),
            )
            .expect("SessionManager::new failed");

            let ws_recording_dir = config.recording_dir.clone();
            let ws_max_message_size_kb = config.ws_max_message_size_kb;
            let server_shutdown = shutdown.clone();
            let max_connections = config.max_connections;
            let max_recording_download_bytes = config.max_recording_download_bytes;
            let ws_ping_interval_secs = config.ws_ping_interval_secs;
            let event_channel_size = config.event_channel_size;
            let critical_event_channel_size = config.critical_event_channel_size;
            let server_task = tokio::spawn(async move {
                let _ = rtpbridge::control::server::run_websocket_server(
                    vec![listen_addr],
                    manager,
                    server_shutdown,
                    metrics,
                    ws_recording_dir,
                    ws_max_message_size_kb,
                    max_connections,
                    max_recording_download_bytes,
                    ws_ping_interval_secs,
                    event_channel_size,
                    critical_event_channel_size,
                )
                .await;
            });

            // Wait for the server to be listening (poll connect instead of fixed sleep)
            let url = format!("ws://127.0.0.1:{port}");
            let mut started = false;
            for i in 0..40 {
                match tokio_tungstenite::connect_async(&url).await {
                    Ok((ws, _)) => {
                        drop(ws);
                        started = true;
                        break;
                    }
                    Err(_) if i < 39 => {
                        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    }
                    Err(_) => {} // probe exhausted for this attempt
                }
            }

            if started {
                return TestServer {
                    addr,
                    media_dir: media_dir.clone(),
                    recording_dir,
                    shutdown,
                    server_task,
                    _temp_dirs: temp_dirs,
                };
            }

            // Cleanup failed attempt before retrying
            shutdown.initiate_shutdown();
            server_task.abort();

            if attempt < MAX_ATTEMPTS - 1 {
                eprintln!(
                    "TestServer failed to start on port {port} (attempt {}/{}), retrying...",
                    attempt + 1,
                    MAX_ATTEMPTS
                );
            } else {
                panic!(
                    "TestServer failed to start after {MAX_ATTEMPTS} attempts (port race or bind failure)"
                );
            }
        }
        unreachable!()
    }
}

impl TestServer {
    /// Start a server with default config (no media_dir, auto tempdir for recording).
    pub async fn start() -> Self {
        TestServerBuilder::default().start().await
    }

    /// Return a builder for custom configuration.
    pub fn builder() -> TestServerBuilder {
        TestServerBuilder::default()
    }

    /// Initiate graceful shutdown (for tests that need to verify shutdown behavior).
    pub fn initiate_shutdown(&self) {
        self.shutdown.initiate_shutdown();
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.shutdown.initiate_shutdown();
        self.server_task.abort();
    }
}
