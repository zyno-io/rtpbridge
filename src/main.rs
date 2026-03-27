mod config;
mod control;
mod media;
mod metrics;
mod net;
mod playback;
mod recording;
mod session;
mod shutdown;

use clap::Parser;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

use std::sync::Arc;

use config::{Cli, Config};
use metrics::Metrics;
use playback::file_cache::FileCache;
use session::SessionManager;
use shutdown::ShutdownCoordinator;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    // Load config before initializing tracing so config-file log_level takes effect.
    let config = Config::load(&cli)?;

    let filter = EnvFilter::try_new(&config.log_level)
        .map_err(|e| anyhow::anyhow!("invalid log_level '{}': {e}", config.log_level))?;

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true)
        .init();
    info!(listen = ?config.listen, media_ip = %config.media_ip, "rtpbridge starting");

    if config.max_sessions == 0 {
        warn!(
            "max_sessions is 0 (unlimited) — consider setting a limit to prevent resource exhaustion"
        );
    }
    if config.max_endpoints_per_session == 0 {
        warn!("max_endpoints_per_session is 0 (unlimited) — consider setting a limit");
    }

    let shutdown = ShutdownCoordinator::new();

    // Install signal handlers.
    // First signal initiates graceful shutdown. Third signal force-exits
    // (useful during dev with Ctrl+C; safe in prod since orchestrators
    // send SIGTERM once then wait, then SIGKILL).
    let shutdown_signal = shutdown.clone();
    tokio::spawn(async move {
        let mut sigint =
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt()) {
                Ok(s) => s,
                Err(e) => {
                    error!(error = %e, "failed to install SIGINT handler");
                    return;
                }
            };
        let mut sigterm =
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                Ok(s) => s,
                Err(e) => {
                    error!(error = %e, "failed to install SIGTERM handler");
                    return;
                }
            };

        // First signal: initiate graceful shutdown
        tokio::select! {
            _ = sigint.recv() => info!("Received SIGINT, initiating graceful shutdown"),
            _ = sigterm.recv() => info!("Received SIGTERM, initiating graceful shutdown"),
        }
        shutdown_signal.initiate_shutdown();

        // Second signal: warn
        tokio::select! {
            _ = sigint.recv() => warn!("Received SIGINT again (send once more to force quit)"),
            _ = sigterm.recv() => warn!("Received SIGTERM again (send once more to force quit)"),
        }

        // Third signal: force exit
        tokio::select! {
            _ = sigint.recv() => {
                error!("Received SIGINT a third time, forcing exit");
                std::process::exit(1);
            }
            _ = sigterm.recv() => {
                error!("Received SIGTERM a third time, forcing exit");
                std::process::exit(1);
            }
        }
    });

    let metrics = Arc::new(Metrics::new());

    let file_cache = Arc::new(
        FileCache::with_options(
            config.cache_dir.clone(),
            1000,
            config.max_concurrent_downloads,
            config.max_file_download_bytes,
        )
        .map_err(|e| anyhow::anyhow!("Failed to initialize file cache: {e}"))?,
    );
    let cleanup_handle =
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
    )?;

    // Start WebSocket control server
    let ws_handle = {
        let manager = manager.clone();
        let shutdown = shutdown.clone();
        let metrics = Arc::clone(&metrics);
        let listen_addrs = config.listen.clone();
        let recording_dir = config.recording_dir.clone();
        let ws_max_message_size_kb = config.ws_max_message_size_kb;
        let max_connections = config.max_connections;
        let max_recording_download_bytes = config.max_recording_download_bytes;
        let ws_ping_interval_secs = config.ws_ping_interval_secs;
        let event_channel_size = config.event_channel_size;
        let critical_event_channel_size = config.critical_event_channel_size;
        tokio::spawn(async move {
            if let Err(e) = control::server::run_websocket_server(
                listen_addrs,
                manager,
                shutdown,
                metrics,
                recording_dir,
                ws_max_message_size_kb,
                max_connections,
                max_recording_download_bytes,
                ws_ping_interval_secs,
                event_channel_size,
                critical_event_channel_size,
            )
            .await
            {
                error!(error = %e, "WebSocket server failed");
            }
        })
    };

    // Wait for shutdown signal, monitoring critical tasks for unexpected exits
    tokio::select! {
        _ = shutdown.wait_for_shutdown() => {}
        result = ws_handle => {
            // Control server exited unexpectedly (bind failure, etc.) — fatal
            match result {
                Ok(()) => error!("Control server exited — cannot accept connections"),
                Err(e) => error!(error = %e, "Control server task panicked"),
            }
            std::process::exit(1);
        }
        result = cleanup_handle => {
            match result {
                Ok(()) => warn!("file cache cleanup task exited unexpectedly"),
                Err(e) => error!(error = %e, "file cache cleanup task panicked"),
            }
            shutdown.initiate_shutdown();
        }
    }

    let active = shutdown.active_session_count();
    if active > 0 {
        info!(
            active_sessions = active,
            max_wait_secs = config.shutdown_max_wait_secs,
            "Waiting for active sessions to drain"
        );

        tokio::select! {
            _ = shutdown.wait_for_drain() => {
                info!("All sessions drained");
            }
            _ = tokio::time::sleep(std::time::Duration::from_secs(config.shutdown_max_wait_secs)) => {
                warn!(remaining = shutdown.active_session_count(),
                      "Shutdown max wait exceeded, forcing shutdown");
            }
        }
    }

    info!("rtpbridge shutdown complete");
    Ok(())
}
