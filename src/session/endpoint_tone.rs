use super::endpoint::EndpointConfig;
use super::stats::EndpointStats;
use crate::control::protocol::{EndpointDirection, EndpointId, EndpointState};

/// Type of tone to generate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToneType {
    /// US ringback: 440Hz + 480Hz, 2s on / 4s off
    Ringback,
    /// Continuous ringing: 440Hz + 480Hz, no cadence
    Ringing,
    /// US busy: 480Hz + 620Hz, 0.5s on / 0.5s off
    Busy,
    /// Custom single-frequency continuous tone
    Sine,
}

/// Cadence definition: on_ms / off_ms pattern. None = continuous.
struct Cadence {
    on_ms: u64,
    off_ms: u64,
}

impl ToneType {
    fn frequencies(&self, custom_freq: Option<f64>) -> (f64, Option<f64>) {
        match self {
            ToneType::Ringback => (440.0, Some(480.0)),
            ToneType::Ringing => (440.0, Some(480.0)),
            ToneType::Busy => (480.0, Some(620.0)),
            ToneType::Sine => (custom_freq.unwrap_or(440.0), None),
        }
    }

    fn cadence(&self) -> Option<Cadence> {
        match self {
            ToneType::Ringback => Some(Cadence {
                on_ms: 2000,
                off_ms: 4000,
            }),
            ToneType::Busy => Some(Cadence {
                on_ms: 500,
                off_ms: 500,
            }),
            ToneType::Ringing | ToneType::Sine => None, // continuous
        }
    }
}

/// A tone generator endpoint that synthesizes audio on-the-fly.
/// Always send-only. Produces PCM samples on a 20ms poll timer.
pub struct ToneEndpoint {
    pub id: EndpointId,
    pub config: EndpointConfig,
    pub state: EndpointState,
    pub stats: EndpointStats,

    #[allow(dead_code)]
    tone_type: ToneType,
    freq1: f64,
    freq2: Option<f64>,
    cadence: Option<Cadence>,
    amplitude: f64,

    /// Total duration limit in ms. None = play until removed.
    duration_ms: Option<u64>,
    /// Milliseconds elapsed since playback started.
    elapsed_ms: u64,
    /// Running sample offset for continuous phase (avoids clicks between calls).
    sample_offset: u64,
}

impl ToneEndpoint {
    /// Create a new tone endpoint.
    pub fn new(
        id: EndpointId,
        tone_type: ToneType,
        frequency: Option<f64>,
        duration_ms: Option<u64>,
    ) -> Self {
        let (freq1, freq2) = tone_type.frequencies(frequency);
        let cadence = tone_type.cadence();

        Self {
            id,
            config: EndpointConfig {
                direction: EndpointDirection::SendOnly,
            },
            state: EndpointState::Playing,
            stats: EndpointStats::new(),
            tone_type,
            freq1,
            freq2,
            cadence,
            amplitude: 8000.0,
            duration_ms,
            elapsed_ms: 0,
            sample_offset: 0,
        }
    }

    /// Returns the tone type.
    #[allow(dead_code)]
    pub fn tone_type(&self) -> ToneType {
        self.tone_type
    }

    /// Generate the next chunk of PCM samples (mono i16 at 8kHz).
    /// Returns None when playback is finished or the tone is in the off phase
    /// of its cadence (silence is indicated by returning a zero-filled buffer).
    pub fn next_pcm(&mut self, target_samples: usize) -> Option<Vec<i16>> {
        if self.state != EndpointState::Playing {
            return None;
        }

        // Check duration limit
        if let Some(limit) = self.duration_ms {
            if self.elapsed_ms >= limit {
                self.state = EndpointState::Finished;
                return None;
            }
        }

        let in_on_phase = match &self.cadence {
            None => true,
            Some(c) => {
                let cycle = c.on_ms + c.off_ms;
                let pos = self.elapsed_ms % cycle;
                pos < c.on_ms
            }
        };

        self.elapsed_ms += 20; // 20ms per poll

        if !in_on_phase {
            // During off phase, produce silence
            return Some(vec![0i16; target_samples]);
        }

        let mut pcm = Vec::with_capacity(target_samples);
        for i in 0..target_samples {
            let t = (self.sample_offset + i as u64) as f64 / 8000.0;
            let mut sample = f64::sin(2.0 * std::f64::consts::PI * self.freq1 * t) * self.amplitude;
            if let Some(f2) = self.freq2 {
                sample += f64::sin(2.0 * std::f64::consts::PI * f2 * t) * self.amplitude;
                // Dual tone: halve amplitude to avoid clipping
                sample *= 0.5;
            }
            pcm.push(sample.clamp(i16::MIN as f64, i16::MAX as f64) as i16);
        }
        self.sample_offset += target_samples as u64;

        Some(pcm)
    }

    /// Sample rate is always 8kHz (PCMU output).
    pub fn sample_rate(&self) -> u32 {
        8000
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sine_tone_generates_pcm() {
        let mut ep = ToneEndpoint::new(EndpointId::new_v4(), ToneType::Sine, Some(440.0), None);
        let pcm = ep.next_pcm(160).expect("should produce PCM");
        assert_eq!(pcm.len(), 160);
        // Should have non-zero samples (sine wave)
        assert!(
            pcm.iter().any(|&s| s != 0),
            "sine should produce non-zero samples"
        );
    }

    #[test]
    fn test_ringback_cadence_on_off() {
        let mut ep = ToneEndpoint::new(EndpointId::new_v4(), ToneType::Ringback, None, None);

        // First 2 seconds (100 frames at 20ms each) should have audio
        let mut has_nonzero = false;
        for _ in 0..100 {
            let pcm = ep.next_pcm(160).expect("should produce PCM");
            if pcm.iter().any(|&s| s != 0) {
                has_nonzero = true;
            }
        }
        assert!(has_nonzero, "on-phase should have non-zero samples");

        // Next 4 seconds (200 frames) should be silence (off phase)
        for _ in 0..200 {
            let pcm = ep.next_pcm(160).expect("should produce PCM in off phase");
            assert!(pcm.iter().all(|&s| s == 0), "off-phase should be silence");
        }

        // Then on again
        let pcm = ep.next_pcm(160).expect("should resume");
        assert!(pcm.iter().any(|&s| s != 0), "should resume after off phase");
    }

    #[test]
    fn test_duration_limit() {
        let mut ep =
            ToneEndpoint::new(EndpointId::new_v4(), ToneType::Sine, Some(440.0), Some(100));

        // 100ms = 5 frames (elapsed: 0→20→40→60→80 after each next_pcm call)
        for _ in 0..5 {
            assert!(ep.next_pcm(160).is_some());
        }
        // 6th call: elapsed=100 >= limit=100 → Finished
        assert!(
            ep.next_pcm(160).is_none(),
            "should return None after duration expires"
        );
        assert_eq!(ep.state, EndpointState::Finished);
    }

    #[test]
    fn test_continuous_ringing() {
        let mut ep = ToneEndpoint::new(EndpointId::new_v4(), ToneType::Ringing, None, None);
        // Should produce non-zero audio continuously (no cadence)
        for _ in 0..500 {
            let pcm = ep.next_pcm(160).expect("should produce PCM");
            assert!(pcm.iter().any(|&s| s != 0), "ringing should be continuous");
        }
    }

    #[test]
    fn test_busy_cadence() {
        let mut ep = ToneEndpoint::new(EndpointId::new_v4(), ToneType::Busy, None, None);

        // 0.5s on = 25 frames
        for _ in 0..25 {
            let pcm = ep.next_pcm(160).expect("should produce PCM");
            assert!(pcm.iter().any(|&s| s != 0), "busy on-phase");
        }
        // 0.5s off = 25 frames
        for _ in 0..25 {
            let pcm = ep.next_pcm(160).expect("should produce PCM");
            assert!(pcm.iter().all(|&s| s == 0), "busy off-phase");
        }
    }

    #[test]
    fn test_dual_tone_amplitude() {
        let mut ep = ToneEndpoint::new(EndpointId::new_v4(), ToneType::Ringing, None, None);
        let pcm = ep.next_pcm(160).unwrap();
        // Dual tone at 8000 amplitude each, halved = ~8000 peak
        let max_abs = pcm
            .iter()
            .map(|&s| (s as i32).unsigned_abs())
            .max()
            .unwrap_or(0);
        assert!(
            max_abs < 10000,
            "dual tone amplitude should be controlled, got {max_abs}"
        );
    }
}
