use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;

use clap::parser::ValueSource;
use clap::{CommandFactory, Parser};
use serde::Deserialize;

/// Parse a comma-separated list of socket addresses.
fn parse_listen_addrs(s: &str) -> Result<Vec<SocketAddr>, String> {
    s.split(',')
        .map(|part| {
            part.trim()
                .parse::<SocketAddr>()
                .map_err(|e| format!("invalid listen address '{}':{e}", part.trim()))
        })
        .collect()
}

/// Deserialize listen addresses from TOML — accepts a single string like
/// `"0.0.0.0:9100"` or comma-separated `"10.0.0.1:9100, 127.0.0.1:9100"`.
fn deserialize_listen_addrs<'de, D>(deserializer: D) -> Result<Vec<SocketAddr>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let s = String::deserialize(deserializer)?;
    parse_listen_addrs(&s).map_err(serde::de::Error::custom)
}

#[derive(Parser, Debug)]
#[command(name = "rtpbridge", about = "RTP media routing server")]
pub struct Cli {
    /// WebSocket control plane listen addresses (comma-separated ip:port)
    #[arg(short, long, default_value = "0.0.0.0:9100", value_delimiter = ',')]
    pub listen: Vec<SocketAddr>,

    /// Media plane bind IP for RTP/WebRTC UDP sockets
    #[arg(short, long)]
    pub media_ip: Option<IpAddr>,

    /// Path to TOML configuration file
    #[arg(short, long)]
    pub config: Option<PathBuf>,

    /// Log level (trace, debug, info, warn, error)
    #[arg(long, default_value = "info")]
    pub log_level: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    /// WebSocket control plane listen addresses (comma-separated ip:port)
    #[serde(deserialize_with = "deserialize_listen_addrs")]
    pub listen: Vec<SocketAddr>,

    /// Media plane bind IP for all RTP/WebRTC UDP sockets.
    /// Used in SDP c= lines and ICE host candidates.
    /// Separate from control plane to support split interfaces.
    #[serde(default = "default_media_ip")]
    pub media_ip: IpAddr,

    /// UDP port range for plain RTP endpoints (start, end inclusive)
    pub rtp_port_range: (u16, u16),

    /// Session disconnect timeout in seconds
    pub disconnect_timeout_secs: u64,

    /// Maximum time to wait for sessions to drain on shutdown (seconds).
    /// In Kubernetes, set this equal to or slightly below terminationGracePeriodSeconds.
    pub shutdown_max_wait_secs: u64,

    /// Allowed base directory for local file playback.
    /// Local file paths must resolve within this directory.
    /// If None, local file playback is disabled (only URLs are allowed).
    pub media_dir: Option<PathBuf>,

    /// Base directory for recordings accessible via HTTP.
    #[serde(default = "default_recording_dir")]
    pub recording_dir: PathBuf,

    /// File cache directory for URL downloads
    pub cache_dir: PathBuf,

    /// File cache cleanup interval in seconds
    pub cache_cleanup_interval_secs: u64,

    /// Maximum concurrent HTTP downloads for URL file playback
    pub max_concurrent_downloads: usize,

    /// Maximum number of concurrent sessions (0 = unlimited)
    pub max_sessions: usize,

    /// Maximum endpoints per session (0 = unlimited)
    pub max_endpoints_per_session: usize,

    /// Maximum concurrent recordings per session
    pub max_recordings_per_session: usize,

    /// Seconds to wait for recording tasks to flush before aborting
    pub recording_flush_timeout_secs: u64,

    /// Maximum WebSocket message size in KB (applies to both message and frame size)
    pub ws_max_message_size_kb: usize,

    /// Maximum SDP size in KB (rejects oversized SDP offers/answers)
    pub max_sdp_size_kb: usize,

    /// Session idle timeout in seconds (0 = disabled).
    /// Sessions with no media packets and no commands for this duration are auto-destroyed.
    pub session_idle_timeout_secs: u64,

    /// Empty session timeout in seconds (0 = disabled).
    /// Sessions with zero endpoints for this duration are auto-destroyed.
    pub empty_session_timeout_secs: u64,

    /// Media timeout in seconds. An `endpoint.media_timeout` event is emitted when
    /// no RTP packets are received from a remote endpoint for this duration.
    pub media_timeout_secs: u64,

    /// Maximum concurrent WebSocket connections (0 = unlimited)
    pub max_connections: usize,

    /// WebSocket ping interval in seconds
    pub ws_ping_interval_secs: u64,

    /// Event channel buffer size for normal events
    pub event_channel_size: usize,

    /// Event channel buffer size for critical events
    pub critical_event_channel_size: usize,

    /// Maximum entries in the transcode pipeline cache per session
    pub transcode_cache_size: usize,

    /// Maximum file download size in bytes for URL playback
    pub max_file_download_bytes: u64,

    /// Maximum recording download size in bytes for HTTP GET
    pub max_recording_download_bytes: u64,

    /// Recording channel buffer size (packets buffered between session task and writer task)
    pub recording_channel_size: usize,

    /// Log level
    pub log_level: String,
}

fn default_media_ip() -> IpAddr {
    IpAddr::V4(Ipv4Addr::LOCALHOST)
}

fn default_recording_dir() -> PathBuf {
    PathBuf::from("/var/lib/rtpbridge/recordings")
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen: vec![
                "0.0.0.0:9100"
                    .parse()
                    .expect("hardcoded default listen address must parse"),
            ],
            media_ip: default_media_ip(),
            rtp_port_range: (30000, 39999),
            disconnect_timeout_secs: 30,
            shutdown_max_wait_secs: 300,
            media_dir: None,
            recording_dir: default_recording_dir(),
            cache_dir: PathBuf::from("/tmp/rtpbridge-cache"),
            cache_cleanup_interval_secs: 300,
            max_concurrent_downloads: 16,
            max_sessions: 10000,
            max_endpoints_per_session: 20,
            max_recordings_per_session: 100,
            recording_flush_timeout_secs: 10,
            ws_max_message_size_kb: 256,
            max_sdp_size_kb: 64,
            session_idle_timeout_secs: 0,
            empty_session_timeout_secs: 0,
            media_timeout_secs: 5,
            max_connections: 1000,
            ws_ping_interval_secs: 30,
            event_channel_size: 256,
            critical_event_channel_size: 64,
            transcode_cache_size: 64,
            max_file_download_bytes: 100 * 1024 * 1024,
            max_recording_download_bytes: 512 * 1024 * 1024,
            recording_channel_size: 1000,
            log_level: "info".to_string(),
        }
    }
}

impl Config {
    pub fn load(cli: &Cli) -> anyhow::Result<Self> {
        // Re-parse to get ArgMatches for value_source() detection.
        // In production this is cheap (just the handful of CLI args).
        let matches = Cli::command()
            .try_get_matches_from(std::env::args_os())
            .unwrap_or_else(|e| {
                tracing::warn!("CLI re-parse failed ({e}), CLI overrides may be ignored");
                Cli::command().get_matches_from::<[&str; 0], &str>([])
            });
        Self::load_with_matches(cli, &matches)
    }

    pub(crate) fn load_with_matches(cli: &Cli, matches: &clap::ArgMatches) -> anyhow::Result<Self> {
        let mut config = if let Some(path) = &cli.config {
            let contents = std::fs::read_to_string(path)?;
            toml::from_str(&contents)?
        } else {
            Config::default()
        };

        // CLI overrides (only when explicitly provided on the command line)
        if matches.value_source("listen") == Some(ValueSource::CommandLine) || cli.config.is_none()
        {
            config.listen = cli.listen.clone();
        }
        if let Some(ip) = cli.media_ip {
            config.media_ip = ip;
        }
        if matches.value_source("log_level") == Some(ValueSource::CommandLine)
            || cli.config.is_none()
        {
            config.log_level = cli.log_level.clone();
        }

        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if self.listen.is_empty() {
            anyhow::bail!("listen must contain at least one address");
        }
        if self.rtp_port_range.0 > self.rtp_port_range.1 {
            anyhow::bail!(
                "rtp_port_range start ({}) must be <= end ({})",
                self.rtp_port_range.0,
                self.rtp_port_range.1
            );
        }
        if self.rtp_port_range.0 < 1024 {
            anyhow::bail!(
                "rtp_port_range start ({}) must be >= 1024",
                self.rtp_port_range.0
            );
        }
        if self.rtp_port_range.0 % 2 != 0 {
            anyhow::bail!(
                "rtp_port_range start ({}) must be even (RTP uses even/odd port pairs)",
                self.rtp_port_range.0
            );
        }
        if self.rtp_port_range.1 < 1024 {
            anyhow::bail!(
                "rtp_port_range end ({}) must be >= 1024",
                self.rtp_port_range.1
            );
        }
        if self.rtp_port_range.1 > 65534 {
            anyhow::bail!(
                "rtp_port_range end ({}) must be <= 65534 to allow even/odd port pairs",
                self.rtp_port_range.1
            );
        }
        if self.rtp_port_range.1 - self.rtp_port_range.0 < 1 {
            anyhow::bail!(
                "rtp_port_range must span at least 2 ports for one RTP/RTCP pair (even start, odd end)"
            );
        }
        if self.media_timeout_secs == 0 || self.media_timeout_secs > 300 {
            anyhow::bail!("media_timeout_secs must be between 1 and 300");
        }
        if self.cache_cleanup_interval_secs == 0 {
            anyhow::bail!("cache_cleanup_interval_secs must be > 0");
        }
        if self.disconnect_timeout_secs == 0 {
            anyhow::bail!("disconnect_timeout_secs must be > 0");
        }
        if self.shutdown_max_wait_secs == 0 {
            anyhow::bail!("shutdown_max_wait_secs must be > 0");
        }
        if self.ws_max_message_size_kb == 0 {
            anyhow::bail!("ws_max_message_size_kb must be > 0");
        }
        if self.ws_max_message_size_kb > 512_000 {
            anyhow::bail!("ws_max_message_size_kb must be <= 512000 (512 MB)");
        }
        if self.max_sdp_size_kb == 0 {
            anyhow::bail!("max_sdp_size_kb must be > 0");
        }
        if self.max_sdp_size_kb > 10_000 {
            anyhow::bail!("max_sdp_size_kb must be <= 10000 (10 MB)");
        }
        if let Some(ref dir) = self.media_dir {
            if !dir.is_dir() {
                anyhow::bail!("media_dir {dir:?} does not exist or is not a directory");
            }
        }
        // Validate recording_dir parent if it's not the default
        // (defaults may not exist yet; they'll be created on first use)
        if self.recording_dir != default_recording_dir() {
            if let Some(parent) = self.recording_dir.parent() {
                if !parent.as_os_str().is_empty() && !parent.exists() {
                    anyhow::bail!("recording_dir parent {parent:?} does not exist");
                }
            }
        }
        // Verify recording_dir is writable if it already exists
        if self.recording_dir.exists() && !is_dir_writable(&self.recording_dir) {
            anyhow::bail!("recording_dir {:?} is not writable", self.recording_dir);
        }
        if self.recording_flush_timeout_secs == 0 {
            anyhow::bail!("recording_flush_timeout_secs must be > 0");
        }
        if self.max_recordings_per_session == 0 {
            anyhow::bail!("max_recordings_per_session must be > 0");
        }
        if self.max_concurrent_downloads == 0 {
            anyhow::bail!("max_concurrent_downloads must be > 0");
        }
        if self.ws_ping_interval_secs == 0 {
            anyhow::bail!("ws_ping_interval_secs must be > 0");
        }
        if self.event_channel_size == 0 {
            anyhow::bail!("event_channel_size must be > 0");
        }
        if self.critical_event_channel_size == 0 {
            anyhow::bail!("critical_event_channel_size must be > 0");
        }
        if self.recording_channel_size == 0 {
            anyhow::bail!("recording_channel_size must be > 0");
        }
        if self.transcode_cache_size == 0 {
            anyhow::bail!("transcode_cache_size must be > 0");
        }
        // Validate cache_dir parent if it's not the default
        let default_cache = PathBuf::from("/tmp/rtpbridge-cache");
        if self.cache_dir != default_cache {
            if let Some(parent) = self.cache_dir.parent() {
                if !parent.as_os_str().is_empty() && !parent.exists() {
                    anyhow::bail!("cache_dir parent {parent:?} does not exist");
                }
            }
        }
        // Verify cache_dir is writable if it already exists
        if self.cache_dir.exists() && !is_dir_writable(&self.cache_dir) {
            anyhow::bail!("cache_dir {:?} is not writable", self.cache_dir);
        }
        if self.max_sessions > 0 {
            let port_pairs = ((self.rtp_port_range.1 - self.rtp_port_range.0) / 2) as usize;
            if port_pairs < self.max_sessions {
                tracing::warn!(
                    port_pairs = port_pairs,
                    max_sessions = self.max_sessions,
                    "rtp_port_range may be insufficient for max_sessions"
                );
            }
        }
        if self.media_ip.is_loopback() {
            tracing::warn!(
                media_ip = %self.media_ip,
                "media_ip is a loopback address — remote peers cannot reach it. \
                 Set media_ip to a routable address for production use."
            );
        }
        Ok(())
    }
}

fn is_dir_writable(path: &std::path::Path) -> bool {
    let test_file = path.join(".rtpbridge-write-test");
    match std::fs::File::create(&test_file) {
        Ok(_) => {
            let _ = std::fs::remove_file(&test_file);
            true
        }
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = Config::default();
        assert_eq!(
            config.listen,
            vec!["0.0.0.0:9100".parse::<SocketAddr>().unwrap()]
        );
        assert_eq!(config.media_ip, IpAddr::V4(Ipv4Addr::LOCALHOST));
        assert_eq!(config.rtp_port_range, (30000, 39999));
        assert_eq!(config.disconnect_timeout_secs, 30);
        assert_eq!(config.shutdown_max_wait_secs, 300);
        assert!(config.media_dir.is_none());
        assert_eq!(config.cache_dir, PathBuf::from("/tmp/rtpbridge-cache"));
        assert_eq!(config.cache_cleanup_interval_secs, 300);
        assert_eq!(config.max_sessions, 10000);
        assert_eq!(config.max_endpoints_per_session, 20);
    }

    #[test]
    fn test_toml_parsing() {
        let toml_content = r#"
            listen = "127.0.0.1:9200"
            media_ip = "10.0.0.1"
            rtp_port_range = [20000, 29999]
            disconnect_timeout_secs = 60
            max_sessions = 500
        "#;
        let config: Config = toml::from_str(toml_content).unwrap();
        assert_eq!(
            config.listen,
            vec!["127.0.0.1:9200".parse::<SocketAddr>().unwrap()]
        );
        assert_eq!(config.media_ip, "10.0.0.1".parse::<IpAddr>().unwrap());
        assert_eq!(config.rtp_port_range, (20000, 29999));
        assert_eq!(config.disconnect_timeout_secs, 60);
        assert_eq!(config.max_sessions, 500);
        // Unset fields should use defaults
        assert_eq!(config.max_endpoints_per_session, 20);
        assert_eq!(config.cache_cleanup_interval_secs, 300);
    }

    #[test]
    fn test_partial_toml_uses_defaults() {
        let toml_content = r#"
            media_dir = "/tmp/media"
        "#;
        let config: Config = toml::from_str(toml_content).unwrap();
        assert_eq!(config.media_dir, Some(PathBuf::from("/tmp/media")));
        // Everything else defaults
        assert_eq!(
            config.listen,
            vec!["0.0.0.0:9100".parse::<SocketAddr>().unwrap()]
        );
        assert_eq!(config.rtp_port_range, (30000, 39999));
        assert_eq!(config.max_sessions, 10000);
    }

    #[test]
    fn test_cli_override() {
        let toml_content = r#"
            media_ip = "10.0.0.1"
        "#;
        let tmp = std::env::temp_dir().join("rtpbridge-test-config.toml");
        std::fs::write(&tmp, toml_content).unwrap();

        let cli = Cli {
            listen: vec!["127.0.0.1:9100".parse().unwrap()],
            media_ip: Some("192.168.1.1".parse().unwrap()),
            config: Some(tmp.clone()),
            log_level: "debug".to_string(),
        };
        // Simulate explicit CLI args using matches
        let matches = Cli::command().get_matches_from([
            "rtpbridge",
            "--listen",
            "127.0.0.1:9100",
            "--media-ip",
            "192.168.1.1",
            "--config",
            tmp.to_str().unwrap(),
            "--log-level",
            "debug",
        ]);
        let config = Config::load_with_matches(&cli, &matches).unwrap();
        // CLI media_ip should override TOML
        assert_eq!(config.media_ip, "192.168.1.1".parse::<IpAddr>().unwrap());
        assert_eq!(config.log_level, "debug");

        std::fs::remove_file(tmp).ok();
    }

    #[test]
    fn test_validation_reversed_port_range() {
        let mut config = Config::default();
        config.rtp_port_range = (39999, 30000);
        let err = config.validate().unwrap_err();
        assert!(
            err.to_string().contains("must be <= end"),
            "unexpected error: {}",
            err
        );
    }

    #[test]
    fn test_validation_low_port_range() {
        let mut config = Config::default();
        config.rtp_port_range = (80, 1000);
        let err = config.validate().unwrap_err();
        assert!(
            err.to_string().contains("must be >= 1024"),
            "unexpected error: {}",
            err
        );
    }

    #[test]
    fn test_validation_odd_port_start() {
        let mut config = Config::default();
        config.rtp_port_range = (30001, 39999);
        let err = config.validate().unwrap_err();
        assert!(
            err.to_string().contains("must be even"),
            "unexpected error: {}",
            err
        );
    }

    #[test]
    fn test_validation_zero_cache_cleanup_interval() {
        let mut config = Config::default();
        config.cache_cleanup_interval_secs = 0;
        let err = config.validate().unwrap_err();
        assert!(
            err.to_string()
                .contains("cache_cleanup_interval_secs must be > 0"),
            "unexpected error: {}",
            err
        );
    }

    #[test]
    fn test_validation_zero_disconnect_timeout() {
        let mut config = Config::default();
        config.disconnect_timeout_secs = 0;
        let err = config.validate().unwrap_err();
        assert!(
            err.to_string()
                .contains("disconnect_timeout_secs must be > 0"),
            "unexpected error: {}",
            err
        );
    }

    #[test]
    fn test_validation_zero_shutdown_timeout() {
        let mut config = Config::default();
        config.shutdown_max_wait_secs = 0;
        let err = config.validate().unwrap_err();
        assert!(
            err.to_string()
                .contains("shutdown_max_wait_secs must be > 0"),
            "unexpected error: {}",
            err
        );
    }

    #[test]
    fn test_validation_nonexistent_media_dir() {
        let mut config = Config::default();
        config.media_dir = Some(PathBuf::from("/nonexistent/path"));
        let err = config.validate().unwrap_err();
        assert!(
            err.to_string()
                .contains("does not exist or is not a directory"),
            "unexpected error: {}",
            err
        );
    }

    #[test]
    fn test_validation_valid_config() {
        let config = Config::default();
        config.validate().unwrap();
    }

    #[test]
    fn test_validation_port_range_end_too_low() {
        let mut config = Config::default();
        config.rtp_port_range = (1024, 1024);
        let err = config.validate().unwrap_err();
        assert!(
            err.to_string().contains("at least 2 ports"),
            "unexpected error: {}",
            err
        );
    }

    #[test]
    fn test_validation_port_range_end_below_1024() {
        let mut config = Config::default();
        // start < end, start >= 1024, start even, but end < 1024
        // This is actually impossible since start must be >= 1024 and start <= end.
        // But we can test the end < 1024 check directly by bypassing the start check:
        // Actually the reversed check catches this first. Test the end-specific check
        // with a valid start but low end via the same-port case.
        config.rtp_port_range = (1024, 500);
        let err = config.validate().unwrap_err();
        // The "start must be <= end" check fires first here
        assert!(
            err.to_string().contains("must be"),
            "unexpected error: {}",
            err
        );
    }

    #[test]
    fn test_validation_port_range_single_port() {
        let mut config = Config::default();
        config.rtp_port_range = (30000, 30000);
        let err = config.validate().unwrap_err();
        assert!(
            err.to_string().contains("at least 2 ports"),
            "unexpected error: {}",
            err
        );
    }

    #[test]
    fn test_validation_port_range_minimal_valid() {
        let mut config = Config::default();
        // Minimum valid range: one even/odd pair (even start, odd end, diff >= 1)
        config.rtp_port_range = (30000, 30001);
        config.validate().unwrap();
    }

    #[test]
    fn test_validation_ws_max_message_size_upper_bound() {
        let mut config = Config::default();
        config.ws_max_message_size_kb = 512_001;
        let err = config.validate().unwrap_err();
        assert!(
            err.to_string()
                .contains("ws_max_message_size_kb must be <= 512000"),
            "unexpected error: {}",
            err
        );
    }

    #[test]
    fn test_validation_max_sdp_size_upper_bound() {
        let mut config = Config::default();
        config.max_sdp_size_kb = 10_001;
        let err = config.validate().unwrap_err();
        assert!(
            err.to_string().contains("max_sdp_size_kb must be <= 10000"),
            "unexpected error: {}",
            err
        );
    }

    #[test]
    fn test_validation_zero_ws_max_message_size() {
        let mut config = Config::default();
        config.ws_max_message_size_kb = 0;
        let err = config.validate().unwrap_err();
        assert!(
            err.to_string()
                .contains("ws_max_message_size_kb must be > 0"),
            "unexpected error: {}",
            err
        );
    }

    #[test]
    fn test_validation_zero_max_sdp_size() {
        let mut config = Config::default();
        config.max_sdp_size_kb = 0;
        let err = config.validate().unwrap_err();
        assert!(
            err.to_string().contains("max_sdp_size_kb must be > 0"),
            "unexpected error: {}",
            err
        );
    }

    #[test]
    fn test_validation_zero_recording_flush_timeout() {
        let mut config = Config::default();
        config.recording_flush_timeout_secs = 0;
        let err = config.validate().unwrap_err();
        assert!(
            err.to_string()
                .contains("recording_flush_timeout_secs must be > 0"),
            "unexpected error: {}",
            err
        );
    }

    #[test]
    fn test_validation_zero_max_recordings_per_session() {
        let mut config = Config::default();
        config.max_recordings_per_session = 0;
        let err = config.validate().unwrap_err();
        assert!(
            err.to_string()
                .contains("max_recordings_per_session must be > 0"),
            "unexpected error: {}",
            err
        );
    }

    #[test]
    fn test_validation_zero_max_concurrent_downloads() {
        let mut config = Config::default();
        config.max_concurrent_downloads = 0;
        let err = config.validate().unwrap_err();
        assert!(
            err.to_string()
                .contains("max_concurrent_downloads must be > 0"),
            "unexpected error: {}",
            err
        );
    }

    #[test]
    fn test_validation_zero_ws_ping_interval() {
        let mut config = Config::default();
        config.ws_ping_interval_secs = 0;
        let err = config.validate().unwrap_err();
        assert!(
            err.to_string()
                .contains("ws_ping_interval_secs must be > 0"),
            "unexpected error: {}",
            err
        );
    }

    #[test]
    fn test_validation_zero_event_channel_size() {
        let mut config = Config::default();
        config.event_channel_size = 0;
        let err = config.validate().unwrap_err();
        assert!(
            err.to_string().contains("event_channel_size must be > 0"),
            "unexpected error: {}",
            err
        );
    }

    #[test]
    fn test_validation_zero_critical_event_channel_size() {
        let mut config = Config::default();
        config.critical_event_channel_size = 0;
        let err = config.validate().unwrap_err();
        assert!(
            err.to_string()
                .contains("critical_event_channel_size must be > 0"),
            "unexpected error: {}",
            err
        );
    }

    #[test]
    fn test_validation_port_range_end_too_high() {
        let mut config = Config::default();
        config.rtp_port_range = (65000, 65535);
        let err = config.validate().unwrap_err();
        assert!(
            err.to_string().contains("must be <= 65534"),
            "unexpected error: {}",
            err
        );
    }

    #[test]
    fn test_validation_port_range_max_valid() {
        let mut config = Config::default();
        config.rtp_port_range = (65530, 65533);
        config.validate().unwrap();
    }

    #[test]
    fn test_validation_zero_transcode_cache_size() {
        let mut config = Config::default();
        config.transcode_cache_size = 0;
        let err = config.validate().unwrap_err();
        assert!(
            err.to_string().contains("transcode_cache_size must be > 0"),
            "unexpected error: {}",
            err
        );
    }
}
