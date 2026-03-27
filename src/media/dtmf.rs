use std::time::{Duration, Instant};

/// DTMF event timeout: if no end-bit arrives within this duration, emit the event anyway.
const DTMF_TIMEOUT: Duration = Duration::from_secs(3);

/// RFC 4733 telephone-event payload format:
///
/// ```text
///  0                   1                   2                   3
///  0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
/// +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
/// |     event     |E|R| volume    |          duration             |
/// +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
/// ```
/// A completed DTMF event
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DtmfEvent {
    pub digit: char,
    pub duration_ms: u32,
    pub volume: u8,
}

/// Detects DTMF events from RFC 4733 telephone-event RTP payloads.
/// Tracks state across multiple packets for a single event (start → end).
#[derive(Debug, Default)]
pub struct DtmfDetector {
    /// Current event being assembled (event_id, volume, last_duration, rtp_timestamp, wall_clock_start)
    current: Option<(u8, u8, u16, u32, Instant)>,
    /// Last completed event (event_id, rtp_timestamp) to suppress RFC 4733 redundant end-bit packets
    last_completed: Option<(u8, u32)>,
}

impl DtmfDetector {
    pub fn new() -> Self {
        Self {
            current: None,
            last_completed: None,
        }
    }

    /// Process an RFC 4733 telephone-event RTP payload.
    /// Returns Some(DtmfEvent) when the end bit is set (event complete).
    ///
    /// `payload` is the RTP payload (4 bytes), `timestamp` is the RTP timestamp.
    /// `clock_rate` is the RTP clock rate for duration calculation.
    pub fn process(
        &mut self,
        payload: &[u8],
        timestamp: u32,
        clock_rate: u32,
    ) -> Option<DtmfEvent> {
        if payload.len() < 4 {
            return None;
        }

        let event_id = payload[0];
        let end_bit = (payload[1] >> 7) & 1 != 0;
        let volume = payload[1] & 0x3F;
        let duration = u16::from_be_bytes([payload[2], payload[3]]);

        // Check if current event has timed out (end-bit lost in transit)
        if let Some(timed_out) = self.drain_if_timed_out(clock_rate) {
            // The timed-out event is from a previous digit; return it immediately.
            // Re-process this packet on the next call (caller should retry, but
            // to keep the API simple we continue and may lose one packet — acceptable
            // since the new digit's first packet is typically a start packet anyway).
            self.current = Some((event_id, volume, duration, timestamp, Instant::now()));
            return Some(timed_out);
        }

        // If we have a current event with a different event_id, emit it
        // (end bit may have been lost in transit)
        let interrupted = match &self.current {
            Some((cur_id, cur_vol, cur_dur, _, _)) if *cur_id != event_id => {
                let old_id = *cur_id;
                let old_vol = *cur_vol;
                let old_dur = *cur_dur;
                self.current = None;
                event_id_to_digit(old_id).map(|d| DtmfEvent {
                    digit: d,
                    duration_ms: {
                        let rate = if clock_rate > 0 { clock_rate } else { 8000 };
                        ((old_dur as u64 * 1000) / rate as u64) as u32
                    },
                    volume: old_vol,
                })
            }
            _ => None,
        };

        self.current = Some((event_id, volume, duration, timestamp, Instant::now()));

        if end_bit {
            if interrupted.is_some() {
                // Prioritize the interrupted event. The new digit stays in
                // self.current (set at line above) and will be completed by
                // a redundant end-bit copy (RFC 4733 mandates 3 copies).
                return interrupted;
            }
            self.current = None;
            // Suppress duplicate end-bit packets (RFC 4733 mandates 3 redundant copies)
            if self.last_completed == Some((event_id, timestamp)) {
                return None;
            }
            self.last_completed = Some((event_id, timestamp));
            let digit = event_id_to_digit(event_id)?;
            let rate = if clock_rate > 0 { clock_rate } else { 8000 };
            let duration_ms = ((duration as u64 * 1000) / rate as u64) as u32;
            return Some(DtmfEvent {
                digit,
                duration_ms,
                volume,
            });
        }

        // Return the interrupted event if we had one
        interrupted
    }

    /// Check for timed-out DTMF events. Call this periodically (e.g., every 20ms)
    /// even when no new packets arrive, to detect events whose end-bit was lost.
    pub fn check_timeout(&mut self, clock_rate: u32) -> Option<DtmfEvent> {
        self.drain_if_timed_out(clock_rate)
    }

    /// Returns true if the detector has an active (in-progress) digit.
    pub fn has_active_digit(&self) -> bool {
        self.current.is_some()
    }

    /// If the current event has been alive longer than DTMF_TIMEOUT, emit it and clear state.
    fn drain_if_timed_out(&mut self, clock_rate: u32) -> Option<DtmfEvent> {
        if let Some((cur_id, cur_vol, cur_dur, _, started)) = &self.current {
            if started.elapsed() > DTMF_TIMEOUT {
                let id = *cur_id;
                let vol = *cur_vol;
                let dur = *cur_dur;
                self.current = None;
                let rate = if clock_rate > 0 { clock_rate } else { 8000 };
                return event_id_to_digit(id).map(|d| DtmfEvent {
                    digit: d,
                    duration_ms: ((dur as u64 * 1000) / rate as u64) as u32,
                    volume: vol,
                });
            }
        }
        None
    }
}

/// Generates RFC 4733 telephone-event RTP payloads for DTMF injection.
pub struct DtmfGenerator;

impl DtmfGenerator {
    /// Generate a sequence of telephone-event RTP payloads for a DTMF digit.
    ///
    /// Returns a vec of DTMF RTP packets.
    /// The caller is responsible for setting the RTP timestamp (same for all packets
    /// in one event) and incrementing sequence numbers.
    ///
    /// `ptime_ms` is the packet interval (typically 20ms).
    /// `clock_rate` is the RTP clock rate for the telephone-event PT.
    pub fn generate(
        digit: char,
        duration_ms: u32,
        volume: u8,
        clock_rate: u32,
        ptime_ms: u32,
    ) -> Option<Vec<DtmfPacket>> {
        let event_id = digit_to_event_id(digit)?;

        if ptime_ms == 0 || clock_rate == 0 {
            return None;
        }

        let total_duration_ticks = ((duration_ms as u64 * clock_rate as u64) / 1000) as u32;
        let ptime_ticks = ((ptime_ms as u64 * clock_rate as u64) / 1000) as u32;
        let num_packets = duration_ms.div_ceil(ptime_ms);

        let mut packets = Vec::new();

        for i in 0..num_packets {
            let is_first = i == 0;
            let is_last = i == num_packets - 1;
            let current_duration = if is_last {
                total_duration_ticks.min(u16::MAX as u32) as u16
            } else {
                ((i + 1) * ptime_ticks).min(u16::MAX as u32) as u16
            };

            // For end events, send 3 times (RFC 4733 §2.5.1.4)
            if is_last {
                for rep in 0..3u8 {
                    packets.push(DtmfPacket {
                        payload: build_telephone_event(event_id, true, volume, current_duration),
                        marker: is_first && rep == 0,
                    });
                }
            } else {
                packets.push(DtmfPacket {
                    payload: build_telephone_event(event_id, false, volume, current_duration),
                    marker: is_first,
                });
            }
        }

        Some(packets)
    }
}

/// A single DTMF RTP packet
#[derive(Debug, Clone)]
pub struct DtmfPacket {
    pub payload: Vec<u8>,
    pub marker: bool,
}

/// Build a 4-byte telephone-event payload
fn build_telephone_event(event_id: u8, end: bool, volume: u8, duration: u16) -> Vec<u8> {
    let mut buf = vec![0u8; 4];
    buf[0] = event_id;
    buf[1] = if end { 0x80 } else { 0x00 } | (volume & 0x3F);
    buf[2] = (duration >> 8) as u8;
    buf[3] = (duration & 0xFF) as u8;
    buf
}

/// Map event ID (0-15) to DTMF digit character
fn event_id_to_digit(id: u8) -> Option<char> {
    match id {
        0 => Some('0'),
        1 => Some('1'),
        2 => Some('2'),
        3 => Some('3'),
        4 => Some('4'),
        5 => Some('5'),
        6 => Some('6'),
        7 => Some('7'),
        8 => Some('8'),
        9 => Some('9'),
        10 => Some('*'),
        11 => Some('#'),
        12 => Some('A'),
        13 => Some('B'),
        14 => Some('C'),
        15 => Some('D'),
        _ => None,
    }
}

/// Map DTMF digit character to event ID (0-15)
fn digit_to_event_id(digit: char) -> Option<u8> {
    match digit {
        '0' => Some(0),
        '1' => Some(1),
        '2' => Some(2),
        '3' => Some(3),
        '4' => Some(4),
        '5' => Some(5),
        '6' => Some(6),
        '7' => Some(7),
        '8' => Some(8),
        '9' => Some(9),
        '*' => Some(10),
        '#' => Some(11),
        'A' | 'a' => Some(12),
        'B' | 'b' => Some(13),
        'C' | 'c' => Some(14),
        'D' | 'd' => Some(15),
        _ => None,
    }
}

/// Check if an RTP payload type is a telephone-event PT
pub fn is_telephone_event_pt(pt: u8, negotiated_te_pt: Option<u8>) -> bool {
    if let Some(te_pt) = negotiated_te_pt {
        pt == te_pt
    } else {
        // Common default PTs for telephone-event
        pt == 101 || pt == 96 || pt == 126
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_dtmf() {
        let mut detector = DtmfDetector::new();

        // Start of digit '5' (event_id=5, no end bit, volume=10, duration=160)
        let payload1 = build_telephone_event(5, false, 10, 160);
        assert!(detector.process(&payload1, 1000, 8000).is_none());

        // Continuation (duration=320)
        let payload2 = build_telephone_event(5, false, 10, 320);
        assert!(detector.process(&payload2, 1000, 8000).is_none());

        // End bit set (duration=480)
        let payload3 = build_telephone_event(5, true, 10, 480);
        let event = detector.process(&payload3, 1000, 8000).unwrap();
        assert_eq!(event.digit, '5');
        assert_eq!(event.duration_ms, 60); // 480 ticks at 8000Hz = 60ms
        assert_eq!(event.volume, 10);
    }

    #[test]
    fn test_detect_star_hash() {
        let mut detector = DtmfDetector::new();

        let payload = build_telephone_event(10, true, 10, 1280);
        let event = detector.process(&payload, 1000, 8000).unwrap();
        assert_eq!(event.digit, '*');

        let payload = build_telephone_event(11, true, 10, 1280);
        let event = detector.process(&payload, 2000, 8000).unwrap();
        assert_eq!(event.digit, '#');
    }

    #[test]
    fn test_generate_dtmf() {
        let packets = DtmfGenerator::generate('5', 160, 10, 8000, 20).unwrap();

        // 160ms / 20ms = 8 regular packets + 3 end packets (replacing last) = 7 + 3 = 10
        assert!(
            packets.len() >= 9,
            "expected >=9 packets, got {}",
            packets.len()
        );

        // First packet should have marker bit
        assert!(packets[0].marker);

        // Last packets should have end bit
        let last = &packets[packets.len() - 1];
        // End bit is in payload byte 1 bit 7
        assert!(
            last.payload[1] & 0x80 != 0,
            "last packet should have end bit set"
        );

        // Verify payload structure
        assert_eq!(packets[0].payload.len(), 4);
        assert_eq!(packets[0].payload[0], 5); // event_id for '5'
    }

    #[test]
    fn test_roundtrip() {
        let packets = DtmfGenerator::generate('#', 100, 10, 8000, 20).unwrap();
        let mut detector = DtmfDetector::new();

        let mut detected = None;
        for pkt in &packets {
            if let Some(evt) = detector.process(&pkt.payload, 5000, 8000) {
                detected = Some(evt);
            }
        }

        let event = detected.expect("should have detected DTMF");
        assert_eq!(event.digit, '#');
    }

    #[test]
    fn test_all_digits() {
        for digit in "0123456789*#ABCD".chars() {
            let id = digit_to_event_id(digit).unwrap();
            let back = event_id_to_digit(id).unwrap();
            assert_eq!(back, digit.to_ascii_uppercase());
        }
    }

    #[test]
    fn test_invalid_digit() {
        assert!(digit_to_event_id('X').is_none());
        assert!(DtmfGenerator::generate('X', 100, 10, 8000, 20).is_none());
    }

    #[test]
    fn test_too_short_payload() {
        let mut detector = DtmfDetector::new();
        assert!(detector.process(&[0, 1, 2], 0, 8000).is_none());
    }

    #[test]
    fn test_is_telephone_event_pt_negotiated() {
        // With negotiated PT=96, only 96 should match
        assert!(is_telephone_event_pt(96, Some(96)));
        assert!(!is_telephone_event_pt(101, Some(96)));
        assert!(!is_telephone_event_pt(126, Some(96)));

        // Without negotiated PT, common defaults (101, 96, 126) should match
        assert!(is_telephone_event_pt(101, None));
        assert!(is_telephone_event_pt(96, None));
        assert!(is_telephone_event_pt(126, None));
        assert!(!is_telephone_event_pt(97, None));
    }

    #[test]
    fn test_interrupted_event() {
        let mut detector = DtmfDetector::new();

        // Start digit '1' (event_id=1, no end bit, volume=10, duration=160)
        let payload1 = build_telephone_event(1, false, 10, 160);
        assert!(detector.process(&payload1, 1000, 8000).is_none());

        // Continue digit '1' (duration=320)
        let payload2 = build_telephone_event(1, false, 10, 320);
        assert!(detector.process(&payload2, 1000, 8000).is_none());

        // New digit '2' starts WITHOUT end bit for '1' (lost end packet)
        let payload3 = build_telephone_event(2, false, 10, 160);
        let event = detector.process(&payload3, 2000, 8000);
        // Digit '1' should be emitted as an interrupted event
        let event = event.expect("interrupted digit '1' should be emitted");
        assert_eq!(event.digit, '1');
        assert_eq!(event.duration_ms, 40); // 320 ticks at 8000Hz = 40ms
        assert_eq!(event.volume, 10);

        // Now end digit '2' normally
        let payload4 = build_telephone_event(2, true, 10, 480);
        let event = detector.process(&payload4, 2000, 8000).unwrap();
        assert_eq!(event.digit, '2');
        assert_eq!(event.duration_ms, 60); // 480 ticks at 8000Hz = 60ms
    }

    #[test]
    fn test_zero_clock_rate() {
        let mut detector = DtmfDetector::new();

        // With clock_rate=0, fall back to 8000 Hz for conversion
        let payload = build_telephone_event(5, true, 10, 480);
        let event = detector.process(&payload, 1000, 0).unwrap();
        assert_eq!(event.digit, '5');
        assert_eq!(event.duration_ms, 60); // 480 ticks at default 8000Hz = 60ms
        assert_eq!(event.volume, 10);
    }

    /// Test: DtmfDetector correctly detects all 16 DTMF digits (0-9, *, #, A-D).
    /// For each digit, feed an event with end_bit=true and verify the detected
    /// digit matches the expected character.
    #[test]
    fn test_all_dtmf_digits_detected() {
        let expected: &[(u8, char)] = &[
            (0, '0'),
            (1, '1'),
            (2, '2'),
            (3, '3'),
            (4, '4'),
            (5, '5'),
            (6, '6'),
            (7, '7'),
            (8, '8'),
            (9, '9'),
            (10, '*'),
            (11, '#'),
            (12, 'A'),
            (13, 'B'),
            (14, 'C'),
            (15, 'D'),
        ];

        let mut detector = DtmfDetector::new();
        for (event_id, expected_digit) in expected {
            // Use a unique timestamp per digit so the detector does not suppress
            // duplicates (last_completed dedup uses (event_id, timestamp))
            let timestamp = *event_id as u32 * 1000;
            let payload = build_telephone_event(*event_id, true, 10, 800);
            let event = detector
                .process(&payload, timestamp, 8000)
                .unwrap_or_else(|| {
                    panic!(
                        "DtmfDetector should detect event_id={} as digit '{}'",
                        event_id, expected_digit
                    )
                });
            assert_eq!(
                event.digit, *expected_digit,
                "event_id={} should map to digit '{}', got '{}'",
                event_id, expected_digit, event.digit
            );
            assert_eq!(event.volume, 10);
            assert_eq!(event.duration_ms, 100); // 800 ticks at 8000Hz = 100ms
        }
    }

    #[test]
    fn test_timeout_emits_event() {
        let mut detector = DtmfDetector::new();
        let payload = build_telephone_event(5, false, 10, 800);
        assert!(detector.process(&payload, 1000, 8000).is_none());

        // Simulate 4 seconds elapsed by replacing the start time
        if let Some(ref mut cur) = detector.current {
            cur.4 = Instant::now() - Duration::from_secs(4);
        }

        let event = detector.check_timeout(8000);
        assert!(event.is_some(), "timed-out event should be emitted");
        let event = event.unwrap();
        assert_eq!(event.digit, '5');
        assert_eq!(event.duration_ms, 100); // 800 / 8000 * 1000
        assert_eq!(event.volume, 10);

        // State should be cleared
        assert!(detector.current.is_none());
    }

    #[test]
    fn test_no_timeout_before_3_seconds() {
        let mut detector = DtmfDetector::new();
        let payload = build_telephone_event(5, false, 10, 800);
        assert!(detector.process(&payload, 1000, 8000).is_none());

        // No time manipulation — should not time out
        assert!(detector.check_timeout(8000).is_none());
        // State should still be active
        assert!(detector.current.is_some());
    }

    #[test]
    fn test_timeout_during_process_emits_old_event() {
        let mut detector = DtmfDetector::new();
        // Start digit '5'
        let payload1 = build_telephone_event(5, false, 10, 800);
        assert!(detector.process(&payload1, 1000, 8000).is_none());

        // Simulate timeout
        if let Some(ref mut cur) = detector.current {
            cur.4 = Instant::now() - Duration::from_secs(4);
        }

        // Process a new digit '3' — should emit timed-out '5' first
        let payload2 = build_telephone_event(3, false, 10, 160);
        let event = detector.process(&payload2, 2000, 8000);
        assert!(event.is_some(), "timed-out digit '5' should be emitted");
        assert_eq!(event.unwrap().digit, '5');

        // Current should now be digit '3'
        assert!(detector.current.is_some());
        assert_eq!(detector.current.as_ref().unwrap().0, 3);
    }

    #[test]
    fn test_interrupted_event_not_dropped_on_end_bit() {
        let mut detector = DtmfDetector::new();

        // Start digit '1' (no end bit)
        let payload1 = build_telephone_event(1, false, 10, 160);
        assert!(detector.process(&payload1, 1000, 8000).is_none());

        // New digit '2' arrives with end-bit immediately (e.g., very short digit)
        let payload2 = build_telephone_event(2, true, 10, 480);
        let event = detector.process(&payload2, 2000, 8000);
        // Should get the interrupted '1' event, NOT the '2' end event
        let event = event.expect("interrupted digit '1' should be emitted");
        assert_eq!(event.digit, '1');

        // Digit '2' is still in self.current. A redundant end-bit copy completes it.
        let payload3 = build_telephone_event(2, true, 10, 480);
        let event = detector.process(&payload3, 2000, 8000);
        let event = event.expect("digit '2' end should be emitted on redundant copy");
        assert_eq!(event.digit, '2');
        assert_eq!(event.duration_ms, 60);
    }
}
