use earshot::{DefaultPredictor, Detector};

use super::resample::Resampler;

/// VAD monitor for a single endpoint. Decodes RTP audio to PCM,
/// resamples to 16kHz, feeds 256-sample frames to earshot.
/// Completely independent of recording.
///
/// Silence duration is tracked in audio samples (at 16kHz) rather than wall
/// clock time, so it is independent of processing jitter and packet arrival
/// timing.
pub struct VadMonitor {
    detector: Detector<DefaultPredictor>,
    resampler: Option<Resampler>,
    pcm_buffer: Vec<i16>,
    resample_buf: Vec<i16>,
    speech_threshold: f32,
    silence_interval_ms: u32,

    /// Current state
    is_speaking: bool,
    /// Whether a speech→silence transition has occurred (gates silence events)
    in_silence: bool,
    /// 16kHz samples processed while in silence (since speech→silence transition)
    silence_samples: u64,
    /// 16kHz samples processed since last silence event was emitted
    samples_since_last_event: u64,
}

/// Events emitted by the VAD monitor
#[derive(Debug)]
pub enum VadEvent {
    SpeechStarted,
    Silence { duration_ms: u64 },
}

impl VadMonitor {
    /// Create a new VAD monitor.
    /// `source_sample_rate`: the sample rate of the decoded PCM being fed (8000, 16000, or 48000)
    pub fn new(source_sample_rate: u32, speech_threshold: f32, silence_interval_ms: u32) -> Self {
        let resampler = if source_sample_rate == 0 {
            tracing::warn!("VadMonitor created with sample_rate 0, resampling disabled");
            None
        } else if source_sample_rate != 16000 {
            Some(Resampler::new(source_sample_rate, 16000))
        } else {
            None
        };

        Self {
            detector: Detector::new(DefaultPredictor::new()),
            resampler,
            pcm_buffer: Vec::with_capacity(512),
            resample_buf: Vec::with_capacity(512),
            speech_threshold,
            silence_interval_ms,
            is_speaking: false,
            in_silence: false,
            silence_samples: 0,
            samples_since_last_event: 0,
        }
    }

    /// Feed decoded PCM samples (at source sample rate).
    /// Returns any VAD events that should be emitted.
    pub fn process(&mut self, pcm: &[i16]) -> Vec<VadEvent> {
        // Resample to 16kHz if needed
        if let Some(resampler) = &mut self.resampler {
            resampler.process(pcm, &mut self.resample_buf);
            self.pcm_buffer.extend_from_slice(&self.resample_buf);
        } else {
            self.pcm_buffer.extend_from_slice(pcm);
        };

        let mut events = Vec::new();

        // Process 256-sample frames (at 16kHz each frame = 16ms)
        while self.pcm_buffer.len() >= 256 {
            let mut frame = [0i16; 256];
            frame.copy_from_slice(&self.pcm_buffer[..256]);
            self.pcm_buffer.drain(..256);
            let score = self.detector.predict_i16(&frame);

            let is_speech = score >= self.speech_threshold;

            if is_speech && !self.is_speaking {
                // Transition: silence → speech
                self.is_speaking = true;
                self.in_silence = false;
                self.silence_samples = 0;
                self.samples_since_last_event = 0;
                events.push(VadEvent::SpeechStarted);
            } else if !is_speech && self.is_speaking {
                // Transition: speech → silence
                self.is_speaking = false;
                self.in_silence = true;
                self.silence_samples = 0;
                self.samples_since_last_event = 0;
            }

            // Accumulate silence samples and emit periodic events
            if !self.is_speaking && self.in_silence {
                self.silence_samples += 256;
                self.samples_since_last_event += 256;

                let since_last_ms = self.samples_since_last_event * 1000 / 16000;
                if since_last_ms >= self.silence_interval_ms as u64 {
                    self.samples_since_last_event = 0;
                    let silence_ms = self.silence_samples * 1000 / 16000;
                    events.push(VadEvent::Silence {
                        duration_ms: silence_ms,
                    });
                }
            }
        }

        events
    }

    /// Flush any remaining samples (<256) at stream end.
    /// Zero-pads to a full frame and processes it.
    #[allow(dead_code)] // used in tests
    pub fn flush(&mut self) -> Vec<VadEvent> {
        if self.pcm_buffer.is_empty() {
            return Vec::new();
        }
        // Zero-pad to 256 samples
        self.pcm_buffer.resize(256, 0);
        let frame: Vec<i16> = self.pcm_buffer.drain(..).collect();
        let score = self.detector.predict_i16(&frame);
        let is_speech = score >= self.speech_threshold;
        let mut events = Vec::new();

        if is_speech && !self.is_speaking {
            self.is_speaking = true;
            self.in_silence = false;
            self.silence_samples = 0;
            self.samples_since_last_event = 0;
            events.push(VadEvent::SpeechStarted);
        } else if !is_speech && self.is_speaking {
            self.is_speaking = false;
            self.in_silence = true;
            self.silence_samples = 0;
            self.samples_since_last_event = 0;
        }

        if !self.is_speaking && self.in_silence {
            self.silence_samples += 256;
            let silence_ms = self.silence_samples * 1000 / 16000;
            events.push(VadEvent::Silence {
                duration_ms: silence_ms,
            });
        }

        events
    }

    /// Reset the VAD state
    #[allow(dead_code)] // used in tests
    pub fn reset(&mut self) {
        self.detector.reset();
        self.pcm_buffer.clear();
        self.resample_buf.clear();
        self.is_speaking = false;
        self.in_silence = false;
        self.silence_samples = 0;
        self.samples_since_last_event = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Generate a sine wave at the given frequency and sample rate
    fn sine_wave(freq_hz: f64, sample_rate: u32, num_samples: usize, amplitude: f64) -> Vec<i16> {
        (0..num_samples)
            .map(|i| {
                let t = i as f64 / sample_rate as f64;
                (f64::sin(2.0 * std::f64::consts::PI * freq_hz * t) * amplitude) as i16
            })
            .collect()
    }

    #[test]
    fn test_vad_silence() {
        // Silence events only fire after a speech→silence transition.
        // So we trigger speech first, then go silent.
        // silence_interval_ms=50 → need 800 samples at 16kHz (50ms * 16 samples/ms)
        let mut vad = VadMonitor::new(16000, 0.5, 50);

        // Trigger speech with a loud sine wave
        let speech = sine_wave(440.0, 16000, 8000, 25000.0);
        for chunk in speech.chunks(1024) {
            vad.process(chunk);
        }

        // Feed enough silence to exceed 50ms in audio time.
        // First chunk triggers the speech→silence transition, subsequent chunks
        // accumulate silence samples. We need >800 samples of silence after
        // the transition frame to exceed the 50ms interval.
        let silence = vec![0i16; 4096]; // 256ms of silence at 16kHz
        let mut all_events = Vec::new();
        all_events.extend(vad.process(&silence));

        let silence_events: Vec<_> = all_events
            .iter()
            .filter(|e| matches!(e, VadEvent::Silence { .. }))
            .collect();
        assert!(
            !silence_events.is_empty(),
            "expected Silence event after speech→silence transition with enough audio samples"
        );
        if let VadEvent::Silence { duration_ms } = silence_events[0] {
            assert!(*duration_ms >= 50, "silence duration should be >= 50ms");
        }
    }

    #[test]
    fn test_vad_speech_detection() {
        let mut vad = VadMonitor::new(16000, 0.5, 1000);

        // Generate a loud 440Hz sine wave — should trigger speech detection
        let speech = sine_wave(440.0, 16000, 8000, 25000.0); // 500ms of loud tone

        let mut all_events = Vec::new();
        // Feed in chunks to give the detector multiple frames
        for chunk in speech.chunks(1024) {
            all_events.extend(vad.process(chunk));
        }

        let speech_started = all_events
            .iter()
            .any(|e| matches!(e, VadEvent::SpeechStarted));
        assert!(
            speech_started,
            "expected SpeechStarted event from loud sine wave, got {:?}",
            all_events
        );
    }

    #[test]
    fn test_vad_speech_to_silence() {
        let mut vad = VadMonitor::new(16000, 0.5, 50);

        // Feed loud speech to trigger SpeechStarted
        let speech = sine_wave(440.0, 16000, 8000, 25000.0);
        let mut all_events = Vec::new();
        for chunk in speech.chunks(1024) {
            all_events.extend(vad.process(chunk));
        }
        assert!(
            all_events
                .iter()
                .any(|e| matches!(e, VadEvent::SpeechStarted)),
            "expected SpeechStarted from speech input"
        );

        // Feed enough silence to trigger Silence event (>50ms = >800 samples at 16kHz)
        let silence = vec![0i16; 4096];
        let events = vad.process(&silence);
        let has_silence = events.iter().any(|e| matches!(e, VadEvent::Silence { .. }));
        assert!(
            has_silence,
            "expected Silence event after speech→silence transition"
        );
    }

    #[test]
    fn test_vad_with_resampling() {
        // 8kHz input resampled to 16kHz internally
        let mut vad = VadMonitor::new(8000, 0.5, 50);

        // Trigger speech at 8kHz: loud sine (resampled internally to 16kHz)
        let speech = sine_wave(440.0, 8000, 4000, 25000.0); // 500ms
        for chunk in speech.chunks(800) {
            vad.process(chunk);
        }

        // Feed enough silence to exceed 50ms in audio time
        // At 8kHz, 1600 samples = 200ms → resampled to 3200 samples at 16kHz
        let silence = vec![0i16; 1600];
        let events = vad.process(&silence);
        let has_silence = events.iter().any(|e| matches!(e, VadEvent::Silence { .. }));
        assert!(
            has_silence,
            "expected Silence event through 8kHz resample path"
        );
    }

    #[test]
    fn test_vad_48k_input() {
        // 48kHz input downsampled to 16kHz internally
        let mut vad = VadMonitor::new(48000, 0.5, 50);

        // Trigger speech at 48kHz
        let speech = sine_wave(440.0, 48000, 24000, 25000.0); // 500ms
        for chunk in speech.chunks(4800) {
            vad.process(chunk);
        }

        // Feed enough silence to exceed 50ms in audio time
        // At 48kHz, 9600 samples = 200ms → resampled to 3200 samples at 16kHz
        let silence = vec![0i16; 9600];
        let events = vad.process(&silence);
        let has_silence = events.iter().any(|e| matches!(e, VadEvent::Silence { .. }));
        assert!(
            has_silence,
            "expected Silence event through 48kHz resample path"
        );
    }

    #[test]
    fn test_vad_flush() {
        let mut vad = VadMonitor::new(16000, 0.5, 1000);

        // Feed less than 256 samples (one frame)
        let input = vec![0i16; 100];
        let events = vad.process(&input);
        assert!(
            events.is_empty(),
            "partial frame should not produce events from process()"
        );

        // flush() should zero-pad to 256 and process the frame
        let events = vad.flush();
        // After flush, pcm_buffer should be empty
        assert!(vad.pcm_buffer.is_empty(), "flush should drain pcm_buffer");
        // Events may or may not contain silence (depends on in_silence state)
        // but it should not panic
        let _ = events;
    }

    #[test]
    fn test_vad_double_reset() {
        // Calling reset() twice should not panic or corrupt state.
        let mut vad = VadMonitor::new(16000, 0.5, 1000);

        // Trigger speech first
        let speech = sine_wave(440.0, 16000, 8000, 25000.0);
        for chunk in speech.chunks(1024) {
            vad.process(chunk);
        }

        vad.reset();
        vad.reset(); // second reset — should be a no-op, not panic

        // State should be fully cleared — feeding silence should produce no events
        let silence = vec![0i16; 4096];
        let events = vad.process(&silence);
        let has_silence = events.iter().any(|e| matches!(e, VadEvent::Silence { .. }));
        assert!(!has_silence, "double reset should leave clean state");
    }

    #[test]
    fn test_vad_flush_before_any_input() {
        // Flushing without ever feeding data should not panic
        let mut vad = VadMonitor::new(16000, 0.5, 1000);
        let events = vad.flush();
        assert!(
            events.is_empty(),
            "flushing with no prior input should produce no events"
        );
    }

    #[test]
    fn test_vad_process_silence_only_no_speech_event() {
        // Feeding only silence (no prior speech) should never emit SpeechStarted
        // or Silence events (Silence requires a speech→silence transition).
        let mut vad = VadMonitor::new(16000, 0.5, 50);
        let silence = vec![0i16; 16000]; // 1 second of silence
        let mut all_events = Vec::new();
        for chunk in silence.chunks(1024) {
            all_events.extend(vad.process(chunk));
        }
        let has_speech = all_events
            .iter()
            .any(|e| matches!(e, VadEvent::SpeechStarted));
        assert!(
            !has_speech,
            "silence-only input should not trigger SpeechStarted"
        );
    }

    #[test]
    fn test_vad_reset() {
        let mut vad = VadMonitor::new(16000, 0.5, 1000);

        // Trigger speech — feed enough loud audio for the detector to flag it
        let speech = sine_wave(440.0, 16000, 8000, 25000.0);
        let mut found_speech = false;
        for chunk in speech.chunks(1024) {
            let events = vad.process(chunk);
            if events.iter().any(|e| matches!(e, VadEvent::SpeechStarted)) {
                found_speech = true;
            }
        }
        assert!(found_speech, "should have detected SpeechStarted");

        // Reset
        vad.reset();

        // After reset, feeding silence should NOT produce a Silence event
        // (because in_silence was cleared — no speech→silence transition happened)
        let silence = vec![0i16; 4096];
        let events = vad.process(&silence);
        let has_silence = events.iter().any(|e| matches!(e, VadEvent::Silence { .. }));
        assert!(
            !has_silence,
            "reset should clear state, no Silence event without prior speech"
        );
    }

    #[test]
    fn test_vad_silence_independent_of_wall_clock() {
        // Verify silence duration is based on audio samples, not wall clock.
        // Feed silence in irregular bursts with no sleep between them.
        // Use a larger interval (200ms) to avoid sensitivity to detector transition latency.
        let mut vad = VadMonitor::new(16000, 0.5, 200);

        // Trigger speech
        let speech = sine_wave(440.0, 16000, 8000, 25000.0);
        for chunk in speech.chunks(1024) {
            vad.process(chunk);
        }

        // Feed silence in small bursts — all processed instantly (no wall-clock delay).
        // The detector may take a few frames to transition from speech to silence,
        // so we feed plenty of silence and just verify the final event has a
        // duration based on sample count, not wall-clock time.
        let mut all_events = Vec::new();
        // Feed 8000 samples of silence = 500ms audio time in small chunks
        for _ in 0..31 {
            all_events.extend(vad.process(&vec![0i16; 256]));
        }

        // Should have emitted at least one Silence event (500ms > 200ms interval)
        let silence_events: Vec<_> = all_events
            .iter()
            .filter_map(|e| match e {
                VadEvent::Silence { duration_ms } => Some(*duration_ms),
                _ => None,
            })
            .collect();
        assert!(
            !silence_events.is_empty(),
            "expected Silence event from 500ms of audio-time silence (no wall-clock sleep)"
        );

        // The duration should reflect audio time. Since all data was fed instantly,
        // wall-clock-based tracking would report ~0ms. Sample-based tracking
        // should report >= 200ms.
        let max_duration = silence_events.iter().copied().max().unwrap();
        assert!(
            max_duration >= 200,
            "silence duration should be >= 200ms based on audio samples, got {max_duration}ms"
        );
    }
}
