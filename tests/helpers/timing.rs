#![allow(dead_code)]

use std::time::Duration;

/// CI timeout multiplier. Set `RTPBRIDGE_TEST_TIMEOUT_MULT=2` (or higher) for slow CI.
pub fn multiplier() -> u64 {
    std::env::var("RTPBRIDGE_TEST_TIMEOUT_MULT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1)
        .max(1)
}

/// Scale a millisecond duration by the CI multiplier.
pub fn scaled_ms(base_ms: u64) -> Duration {
    Duration::from_millis(base_ms * multiplier())
}

/// Packet pacing interval (not scaled — simulates real-time RTP at 50 pps).
pub const PACING: Duration = Duration::from_millis(20);
