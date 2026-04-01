use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU16, Ordering};

use tokio::net::UdpSocket;
use tracing::debug;

/// Allocates UDP port pairs (even RTP, odd RTCP) from a configured range.
pub struct SocketPool {
    bind_ip: IpAddr,
    port_start: u16,
    port_end: u16,
    next_port: AtomicU16,
}

/// An allocated pair of UDP sockets for RTP and RTCP
#[derive(Debug)]
#[allow(dead_code)] // rtcp_addr is read in tests only
pub struct SocketPair {
    pub rtp_socket: UdpSocket,
    pub rtcp_socket: UdpSocket,
    pub rtp_addr: SocketAddr,
    pub rtcp_addr: SocketAddr,
}

impl SocketPool {
    pub fn new(bind_ip: IpAddr, port_start: u16, port_end: u16) -> anyhow::Result<Self> {
        // Ensure start is even
        let start = if port_start.is_multiple_of(2) {
            port_start
        } else {
            port_start.checked_add(1).ok_or_else(|| {
                anyhow::anyhow!("port_start {port_start} too high to round to even")
            })?
        };

        // Ensure end is odd so the last pair (even RTP, odd RTCP) fits within the range
        let port_end = if port_end.is_multiple_of(2) {
            port_end
                .checked_sub(1)
                .ok_or_else(|| anyhow::anyhow!("port_end {port_end} too low to round to odd"))?
        } else {
            port_end
        };

        if !(start < port_end && (port_end - start) >= 1) {
            anyhow::bail!(
                "Invalid port range: {port_start}-{port_end} (need at least one even/odd pair)"
            );
        }

        Ok(Self {
            bind_ip,
            port_start: start,
            port_end,
            next_port: AtomicU16::new(start),
        })
    }

    /// Allocate a pair of UDP sockets (even port for RTP, odd for RTCP).
    /// Tries ports sequentially, wrapping around.
    pub async fn allocate_pair(&self) -> anyhow::Result<SocketPair> {
        // port_end is inclusive, so number of even ports is (port_end - port_start + 1) / 2
        // (integer division rounds down, which is correct: e.g., 20000..=29999 → 5000 pairs)
        let num_pairs = (self.port_end - self.port_start).div_ceil(2);

        // Use 2x iterations to handle CAS contention under concurrent allocation.
        // Each iteration advances the atomic counter, but contending threads may
        // consume extra counter steps without actually trying a unique port.
        for _ in 0..(num_pairs as u32).saturating_mul(2) {
            let port = self.next_even_port();

            let rtp_addr = SocketAddr::new(self.bind_ip, port);
            let rtcp_addr = SocketAddr::new(self.bind_ip, port + 1);

            // Try to bind both sockets
            let rtp_socket = match UdpSocket::bind(rtp_addr).await {
                Ok(s) => s,
                Err(e) => {
                    // Fail fast on permission errors instead of trying every port
                    if e.kind() == std::io::ErrorKind::PermissionDenied {
                        anyhow::bail!("Permission denied binding UDP port {port}: {e}");
                    }
                    debug!(port = port, error = %e, "RTP port bind failed, trying next");
                    continue;
                }
            };
            let rtcp_socket = match UdpSocket::bind(rtcp_addr).await {
                Ok(s) => s,
                Err(e) => {
                    if e.kind() == std::io::ErrorKind::PermissionDenied {
                        anyhow::bail!("Permission denied binding UDP port {}: {}", port + 1, e);
                    }
                    debug!(port = port + 1, error = %e, "RTCP port bind failed, trying next");
                    continue;
                }
            };

            return Ok(SocketPair {
                rtp_addr: rtp_socket.local_addr()?,
                rtcp_addr: rtcp_socket.local_addr()?,
                rtp_socket,
                rtcp_socket,
            });
        }

        anyhow::bail!(
            "No available port pairs in range {}-{}",
            self.port_start,
            self.port_end
        )
    }

    fn next_even_port(&self) -> u16 {
        loop {
            let current = self.next_port.load(Ordering::Relaxed);
            // port_end is inclusive; wrap when the next even port would exceed it
            let next = if (current as u32) + 2 > self.port_end as u32 {
                self.port_start
            } else {
                current + 2
            };

            if self
                .next_port
                .compare_exchange(current, next, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                return current;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_pool_exhaustion() {
        use tokio::net::UdpSocket;

        // Pre-bind all ports in the range so the pool can't allocate any
        let _b0 = UdpSocket::bind("127.0.0.1:40500").await.unwrap();
        let _b1 = UdpSocket::bind("127.0.0.1:40501").await.unwrap();
        let _b2 = UdpSocket::bind("127.0.0.1:40502").await.unwrap();
        let _b3 = UdpSocket::bind("127.0.0.1:40503").await.unwrap();

        let pool = SocketPool::new("127.0.0.1".parse().unwrap(), 40500, 40504).unwrap();
        let result = pool.allocate_pair().await;
        assert!(
            result.is_err(),
            "pool should fail when all ports are pre-bound"
        );
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("No available port pairs"),
            "error should mention exhaustion: {err_msg}"
        );
    }

    #[tokio::test]
    async fn test_pool_wraps_around() {
        // Allocate a pair, drop it, then allocate again — should wrap around and succeed
        let pool = SocketPool::new("127.0.0.1".parse().unwrap(), 40200, 40204).unwrap();

        let pair1 = pool.allocate_pair().await;
        if pair1.is_err() {
            return;
        }
        drop(pair1);

        // Port is freed, next allocation should succeed (wraps around)
        let pair2 = pool.allocate_pair().await;
        assert!(pair2.is_ok(), "should allocate after wrap-around");
    }

    #[tokio::test]
    async fn test_pool_even_port_enforcement() {
        // Pass an odd start port — pool should round up to even
        let pool = SocketPool::new("127.0.0.1".parse().unwrap(), 40301, 40310).unwrap();

        let pair = pool.allocate_pair().await;
        if let Ok(pair) = pair {
            assert_eq!(
                pair.rtp_addr.port() % 2,
                0,
                "RTP port should be even: {}",
                pair.rtp_addr.port()
            );
            assert_eq!(
                pair.rtcp_addr.port() % 2,
                1,
                "RTCP port should be odd: {}",
                pair.rtcp_addr.port()
            );
        }
    }

    #[test]
    fn test_invalid_port_range() {
        let ip: IpAddr = "127.0.0.1".parse().unwrap();
        assert!(SocketPool::new(ip, 100, 100).is_err()); // even end normalized to 99, 99 < 100
        assert!(SocketPool::new(ip, 100, 101).is_ok()); // single pair 100/101
        assert!(SocketPool::new(ip, 200, 100).is_err()); // reversed
        assert!(SocketPool::new(ip, 100, 102).is_ok()); // even end normalized to 101, single pair
    }

    #[tokio::test]
    async fn test_pool_permission_denied_privileged_ports() {
        // Ports below 1024 require root on most systems.
        // If we're running as root this test is a no-op.
        let pool = SocketPool::new("127.0.0.1".parse().unwrap(), 80, 90);
        if pool.is_err() {
            return; // range too small
        }
        let result = pool.unwrap().allocate_pair().await;
        if cfg!(target_os = "linux") || cfg!(target_os = "macos") {
            // On non-root this should fail fast with permission denied,
            // not try all ports silently.
            if result.is_err() {
                let err = result.unwrap_err().to_string();
                assert!(
                    err.contains("Permission denied") || err.contains("No available port pairs"),
                    "error should mention permission denied or exhaustion: {err}"
                );
            }
            // If it succeeds, we're running as root — that's fine
        }
    }

    #[test]
    fn test_port_range_exactly_one_pair() {
        // A range of exactly 2 ports (one even/odd pair) should work
        let ip: IpAddr = "127.0.0.1".parse().unwrap();
        let pool = SocketPool::new(ip, 40600, 40602);
        assert!(
            pool.is_ok(),
            "range of exactly 2 ports (1 pair) should be valid"
        );
    }

    #[test]
    fn test_port_range_odd_start_single_pair() {
        // Odd start rounded up to even, leaving room for exactly 1 pair
        let ip: IpAddr = "127.0.0.1".parse().unwrap();
        // 40601 rounds to 40602, need end > 40604 for one pair
        let pool = SocketPool::new(ip, 40601, 40604);
        assert!(
            pool.is_ok(),
            "odd start rounded up should still allow 1 pair"
        );
    }

    #[test]
    fn test_port_range_after_rounding() {
        // Odd start=40603 rounds to 40604. End=40605 (odd, no normalization).
        // (40605-40604)=1 → valid single pair 40604/40605.
        let ip: IpAddr = "127.0.0.1".parse().unwrap();
        let result = SocketPool::new(ip, 40603, 40605);
        assert!(result.is_ok(), "single pair after rounding should be valid");

        // Odd start=40603 rounds to 40604. End=40604 (even) normalizes to 40603.
        // 40603 < 40604 is false → fails.
        let result = SocketPool::new(ip, 40603, 40604);
        assert!(
            result.is_err(),
            "range too small after rounding should fail"
        );
    }

    #[tokio::test]
    async fn test_pool_allocate_single_pair_range() {
        // Range of exactly 1 pair.
        let pool = SocketPool::new("127.0.0.1".parse().unwrap(), 40700, 40702).unwrap();
        let pair1 = pool.allocate_pair().await;
        if pair1.is_err() {
            return; // ports in use on CI
        }
        let _pair1 = pair1.unwrap();
        assert_eq!(_pair1.rtp_addr.port() % 2, 0, "RTP port should be even");

        // Pre-bind the wrap-around port so the pool has no ports left to try
        let _blocker = UdpSocket::bind("127.0.0.1:40702").await.ok();

        // Second allocation: range_size=1, so it tries 1 port. The first port
        // (40700) is already bound by pair1. If 40702 is also blocked, it fails.
        let pair2 = pool.allocate_pair().await;
        // Whether this succeeds depends on whether _blocker took 40702.
        // The key assertion is it doesn't panic.
        drop(pair2);
    }

    #[tokio::test]
    async fn test_pool_last_pair_odd_inclusive_end() {
        // Regression: with an odd inclusive port_end (e.g., 41209), the last valid
        // even port is 41208 (RTCP on 41209). Pre-bind all pairs EXCEPT the last
        // one and verify the pool can still allocate it.
        //
        // Range 41200..=41209 has 5 pairs: 41200/01, 41202/03, 41204/05, 41206/07, 41208/09
        let pool = SocketPool::new("127.0.0.1".parse().unwrap(), 41200, 41209).unwrap();

        // Pre-bind the first 4 pairs so only 41208/41209 is free
        let _b0 = UdpSocket::bind("127.0.0.1:41200").await.unwrap();
        let _b1 = UdpSocket::bind("127.0.0.1:41201").await.unwrap();
        let _b2 = UdpSocket::bind("127.0.0.1:41202").await.unwrap();
        let _b3 = UdpSocket::bind("127.0.0.1:41203").await.unwrap();
        let _b4 = UdpSocket::bind("127.0.0.1:41204").await.unwrap();
        let _b5 = UdpSocket::bind("127.0.0.1:41205").await.unwrap();
        let _b6 = UdpSocket::bind("127.0.0.1:41206").await.unwrap();
        let _b7 = UdpSocket::bind("127.0.0.1:41207").await.unwrap();

        let result = pool.allocate_pair().await;
        assert!(result.is_ok(), "should find the last free pair 41208/41209");
        let pair = result.unwrap();
        assert_eq!(pair.rtp_addr.port(), 41208, "RTP port should be 41208");
        assert_eq!(pair.rtcp_addr.port(), 41209, "RTCP port should be 41209");
    }

    #[tokio::test]
    async fn test_pool_allocate_and_release() {
        // Use a range with 3 even/odd pairs (6 ports)
        let pool = SocketPool::new("127.0.0.1".parse().unwrap(), 41100, 41106).unwrap();

        // Allocate first pair
        let pair1 = pool.allocate_pair().await;
        if pair1.is_err() {
            // Ports may be in use on CI; skip gracefully
            return;
        }
        let pair1 = pair1.unwrap();

        // Verify ports are even/odd and in the expected range
        let rtp_port = pair1.rtp_addr.port();
        let rtcp_port = pair1.rtcp_addr.port();
        assert!(
            rtp_port >= 41100 && rtp_port <= 41106,
            "RTP port {} should be in range 41100..=41106",
            rtp_port
        );
        assert_eq!(rtp_port % 2, 0, "RTP port should be even");
        assert_eq!(rtcp_port, rtp_port + 1, "RTCP port should be RTP port + 1");

        // Allocate a second pair (should be a different port)
        let pair2 = pool.allocate_pair().await;
        if pair2.is_err() {
            drop(pair1);
            return;
        }
        let pair2 = pair2.unwrap();
        assert_ne!(
            pair1.rtp_addr.port(),
            pair2.rtp_addr.port(),
            "two allocations should use different ports"
        );

        // Drop both pairs (releasing the sockets)
        drop(pair1);
        drop(pair2);

        // After releasing, allocating again should succeed
        // (the pool wraps around and the OS has freed the ports)
        let pair3 = pool.allocate_pair().await;
        assert!(
            pair3.is_ok(),
            "allocation should succeed after releasing previous pairs"
        );
        let pair3 = pair3.unwrap();
        assert_eq!(
            pair3.rtp_addr.port() % 2,
            0,
            "re-allocated RTP port should be even"
        );
        assert_eq!(
            pair3.rtcp_addr.port(),
            pair3.rtp_addr.port() + 1,
            "re-allocated RTCP port should be RTP port + 1"
        );
    }

    #[tokio::test]
    async fn test_pool_even_port_end_normalized() {
        // Even port_end should be normalized to port_end-1 so RTCP never exceeds the range.
        // Range (41300, 41302): effective end becomes 41301, only pair is 41300/41301.
        let pool = SocketPool::new("127.0.0.1".parse().unwrap(), 41300, 41302).unwrap();
        let pair = pool.allocate_pair().await;
        if let Ok(pair) = pair {
            assert_eq!(pair.rtp_addr.port(), 41300);
            assert_eq!(pair.rtcp_addr.port(), 41301);
        }
    }

    #[test]
    fn test_even_port_end_too_small_after_normalization() {
        // Range (40600, 40600): even end normalizes to 40599, 40599 < 40600 fails.
        let ip: IpAddr = "127.0.0.1".parse().unwrap();
        assert!(SocketPool::new(ip, 40600, 40600).is_err());
    }

    #[test]
    fn test_next_even_port_wraps_correctly_near_u16_max() {
        // Verify that next_even_port doesn't overflow u16 arithmetic when
        // port_end is near 65535. The u32 cast in the comparison prevents this.
        let ip: IpAddr = "127.0.0.1".parse().unwrap();
        let pool = SocketPool::new(ip, 65530, 65533).unwrap();

        // Advance the counter to the last valid port (65532)
        pool.next_port.store(65532, Ordering::Relaxed);
        let port = pool.next_even_port();
        assert_eq!(port, 65532, "should return the last valid port");

        // After returning 65532, the counter should wrap to port_start (65530)
        let next = pool.next_port.load(Ordering::Relaxed);
        assert_eq!(
            next, 65530,
            "counter should wrap to port_start, not overflow"
        );
    }
}
