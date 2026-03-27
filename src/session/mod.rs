pub mod endpoint;
pub mod endpoint_bridge;
pub mod endpoint_enum;
pub mod endpoint_file;
pub mod endpoint_rtp;
pub mod endpoint_tone;
pub mod endpoint_webrtc;
pub mod file_poll;
pub mod media_session;
pub mod mixer;
pub mod routing;
pub mod session_dtmf;
pub mod stats;
pub mod tone_poll;
pub mod vad_tap;

use dashmap::DashMap;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use tokio::sync::mpsc;
use tracing::{Instrument, warn};

use crate::control::protocol::*;
use crate::metrics::Metrics;
use crate::net::socket_pool::SocketPool;
use crate::playback::file_cache::FileCache;
use crate::shutdown::ShutdownCoordinator;

pub use media_session::SessionCommand;

/// A managed media session
pub struct Session {
    pub id: SessionId,
    pub state: SessionState,
    pub created_at_wall: String,
    /// Channel to send commands to the session task
    pub cmd_tx: mpsc::Sender<SessionCommand>,
    /// Disconnect timeout handle (if orphaned)
    pub orphan_timeout: Option<tokio::task::JoinHandle<()>>,
    /// Current endpoint count, updated atomically by the media session task
    pub endpoint_count: Arc<AtomicUsize>,
    /// Handle to the media session task (aborted on force-destroy)
    pub task_handle: Option<tokio::task::JoinHandle<()>>,
}

/// Manages all sessions on this server
pub struct SessionManager {
    sessions: DashMap<SessionId, Session>,
    shutdown: ShutdownCoordinator,
    disconnect_timeout_secs: u64,
    media_ip: IpAddr,
    socket_pool: Arc<SocketPool>,
    max_sessions: usize,
    max_endpoints_per_session: usize,
    max_recordings_per_session: usize,
    recording_flush_timeout_secs: u64,
    recording_channel_size: usize,
    max_sdp_size_kb: usize,
    session_idle_timeout_secs: u64,
    empty_session_timeout_secs: u64,
    media_timeout_secs: u64,
    transcode_cache_size: usize,
    media_dir: Option<PathBuf>,
    file_cache: Arc<FileCache>,
    metrics: Arc<Metrics>,
    shared_playback: Arc<crate::playback::shared_playback::SharedPlaybackManager>,
    recording_dir: PathBuf,
    /// Atomic session count for race-free max_sessions enforcement
    session_count: AtomicUsize,
}

impl SessionManager {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        shutdown: ShutdownCoordinator,
        disconnect_timeout_secs: u64,
        media_ip: IpAddr,
        rtp_port_range: (u16, u16),
        max_sessions: usize,
        max_endpoints_per_session: usize,
        max_recordings_per_session: usize,
        recording_flush_timeout_secs: u64,
        recording_channel_size: usize,
        max_sdp_size_kb: usize,
        session_idle_timeout_secs: u64,
        empty_session_timeout_secs: u64,
        media_timeout_secs: u64,
        transcode_cache_size: usize,
        media_dir: Option<PathBuf>,
        file_cache: Arc<FileCache>,
        metrics: Arc<Metrics>,
        recording_dir: PathBuf,
    ) -> anyhow::Result<Arc<Self>> {
        let socket_pool = Arc::new(SocketPool::new(
            media_ip,
            rtp_port_range.0,
            rtp_port_range.1,
        )?);
        Ok(Arc::new(Self {
            sessions: DashMap::new(),
            shutdown,
            disconnect_timeout_secs,
            media_ip,
            socket_pool,
            max_sessions,
            max_endpoints_per_session,
            max_recordings_per_session,
            recording_flush_timeout_secs,
            recording_channel_size,
            max_sdp_size_kb,
            session_idle_timeout_secs,
            empty_session_timeout_secs,
            media_timeout_secs,
            transcode_cache_size,
            media_dir,
            file_cache,
            metrics,
            shared_playback: Arc::new(
                crate::playback::shared_playback::SharedPlaybackManager::new(),
            ),
            recording_dir,
            session_count: AtomicUsize::new(0),
        }))
    }

    pub fn recording_dir(&self) -> &Path {
        &self.recording_dir
    }

    /// Maximum SDP size in bytes (derived from max_sdp_size_kb config)
    pub fn max_sdp_size(&self) -> usize {
        self.max_sdp_size_kb * 1024
    }

    pub fn create_session(self: &Arc<Self>) -> Result<SessionId, &'static str> {
        // Atomically reserve a session slot to prevent races
        if self.max_sessions > 0 {
            loop {
                let current = self.session_count.load(Ordering::Acquire);
                if current >= self.max_sessions {
                    return Err("MAX_SESSIONS_REACHED");
                }
                if self
                    .session_count
                    .compare_exchange(current, current + 1, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
                {
                    break;
                }
            }
        } else {
            self.session_count.fetch_add(1, Ordering::AcqRel);
        }

        let id = SessionId::new_v4();
        let (cmd_tx, cmd_rx) = mpsc::channel::<SessionCommand>(64);
        let cmd_tx_clone = cmd_tx.clone();
        let endpoint_count = Arc::new(AtomicUsize::new(0));
        let endpoint_count_session = Arc::clone(&endpoint_count);

        self.shutdown.session_started();
        self.metrics.sessions_total.inc();
        self.metrics.sessions_active.inc();

        // Spawn the media session task
        let manager = Arc::clone(self);
        let session_id = id;
        let media_ip = self.media_ip;
        let socket_pool = Arc::clone(&self.socket_pool);
        let media_dir = self.media_dir.clone();
        let file_cache = Arc::clone(&self.file_cache);
        let max_endpoints = self.max_endpoints_per_session;
        let max_recordings = self.max_recordings_per_session;
        let recording_flush_timeout_secs = self.recording_flush_timeout_secs;
        let recording_channel_size = self.recording_channel_size;
        let idle_timeout_secs = self.session_idle_timeout_secs;
        let empty_timeout_secs = self.empty_session_timeout_secs;
        let media_timeout_secs = self.media_timeout_secs;
        let transcode_cache_size = self.transcode_cache_size;
        let metrics = Arc::clone(&self.metrics);
        let shared_playback = Arc::clone(&self.shared_playback);
        let session_span = tracing::info_span!("session", %session_id);
        // Drop guard ensures session cleanup runs even if the media task panics.
        // On normal completion, the guard runs when it goes out of scope after
        // run_media_session returns. On panic, the guard runs during unwinding.
        struct SessionCleanupGuard<F: FnOnce()>(Option<F>);
        impl<F: FnOnce()> Drop for SessionCleanupGuard<F> {
            fn drop(&mut self) {
                if let Some(f) = self.0.take() {
                    f();
                }
            }
        }

        let task_handle = tokio::spawn(
            async move {
                let _guard = SessionCleanupGuard(Some(|| {
                    let ep_count = endpoint_count.load(std::sync::atomic::Ordering::Acquire);
                    for _ in 0..ep_count {
                        metrics.endpoints_active.dec();
                    }
                    manager.sessions.remove(&session_id);
                    manager.session_count.fetch_sub(1, Ordering::AcqRel);
                    manager.shutdown.session_ended();
                    metrics.sessions_active.dec();
                }));
                media_session::run_media_session(
                    session_id,
                    media_ip,
                    socket_pool,
                    media_dir,
                    file_cache,
                    endpoint_count.clone(),
                    max_endpoints,
                    max_recordings,
                    recording_flush_timeout_secs,
                    recording_channel_size,
                    idle_timeout_secs,
                    empty_timeout_secs,
                    media_timeout_secs,
                    transcode_cache_size,
                    Arc::clone(&metrics),
                    shared_playback,
                    cmd_tx_clone,
                    cmd_rx,
                )
                .await;
            }
            .instrument(session_span),
        );

        let session = Session {
            id,
            state: SessionState::Active,
            created_at_wall: chrono_now(),
            cmd_tx,
            orphan_timeout: None,
            endpoint_count: endpoint_count_session,
            task_handle: Some(task_handle),
        };

        self.sessions.insert(id, session);

        Ok(id)
    }

    pub fn destroy_session(&self, id: &SessionId) -> Result<(), &'static str> {
        if let Some(entry) = self.sessions.get(id) {
            if entry.cmd_tx.try_send(SessionCommand::Destroy).is_err() {
                // Channel full or closed — force-remove the session from the map
                // so it doesn't leak in the DashMap forever.
                warn!(session_id = %id, "destroy command channel full/closed, force-removing session");
                drop(entry);
                // Remove and abort the media task so it doesn't run as a zombie.
                // Only decrement counters if the task hasn't already finished
                // (if it has, the on_done closure already did the cleanup).
                if let Some((_, mut session)) = self.sessions.remove(id) {
                    // The SessionCleanupGuard on the spawned task handles counter
                    // cleanup. If the task already finished, the guard already ran.
                    // If it's still running, aborting it triggers the guard's Drop.
                    // We only need manual cleanup when there's no task handle at all.
                    let task_finished = session
                        .task_handle
                        .as_ref()
                        .is_some_and(|h| h.is_finished());
                    if let Some(handle) = session.task_handle.take() {
                        if !task_finished {
                            handle.abort();
                            // Guard's Drop fires on abort and handles cleanup.
                            // The guard's sessions.remove will be a no-op since
                            // we already removed the entry above.
                        }
                    } else {
                        // No task handle — shouldn't happen, but manually clean up
                        let ep_count = session
                            .endpoint_count
                            .load(std::sync::atomic::Ordering::Acquire);
                        for _ in 0..ep_count {
                            self.metrics.endpoints_active.dec();
                        }
                        self.session_count.fetch_sub(1, Ordering::AcqRel);
                        self.shutdown.session_ended();
                        self.metrics.sessions_active.dec();
                    }
                }
                return Ok(());
            }
            if let Some(ref handle) = entry.orphan_timeout {
                handle.abort();
            }
            Ok(())
        } else {
            Err("SESSION_NOT_FOUND")
        }
    }

    pub fn attach_session(
        &self,
        id: &SessionId,
        event_tx: mpsc::Sender<Event>,
        critical_event_tx: mpsc::Sender<Event>,
        dropped_events: Arc<AtomicU64>,
    ) -> Result<mpsc::Sender<SessionCommand>, &'static str> {
        let mut entry = self.sessions.get_mut(id).ok_or("SESSION_NOT_FOUND")?;

        if entry.state != SessionState::Orphaned {
            return Err("SESSION_NOT_ORPHANED");
        }

        if let Some(handle) = entry.orphan_timeout.take() {
            handle.abort();
        }

        entry.state = SessionState::Active;
        let cmd_tx = entry.cmd_tx.clone();
        if cmd_tx
            .try_send(SessionCommand::Attach {
                event_tx,
                critical_event_tx,
                dropped_events,
            })
            .is_err()
        {
            entry.state = SessionState::Orphaned;
            return Err("SESSION_BUSY");
        }

        Ok(cmd_tx)
    }

    pub fn detach_session(self: &Arc<Self>, id: &SessionId) {
        if let Some(mut entry) = self.sessions.get_mut(id) {
            if entry.cmd_tx.try_send(SessionCommand::Detach).is_err() {
                warn!(session_id = %id, "detach command dropped (channel full/closed)");
            }
            entry.state = SessionState::Orphaned;

            let session_id = *id;
            let manager = Arc::clone(self);
            let timeout_secs = self.disconnect_timeout_secs;
            let shutdown = self.shutdown.clone();

            let handle = tokio::spawn(async move {
                // Wake immediately on shutdown so orphan timeouts don't delay drain
                shutdown
                    .sleep_or_shutdown(std::time::Duration::from_secs(timeout_secs))
                    .await;
                // Re-check state: if the session was re-attached in the meantime,
                // don't destroy it (fixes race between attach and orphan timeout).
                let should_destroy = manager
                    .sessions
                    .get(&session_id)
                    .map(|s| s.state == SessionState::Orphaned)
                    .unwrap_or(false);
                if should_destroy {
                    warn!(session_id = %session_id, "orphan timeout expired, destroying session");
                    manager.destroy_session(&session_id).ok();
                }
            });

            entry.orphan_timeout = Some(handle);
        }
    }

    pub fn list_sessions(&self) -> Vec<SessionSummary> {
        self.sessions
            .iter()
            .map(|entry| SessionSummary {
                session_id: entry.id,
                state: entry.state,
                endpoint_count: entry.endpoint_count.load(Ordering::Relaxed),
                created_at: entry.created_at_wall.clone(),
            })
            .collect()
    }

    pub fn get_session_cmd_tx(&self, id: &SessionId) -> Option<mpsc::Sender<SessionCommand>> {
        self.sessions.get(id).map(|s| s.cmd_tx.clone())
    }

    pub fn is_shutting_down(&self) -> bool {
        self.shutdown.is_shutting_down()
    }

    pub fn disconnect_timeout_ms(&self) -> u64 {
        self.disconnect_timeout_secs.saturating_mul(1000)
    }

    /// Query a session's detailed state (endpoints, recordings, VAD)
    pub async fn get_session_details(
        &self,
        id: &SessionId,
    ) -> Option<(SessionState, String, media_session::SessionDetails)> {
        let entry = self.sessions.get(id)?;
        let state = entry.state;
        let created_at = entry.created_at_wall.clone();
        let cmd_tx = entry.cmd_tx.clone();
        drop(entry); // release DashMap lock before await

        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        let timeout = std::time::Duration::from_secs(5);
        tokio::time::timeout(
            timeout,
            cmd_tx.send(SessionCommand::GetInfo { reply: reply_tx }),
        )
        .await
        .ok()? // timeout → None
        .ok()?; // send error → None
        let details = tokio::time::timeout(timeout, reply_rx)
            .await
            .ok()? // timeout → None
            .ok()?; // recv error → None
        Some((state, created_at, details))
    }
}

fn chrono_now() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "unknown".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::IpAddr;
    use std::sync::atomic::AtomicUsize;

    /// Helper: build a minimal SessionManager for unit tests.
    /// Uses a real SocketPool bound to a small ephemeral port range.
    fn test_manager() -> Arc<SessionManager> {
        let shutdown = ShutdownCoordinator::new();
        let metrics = Arc::new(Metrics::new());
        let socket_pool = Arc::new(
            SocketPool::new(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), 30000, 30100).unwrap(),
        );
        let file_cache =
            Arc::new(FileCache::new(std::env::temp_dir().join("rtpbridge_test_cache")).unwrap());
        Arc::new(SessionManager {
            sessions: DashMap::new(),
            shutdown,
            disconnect_timeout_secs: 300,
            media_ip: IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            socket_pool,
            max_sessions: 0,
            max_endpoints_per_session: 0,
            max_recordings_per_session: 100,
            recording_flush_timeout_secs: 10,
            recording_channel_size: 1000,
            max_sdp_size_kb: 64,
            session_idle_timeout_secs: 0,
            empty_session_timeout_secs: 0,
            media_timeout_secs: 5,
            transcode_cache_size: 64,
            media_dir: None,
            file_cache,
            metrics,
            shared_playback: Arc::new(
                crate::playback::shared_playback::SharedPlaybackManager::new(),
            ),
            recording_dir: std::env::temp_dir().join("rtpbridge_test_recordings"),
            session_count: AtomicUsize::new(0),
        })
    }

    /// Helper: insert a synthetic session directly into the DashMap,
    /// returning the receiver end of the command channel.
    fn insert_session(
        mgr: &SessionManager,
        state: SessionState,
        channel_size: usize,
    ) -> (SessionId, mpsc::Receiver<SessionCommand>) {
        let id = SessionId::new_v4();
        let (cmd_tx, cmd_rx) = mpsc::channel::<SessionCommand>(channel_size);
        let session = Session {
            id,
            state,
            created_at_wall: chrono_now(),
            cmd_tx,
            orphan_timeout: None,
            endpoint_count: Arc::new(AtomicUsize::new(0)),
            task_handle: None,
        };
        mgr.sessions.insert(id, session);
        mgr.shutdown.session_started();
        mgr.metrics.sessions_active.inc();
        (id, cmd_rx)
    }

    #[test]
    fn test_attach_reverts_on_channel_full() {
        let mgr = test_manager();

        // Insert an orphaned session with a small channel (capacity = 1).
        let (id, _rx) = insert_session(&mgr, SessionState::Orphaned, 1);

        // Fill the command channel so try_send will fail.
        // Channel capacity is 1, so one Detach fills it.
        let entry = mgr.sessions.get(&id).unwrap();
        entry.cmd_tx.try_send(SessionCommand::Detach).unwrap();
        drop(entry);

        // Now try to attach — the channel is full so Attach should fail.
        let (event_tx, _event_rx) = mpsc::channel(1);
        let (critical_tx, _critical_rx) = mpsc::channel(1);
        let result = mgr.attach_session(&id, event_tx, critical_tx, Arc::new(AtomicU64::new(0)));
        assert_eq!(result.unwrap_err(), "SESSION_BUSY");

        // Verify state was reverted to Orphaned (not stuck as Active).
        let entry = mgr.sessions.get(&id).unwrap();
        assert_eq!(entry.state, SessionState::Orphaned);
    }

    #[test]
    fn test_attach_succeeds_when_channel_has_room() {
        let mgr = test_manager();

        // Insert an orphaned session with enough channel capacity.
        let (id, mut rx) = insert_session(&mgr, SessionState::Orphaned, 8);

        let (event_tx, _event_rx) = mpsc::channel(1);
        let (critical_tx, _critical_rx) = mpsc::channel(1);
        let result = mgr.attach_session(&id, event_tx, critical_tx, Arc::new(AtomicU64::new(0)));
        assert!(result.is_ok());

        // Verify state is now Active.
        let entry = mgr.sessions.get(&id).unwrap();
        assert_eq!(entry.state, SessionState::Active);

        // The Attach command should have been sent.
        let cmd = rx.try_recv().unwrap();
        assert!(matches!(cmd, SessionCommand::Attach { .. }));
    }

    #[test]
    fn test_force_destroy_decrements_metrics() {
        let mgr = test_manager();

        // Insert an active session with channel capacity = 1 and 3 endpoints.
        let (id, _rx) = insert_session(&mgr, SessionState::Active, 1);
        {
            let entry = mgr.sessions.get(&id).unwrap();
            entry.endpoint_count.store(3, Ordering::Relaxed);
            // Fill the channel so Destroy try_send fails.
            entry.cmd_tx.try_send(SessionCommand::Detach).unwrap();
        }

        // Simulate that 3 endpoints had been counted.
        mgr.metrics.endpoints_active.inc();
        mgr.metrics.endpoints_active.inc();
        mgr.metrics.endpoints_active.inc();

        // Force-destroy (channel is full, so it will take the force-remove path).
        let result = mgr.destroy_session(&id);
        assert!(result.is_ok());

        // Session should be removed from the map.
        assert!(mgr.sessions.get(&id).is_none());

        // Metrics should be decremented: sessions_active back to 0, endpoints_active back to 0.
        let output = mgr.metrics.encode().unwrap();
        assert!(
            output.contains("rtpbridge_sessions_active 0"),
            "sessions_active should be 0 after force-destroy, got: {}",
            output
        );
        assert!(
            output.contains("rtpbridge_endpoints_active 0"),
            "endpoints_active should be 0 after force-destroy, got: {}",
            output
        );
    }

    #[test]
    fn test_force_destroy_decrements_metrics_zero_endpoints() {
        let mgr = test_manager();

        // Session with 0 endpoints, channel full.
        let (id, _rx) = insert_session(&mgr, SessionState::Active, 1);
        {
            let entry = mgr.sessions.get(&id).unwrap();
            entry.cmd_tx.try_send(SessionCommand::Detach).unwrap();
        }

        let result = mgr.destroy_session(&id);
        assert!(result.is_ok());
        assert!(mgr.sessions.get(&id).is_none());

        let output = mgr.metrics.encode().unwrap();
        assert!(
            output.contains("rtpbridge_sessions_active 0"),
            "sessions_active should be 0 after force-destroy with no endpoints, got: {}",
            output
        );
    }

    #[tokio::test]
    async fn test_force_destroy_aborts_task_handle() {
        let mgr = test_manager();

        // Insert session with a long-running task and a full channel.
        let (id, _rx) = insert_session(&mgr, SessionState::Active, 1);
        let task_handle = tokio::spawn(async {
            // Simulate a long-running media task
            tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
        });

        // Set the task handle on the session
        {
            let mut entry = mgr.sessions.get_mut(&id).unwrap();
            entry.task_handle = Some(task_handle);
            // Fill the channel to force the force-destroy path
            entry.cmd_tx.try_send(SessionCommand::Detach).unwrap();
        }

        let result = mgr.destroy_session(&id);
        assert!(result.is_ok());
        assert!(
            mgr.sessions.get(&id).is_none(),
            "session should be removed from the map"
        );

        // Give the runtime a chance to process the abort
        tokio::task::yield_now().await;
        // If we reach here without hanging, the task was aborted successfully
    }
}
