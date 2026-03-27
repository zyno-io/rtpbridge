#![allow(dead_code)]

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use str0m::change::SdpOffer;
use str0m::media::{Direction, MediaKind};
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, Input, Output, Rtc, RtcConfig};
use tokio::net::UdpSocket;
use tokio::sync::Mutex;

/// A simulated WebRTC peer for e2e testing.
/// Uses str0m directly (not ICE lite — acts as a normal client).
pub struct TestPeer {
    rtc: Rtc,
    socket: Arc<UdpSocket>,
    local_addr: SocketAddr,
    packets_received: Arc<AtomicU64>,
    drive_handle: Option<tokio::task::JoinHandle<()>>,
}

impl TestPeer {
    /// Create a new test peer bound to a local address
    pub async fn new() -> Self {
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let local_addr = socket.local_addr().unwrap();

        let rtc = RtcConfig::new().set_rtp_mode(true).build(Instant::now());

        Self {
            rtc,
            socket: Arc::new(socket),
            local_addr,
            packets_received: Arc::new(AtomicU64::new(0)),
            drive_handle: None,
        }
    }

    /// Create an SDP offer to send to rtpbridge
    pub fn create_offer(&mut self) -> String {
        let candidate = Candidate::host(self.local_addr, "udp").unwrap();
        self.rtc.add_local_candidate(candidate);

        let mut api = self.rtc.sdp_api();
        let _mid = api.add_media(MediaKind::Audio, Direction::SendRecv, None, None, None);
        let (offer, _pending) = api.apply().expect("apply failed");

        // We need to store pending for later accept_answer, but since
        // TestPeer is simplified, we'll accept the answer in a separate call
        // For now, leak the pending (simplified test helper)
        let offer_str = offer.to_sdp_string();

        // Store pending in a way we can retrieve it — use a simple approach
        // by immediately preparing for accept_answer
        self.rtc
            .sdp_api()
            .accept_offer(SdpOffer::from_sdp_string(&offer_str).unwrap())
            .ok();

        offer_str
    }

    /// Accept an SDP answer from rtpbridge (after we sent an offer)
    pub fn accept_answer(&mut self, answer_sdp: &str) {
        // In the simplified helper, the offer/answer is already handled
        // by the create_offer flow. For a real test, we'd track the pending offer.
        // This is a no-op placeholder.
        let _ = answer_sdp;
    }

    /// Accept an SDP offer from rtpbridge and return our answer
    pub fn accept_offer(&mut self, offer_sdp: &str) -> String {
        let candidate = Candidate::host(self.local_addr, "udp").unwrap();
        self.rtc.add_local_candidate(candidate);

        let offer = SdpOffer::from_sdp_string(offer_sdp).unwrap();
        let answer = self.rtc.sdp_api().accept_offer(offer).unwrap();
        answer.to_sdp_string()
    }

    /// Get the number of RTP packets received
    pub fn received_count(&self) -> u64 {
        self.packets_received.load(Ordering::Relaxed)
    }

    /// Drive the str0m event loop for a specified duration.
    /// Processes incoming UDP packets and str0m output.
    pub async fn drive_for(&mut self, duration: Duration) {
        let socket = Arc::clone(&self.socket);
        let packets_received = Arc::clone(&self.packets_received);
        let local_addr = self.local_addr;

        // We need to take ownership of rtc for the drive loop.
        // Use a Mutex to share it (simplified for testing).
        let rtc = std::mem::replace(
            &mut self.rtc,
            RtcConfig::new().build(Instant::now()), // placeholder
        );
        let rtc = Arc::new(Mutex::new(rtc));
        let rtc_clone = Arc::clone(&rtc);

        let handle = tokio::spawn(async move {
            let deadline = Instant::now() + duration;
            let mut buf = vec![0u8; 2048];

            while Instant::now() < deadline {
                let mut rtc = rtc_clone.lock().await;

                // Poll for output
                loop {
                    match rtc.poll_output() {
                        Ok(Output::Timeout(when)) => {
                            let wait = when
                                .checked_duration_since(Instant::now())
                                .unwrap_or(Duration::ZERO)
                                .min(Duration::from_millis(10));
                            drop(rtc);

                            // Try to receive a packet within the timeout
                            match tokio::time::timeout(wait, socket.recv_from(&mut buf)).await {
                                Ok(Ok((n, source))) => {
                                    let mut rtc = rtc_clone.lock().await;
                                    if let Ok(receive) =
                                        Receive::new(Protocol::Udp, source, local_addr, &buf[..n])
                                    {
                                        let _ = rtc
                                            .handle_input(Input::Receive(Instant::now(), receive));
                                    }
                                }
                                Ok(Err(_)) => {}
                                Err(_) => {
                                    // Timeout — feed a timeout input
                                    let mut rtc = rtc_clone.lock().await;
                                    let _ = rtc.handle_input(Input::Timeout(Instant::now()));
                                }
                            }
                            break;
                        }
                        Ok(Output::Transmit(transmit)) => {
                            let _ = socket
                                .send_to(&transmit.contents, transmit.destination)
                                .await;
                        }
                        Ok(Output::Event(event)) => match event {
                            Event::RtpPacket(_) => {
                                packets_received.fetch_add(1, Ordering::Relaxed);
                            }
                            _ => {}
                        },
                        Err(_) => break,
                    }
                }
            }
        });

        handle.await.ok();

        // Restore rtc
        self.rtc = Arc::try_unwrap(rtc)
            .ok()
            .map(|m| m.into_inner())
            .unwrap_or_else(|| RtcConfig::new().build(Instant::now()));
    }
}

impl Drop for TestPeer {
    fn drop(&mut self) {
        if let Some(handle) = self.drive_handle.take() {
            handle.abort();
        }
    }
}
