use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use tokio::sync::Notify;

/// Coordinates graceful shutdown across the application.
///
/// On SIGINT/SIGTERM:
/// 1. Stop accepting new connections/sessions
/// 2. Wait for active sessions to drain (up to max_wait)
/// 3. Force shutdown if max_wait exceeded
#[derive(Clone)]
pub struct ShutdownCoordinator {
    inner: Arc<Inner>,
}

struct Inner {
    /// Set to true when shutdown signal received
    shutting_down: AtomicBool,
    /// Number of active sessions
    active_sessions: AtomicUsize,
    /// Notified when active_sessions reaches 0
    drained: Notify,
    /// Notified when shutdown signal received
    shutdown_signal: Notify,
}

impl Default for ShutdownCoordinator {
    fn default() -> Self {
        Self::new()
    }
}

impl ShutdownCoordinator {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                shutting_down: AtomicBool::new(false),
                active_sessions: AtomicUsize::new(0),
                drained: Notify::new(),
                shutdown_signal: Notify::new(),
            }),
        }
    }

    /// Returns true if shutdown has been initiated
    pub fn is_shutting_down(&self) -> bool {
        self.inner.shutting_down.load(Ordering::Acquire)
    }

    /// Signal that shutdown should begin
    pub fn initiate_shutdown(&self) {
        self.inner.shutting_down.store(true, Ordering::Release);
        self.inner.shutdown_signal.notify_waiters();
    }

    /// Wait for the shutdown signal
    pub async fn wait_for_shutdown(&self) {
        // Register the notified future before checking the flag to avoid missing the signal
        let notified = self.inner.shutdown_signal.notified();
        if self.is_shutting_down() {
            return;
        }
        notified.await;
    }

    /// Register a new active session
    pub fn session_started(&self) {
        self.inner.active_sessions.fetch_add(1, Ordering::AcqRel);
    }

    /// Unregister an active session
    pub fn session_ended(&self) {
        let result = self.inner.active_sessions.fetch_update(
            Ordering::AcqRel,
            Ordering::Relaxed,
            |current| {
                if current > 0 { Some(current - 1) } else { None }
            },
        );
        match result {
            Ok(1) => self.inner.drained.notify_waiters(), // was 1, now 0
            Err(_) => {
                tracing::error!("session_ended called with 0 active sessions — ignoring");
            }
            _ => {}
        }
    }

    /// Get current active session count
    pub fn active_session_count(&self) -> usize {
        self.inner.active_sessions.load(Ordering::Acquire)
    }

    /// Sleep for the given duration, but wake immediately if shutdown is initiated.
    /// Returns immediately if shutdown was already initiated.
    pub async fn sleep_or_shutdown(&self, duration: std::time::Duration) {
        let notified = self.inner.shutdown_signal.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();

        if self.is_shutting_down() {
            return;
        }

        tokio::select! {
            _ = tokio::time::sleep(duration) => {}
            _ = notified => {}
        }
    }

    /// Wait until all sessions have drained
    pub async fn wait_for_drain(&self) {
        loop {
            let notified = self.inner.drained.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.active_session_count() == 0 {
                return;
            }
            notified.await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn test_shutdown_lifecycle() {
        let coord = ShutdownCoordinator::new();
        assert!(!coord.is_shutting_down());
        assert_eq!(coord.active_session_count(), 0);

        coord.session_started();
        coord.session_started();
        assert_eq!(coord.active_session_count(), 2);

        coord.initiate_shutdown();
        assert!(coord.is_shutting_down());

        coord.session_ended();
        assert_eq!(coord.active_session_count(), 1);

        coord.session_ended();
        assert_eq!(coord.active_session_count(), 0);
    }

    #[tokio::test]
    async fn test_wait_for_drain() {
        let coord = ShutdownCoordinator::new();
        coord.session_started();
        coord.session_started();
        coord.session_started();

        let coord_clone = coord.clone();
        let drain_handle = tokio::spawn(async move {
            coord_clone.wait_for_drain().await;
        });

        // End sessions one by one
        for _ in 0..3 {
            tokio::time::sleep(Duration::from_millis(10)).await;
            coord.session_ended();
        }

        // wait_for_drain should complete within a reasonable time
        let result = tokio::time::timeout(Duration::from_secs(1), drain_handle).await;
        assert!(
            result.is_ok(),
            "wait_for_drain should complete when all sessions end"
        );
    }

    #[tokio::test]
    async fn test_drain_immediate_if_no_sessions() {
        let coord = ShutdownCoordinator::new();
        // No sessions started — drain should return immediately
        let result = tokio::time::timeout(Duration::from_millis(100), coord.wait_for_drain()).await;
        assert!(
            result.is_ok(),
            "wait_for_drain should return immediately with 0 sessions"
        );
    }

    #[tokio::test]
    async fn test_wait_for_drain_race_condition() {
        // Regression test: if the last session ends between the count check
        // and the .await on the Notify, wait_for_drain must still complete.
        let coord = ShutdownCoordinator::new();
        coord.session_started();

        let coord_clone = coord.clone();
        let drain_handle = tokio::spawn(async move {
            coord_clone.wait_for_drain().await;
        });

        // End the session immediately (no sleep) to maximize the chance
        // of hitting the race window.
        coord.session_ended();

        let result = tokio::time::timeout(Duration::from_secs(1), drain_handle).await;
        assert!(
            result.is_ok(),
            "wait_for_drain should complete even when session ends immediately"
        );
    }

    #[test]
    fn test_session_ended_underflow_guard() {
        let coord = ShutdownCoordinator::new();
        // Calling session_ended with 0 active sessions should not underflow
        coord.session_ended();
        assert_eq!(coord.active_session_count(), 0);

        // Normal lifecycle still works after the spurious call
        coord.session_started();
        assert_eq!(coord.active_session_count(), 1);
        coord.session_ended();
        assert_eq!(coord.active_session_count(), 0);
    }

    #[tokio::test]
    async fn test_shutdown_signal() {
        let coord = ShutdownCoordinator::new();
        let coord_clone = coord.clone();

        let wait_handle = tokio::spawn(async move {
            coord_clone.wait_for_shutdown().await;
        });

        // Initiate shutdown from another task
        tokio::time::sleep(Duration::from_millis(10)).await;
        coord.initiate_shutdown();

        let result = tokio::time::timeout(Duration::from_secs(1), wait_handle).await;
        assert!(
            result.is_ok(),
            "wait_for_shutdown should complete after initiate_shutdown"
        );
    }
}
