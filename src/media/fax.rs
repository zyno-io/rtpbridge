//! Fax calling/answer tone detection.
//!
//! Detects the two in-band tones that flank the start of a T.30 fax call:
//!
//! - **CNG** (calling tone): 1100 Hz, emitted by the *calling* fax in a
//!   0.5s-on / 3s-off cadence.
//! - **CED** (called terminal identification / answer tone): 2100 Hz,
//!   emitted by the *answering* fax as a continuous ~2.6–4s tone. Note this
//!   is the same frequency as the V.25 modem answer tone, so a CED detection
//!   really means "fax-or-modem answer tone heard".
//!
//! Detection uses the Goertzel algorithm — a cheap single-frequency DFT — over
//! fixed 20ms frames. A tone is considered "present" in a frame when the energy
//! at the target frequency dominates the total frame energy. Detection is
//! edge-triggered: a [`FaxTone`] is emitted once when a tone's onset is
//! confirmed (after [`DETECT_FRAMES`] consecutive present frames, which debounces
//! transient speech energy), and the detector re-arms once the tone has been
//! absent for [`RELEASE_FRAMES`] consecutive frames. This means a later
//! occurrence (e.g. the next CNG burst, or a fresh call) fires again — detection
//! stays armed for the life of the detector.

use std::f64::consts::PI;
use std::time::{Duration, Instant};

/// Frame length in milliseconds. 20ms matches the typical RTP ptime and gives a
/// 50 Hz frequency resolution — both 1100 and 2100 Hz land exactly on a Goertzel
/// bin at every supported sample rate (8/16/48 kHz), so there is no scalloping
/// loss.
const FRAME_MS: usize = 20;

/// Consecutive present frames required to confirm a tone onset (~160ms).
/// Long enough to reject transient speech/music energy at the target frequency,
/// short enough to fit comfortably inside a 0.5s CNG burst.
const DETECT_FRAMES: u32 = 8;

/// Consecutive absent frames required before the detector re-arms (~100ms).
/// Gates re-firing so a single continuous tone reports once, while the
/// CNG on/off cadence (and subsequent calls) still re-trigger.
const RELEASE_FRAMES: u32 = 5;

/// Minimum fraction of total frame energy that must sit at the target frequency
/// for the tone to be considered present. A pure on-bin tone yields ~0.5
/// (energy splits between the +f and -f bins); speech/music spread across the
/// spectrum yields far less. 0.20 cleanly separates the two.
const FRACTION_THRESHOLD: f64 = 0.20;

/// Minimum mean-square sample energy for a frame to be analysed at all. Rejects
/// near-silence (where rounding noise could otherwise spike a bin) and avoids a
/// divide-by-(near-)zero in the fraction test. ~RMS 7 on the i16 scale.
const MIN_MEAN_SQUARE: f64 = 50.0;

/// Number of Goertzel probe frequencies per tone. The detector evaluates each
/// probe and takes the strongest, so a tone anywhere within its allowed
/// frequency tolerance is caught — a single exact-frequency Goertzel would miss
/// compliant-but-off-nominal tones, since at 20ms frames the Dirichlet response
/// nulls out ~50 Hz off target.
const N_PROBES: usize = 3;

/// CNG probe frequencies: nominal 1100 Hz spanning the T.30 ±38 Hz tolerance.
/// Spacing keeps the worst-case offset to the nearest probe well inside the
/// ~50 Hz main lobe.
const CNG_FREQS: [f64; N_PROBES] = [1064.0, 1100.0, 1136.0];
/// CED probe frequencies: nominal 2100 Hz spanning the ±15 Hz answer-tone
/// tolerance.
const CED_FREQS: [f64; N_PROBES] = [2086.0, 2100.0, 2114.0];

/// If no PCM is fed for this long, treat the current tone as ended and re-arm.
/// Covers RTP gaps (silence suppression / DTX) where the off period between
/// tones delivers no packets at all, so the frame-based release counter never
/// advances. Well beyond packet jitter, well under the 3s CNG off cadence.
const REARM_GAP: Duration = Duration::from_millis(500);

/// A detected fax tone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FaxTone {
    /// CNG (1100 Hz) — calling fax.
    Cng,
    /// CED (2100 Hz) — answering fax / modem answer tone.
    Ced,
}

/// Per-tone onset tracking state (the present/absent debounce + re-arm state
/// machine). Frequency probing lives in [`FaxDetector`].
struct ToneState {
    /// Consecutive present frames (resets on an absent frame).
    present_frames: u32,
    /// Consecutive absent frames (resets on a present frame).
    absent_frames: u32,
    /// Whether the detector is ready to fire on the next confirmed onset.
    armed: bool,
}

impl ToneState {
    fn new() -> Self {
        Self {
            present_frames: 0,
            // Start already past the release threshold so the very first
            // confirmed tone fires immediately.
            absent_frames: RELEASE_FRAMES,
            armed: true,
        }
    }

    /// Feed one frame's present/absent decision. Returns true if a tone onset
    /// fired on this frame.
    fn update(&mut self, present: bool) -> bool {
        if present {
            self.absent_frames = 0;
            self.present_frames = self.present_frames.saturating_add(1);
            if self.armed && self.present_frames >= DETECT_FRAMES {
                self.armed = false;
                return true;
            }
        } else {
            self.present_frames = 0;
            self.absent_frames = self.absent_frames.saturating_add(1);
            if self.absent_frames >= RELEASE_FRAMES {
                self.armed = true;
            }
        }
        false
    }

    /// Re-arm after the tone is considered ended (frame counters reset).
    fn rearm(&mut self) {
        self.present_frames = 0;
        self.absent_frames = RELEASE_FRAMES;
        self.armed = true;
    }
}

/// Fax tone detector for a single endpoint. Feed it decoded PCM at a fixed
/// sample rate; it accumulates whole frames and reports tone onsets.
pub struct FaxDetector {
    sample_rate: u32,
    frame_size: usize,
    buffer: Vec<i16>,
    /// Goertzel coefficients (`2·cos(2π·f/fs)`) for each CNG / CED probe frequency.
    cng_coeffs: [f64; N_PROBES],
    ced_coeffs: [f64; N_PROBES],
    cng: ToneState,
    ced: ToneState,
    /// When PCM was last fed, for gap-based re-arming via [`FaxDetector::check_timeout`].
    last_process_time: Option<Instant>,
}

impl FaxDetector {
    /// Create a detector for PCM at `source_sample_rate` Hz (8000/16000/48000).
    pub fn new(source_sample_rate: u32) -> Self {
        let sr = if source_sample_rate == 0 {
            tracing::warn!("FaxDetector created with sample_rate 0, defaulting to 8000");
            8000
        } else {
            source_sample_rate
        };
        let frame_size = (sr as usize * FRAME_MS / 1000).max(64);
        let coeff = |f: f64| 2.0 * (2.0 * PI * f / sr as f64).cos();
        Self {
            sample_rate: sr,
            frame_size,
            buffer: Vec::with_capacity(frame_size * 2),
            cng_coeffs: CNG_FREQS.map(coeff),
            ced_coeffs: CED_FREQS.map(coeff),
            cng: ToneState::new(),
            ced: ToneState::new(),
            last_process_time: None,
        }
    }

    /// Feed decoded PCM samples (at the detector's sample rate). Returns any
    /// tone onsets detected across the whole-frames now available.
    pub fn process(&mut self, pcm: &[i16]) -> Vec<FaxTone> {
        // Self-detect an input gap: if audio paused longer than REARM_GAP since
        // the previous call, the prior tone has ended — re-arm before analysing
        // the resumed stream. Doing this here (not only in check_timeout) means
        // re-arming does not depend on the session loop calling check_timeout
        // during the gap; a post-gap packet arriving first still re-arms.
        let now = Instant::now();
        if let Some(last) = self.last_process_time
            && now.saturating_duration_since(last) >= REARM_GAP
        {
            self.rearm();
        }
        self.last_process_time = Some(now);
        self.buffer.extend_from_slice(pcm);

        let mut events = Vec::new();
        while self.buffer.len() >= self.frame_size {
            let (energy, cng_power, ced_power) = analyze(
                &self.buffer[..self.frame_size],
                &self.cng_coeffs,
                &self.ced_coeffs,
            );
            self.buffer.drain(..self.frame_size);

            let n = self.frame_size as f64;
            let mean_square = energy / n;
            let loud_enough = mean_square >= MIN_MEAN_SQUARE;
            // Fraction of total energy in the target bin (Parseval normalization:
            // sum over bins of |X[k]|^2 == N * sum of x[n]^2).
            let denom = energy * n;
            let cng_present = loud_enough && cng_power / denom >= FRACTION_THRESHOLD;
            let ced_present = loud_enough && ced_power / denom >= FRACTION_THRESHOLD;

            if self.cng.update(cng_present) {
                events.push(FaxTone::Cng);
            }
            if self.ced.update(ced_present) {
                events.push(FaxTone::Ced);
            }
        }
        events
    }

    /// Re-arm the detector if no PCM has been fed for [`REARM_GAP`]. Call
    /// periodically from the session loop (like VAD's `check_timeout`) so a tone
    /// followed by an RTP gap — e.g. silence suppression during the CNG off
    /// period, where no frames reach [`FaxDetector::process`] — re-arms even
    /// while the stream stays paused. (`process` also self-detects the gap when
    /// audio resumes; this is the proactive counterpart.) A no-op while audio is
    /// flowing.
    pub fn check_timeout(&mut self) {
        if let Some(last) = self.last_process_time
            && last.elapsed() >= REARM_GAP
        {
            self.rearm();
            // Clear the marker so a long gap re-arms once, not every tick.
            self.last_process_time = None;
        }
    }

    /// Sample rate the detector is currently configured for (Hz).
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Drop buffered samples and re-arm both tones (treat the current tone as
    /// ended). Does not touch `last_process_time`.
    fn rearm(&mut self) {
        self.buffer.clear();
        self.cng.rearm();
        self.ced.rearm();
    }

    /// Reset all detection state (drops buffered samples and re-arms).
    #[allow(dead_code)] // used in tests
    pub fn reset(&mut self) {
        self.rearm();
        self.last_process_time = None;
    }
}

/// Single-pass Goertzel over one frame for every CNG and CED probe frequency,
/// returning `(total_energy, max_cng_power, max_ced_power)`. Each `*_power` is
/// the strongest squared magnitude |X[k]|² across that tone's probe band (so a
/// tone anywhere in its tolerance registers), and `total_energy` is Σx[n]².
fn analyze(
    frame: &[i16],
    cng_coeffs: &[f64; N_PROBES],
    ced_coeffs: &[f64; N_PROBES],
) -> (f64, f64, f64) {
    let mut energy = 0.0f64;
    // Goertzel recurrence state (s[n-1], s[n-2]) for each probe.
    let mut cs1 = [0.0f64; N_PROBES];
    let mut cs2 = [0.0f64; N_PROBES];
    let mut ds1 = [0.0f64; N_PROBES];
    let mut ds2 = [0.0f64; N_PROBES];
    for &s in frame {
        let x = s as f64;
        energy += x * x;
        for k in 0..N_PROBES {
            let c0 = x + cng_coeffs[k] * cs1[k] - cs2[k];
            cs2[k] = cs1[k];
            cs1[k] = c0;
            let d0 = x + ced_coeffs[k] * ds1[k] - ds2[k];
            ds2[k] = ds1[k];
            ds1[k] = d0;
        }
    }
    let mut cng_power = 0.0f64;
    let mut ced_power = 0.0f64;
    for k in 0..N_PROBES {
        let cp = cs1[k] * cs1[k] + cs2[k] * cs2[k] - cng_coeffs[k] * cs1[k] * cs2[k];
        cng_power = cng_power.max(cp);
        let dp = ds1[k] * ds1[k] + ds2[k] * ds2[k] - ced_coeffs[k] * ds1[k] * ds2[k];
        ced_power = ced_power.max(dp);
    }
    (energy, cng_power, ced_power)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Nominal tone frequencies, for tests that exercise on-target detection.
    const CNG_FREQ: f64 = 1100.0;
    const CED_FREQ: f64 = 2100.0;

    /// Generate a sine wave at the given frequency and sample rate.
    fn sine_wave(freq_hz: f64, sample_rate: u32, num_samples: usize, amplitude: f64) -> Vec<i16> {
        (0..num_samples)
            .map(|i| {
                let t = i as f64 / sample_rate as f64;
                (f64::sin(2.0 * PI * freq_hz * t) * amplitude) as i16
            })
            .collect()
    }

    #[test]
    fn detects_ced_2100hz() {
        let mut det = FaxDetector::new(8000);
        // 500ms of 2100 Hz — well past the ~160ms confirmation window.
        let tone = sine_wave(CED_FREQ, 8000, 4000, 10000.0);
        let events = det.process(&tone);
        assert!(
            events.contains(&FaxTone::Ced),
            "expected CED detection from 2100Hz tone, got {events:?}"
        );
        assert!(
            !events.contains(&FaxTone::Cng),
            "2100Hz tone must not trigger CNG, got {events:?}"
        );
    }

    #[test]
    fn detects_cng_1100hz() {
        let mut det = FaxDetector::new(8000);
        let tone = sine_wave(CNG_FREQ, 8000, 4000, 10000.0);
        let events = det.process(&tone);
        assert!(
            events.contains(&FaxTone::Cng),
            "expected CNG detection from 1100Hz tone, got {events:?}"
        );
        assert!(
            !events.contains(&FaxTone::Ced),
            "1100Hz tone must not trigger CED, got {events:?}"
        );
    }

    #[test]
    fn silence_produces_no_events() {
        let mut det = FaxDetector::new(8000);
        let events = det.process(&vec![0i16; 8000]);
        assert!(
            events.is_empty(),
            "silence should not detect tones: {events:?}"
        );
    }

    #[test]
    fn detects_cng_off_nominal_within_tolerance() {
        // T.30 allows CNG at 1100 ±38 Hz. A compliant transmitter at the edge of
        // the band must still be detected (a single exact-1100Hz Goertzel would
        // miss it).
        for freq in [1063.0, 1138.0] {
            let mut det = FaxDetector::new(8000);
            let tone = sine_wave(freq, 8000, 4000, 10000.0); // 500ms
            assert!(
                det.process(&tone).contains(&FaxTone::Cng),
                "CNG at {freq}Hz (within ±38Hz tolerance) should be detected"
            );
        }
    }

    #[test]
    fn detects_ced_off_nominal_within_tolerance() {
        for freq in [2087.0, 2113.0] {
            let mut det = FaxDetector::new(8000);
            let tone = sine_wave(freq, 8000, 4000, 10000.0);
            assert!(
                det.process(&tone).contains(&FaxTone::Ced),
                "CED at {freq}Hz (within tolerance) should be detected"
            );
        }
    }

    #[test]
    fn tone_outside_tolerance_not_detected() {
        // 1209 Hz is a DTMF column frequency, ~70 Hz past the CNG band — it must
        // not be mistaken for CNG (or CED).
        let mut det = FaxDetector::new(8000);
        let tone = sine_wave(1209.0, 8000, 8000, 10000.0); // 1s
        let events = det.process(&tone);
        assert!(
            events.is_empty(),
            "1209Hz (outside the CNG tolerance band) must not fire: {events:?}"
        );
    }

    #[test]
    fn non_fax_tone_produces_no_events() {
        // 440 Hz (a speech-band tone, not a fax frequency) must not trigger.
        let mut det = FaxDetector::new(8000);
        let tone = sine_wave(440.0, 8000, 8000, 10000.0);
        let events = det.process(&tone);
        assert!(
            events.is_empty(),
            "440Hz tone should not be mistaken for a fax tone: {events:?}"
        );
    }

    #[test]
    fn brief_tone_below_debounce_does_not_fire() {
        // 100ms of CED is shorter than the ~160ms confirmation window.
        let mut det = FaxDetector::new(8000);
        let tone = sine_wave(CED_FREQ, 8000, 800, 10000.0);
        let events = det.process(&tone);
        assert!(
            events.is_empty(),
            "a tone shorter than the debounce window should not fire: {events:?}"
        );
    }

    #[test]
    fn continuous_tone_fires_once() {
        // A single uninterrupted CED tone should produce exactly one event.
        let mut det = FaxDetector::new(8000);
        let tone = sine_wave(CED_FREQ, 8000, 8000, 10000.0); // 1s
        let ced_count = det
            .process(&tone)
            .iter()
            .filter(|t| **t == FaxTone::Ced)
            .count();
        assert_eq!(ced_count, 1, "a continuous tone should fire exactly once");
    }

    #[test]
    fn rearms_after_gap() {
        // CED tone, then silence past the release window, then CED again ->
        // two distinct detections (models "continuous" detection across calls).
        let mut det = FaxDetector::new(8000);
        let tone = sine_wave(CED_FREQ, 8000, 4000, 10000.0); // 500ms
        let silence = vec![0i16; 2400]; // 300ms — past RELEASE_FRAMES (~100ms)

        let first = det.process(&tone);
        assert!(first.contains(&FaxTone::Ced), "first burst should fire");
        let gap = det.process(&silence);
        assert!(gap.is_empty(), "silence gap should not fire");
        let second = det.process(&tone);
        assert!(
            second.contains(&FaxTone::Ced),
            "second burst after a gap should re-fire"
        );
    }

    #[test]
    fn rearms_after_input_gap_via_check_timeout() {
        // Models an RTP gap (silence suppression) where NO frames reach the
        // detector during the off period — the frame-based release counter can't
        // advance, so re-arming must come from the time-based check_timeout.
        let mut det = FaxDetector::new(8000);
        let tone = || sine_wave(CED_FREQ, 8000, 4000, 10000.0); // 500ms

        assert!(
            det.process(&tone()).contains(&FaxTone::Ced),
            "first burst fires"
        );

        // No gap yet: check_timeout must not re-arm, and a fresh burst (detector
        // still disarmed) must not fire.
        det.check_timeout();
        assert!(
            !det.process(&tone()).contains(&FaxTone::Ced),
            "must stay disarmed before the gap threshold elapses"
        );

        // Real input gap (no process() calls), then a periodic check re-arms.
        std::thread::sleep(REARM_GAP + Duration::from_millis(100));
        det.check_timeout();
        assert!(
            det.process(&tone()).contains(&FaxTone::Ced),
            "a burst after a gap re-arm should fire again"
        );
    }

    #[test]
    fn process_self_rearms_after_gap_without_check_timeout() {
        // The post-gap packet can arrive before the session loop calls
        // check_timeout(); process() must observe the gap itself and re-arm, or
        // the next burst is missed entirely.
        let mut det = FaxDetector::new(8000);
        let tone = || sine_wave(CED_FREQ, 8000, 4000, 10000.0);

        assert!(
            det.process(&tone()).contains(&FaxTone::Ced),
            "first burst fires"
        );

        std::thread::sleep(REARM_GAP + Duration::from_millis(100));
        // No check_timeout() — process() alone must re-arm from the elapsed gap.
        assert!(
            det.process(&tone()).contains(&FaxTone::Ced),
            "process() must self-re-arm after an input gap"
        );
    }

    #[test]
    fn cng_cadence_fires_per_burst() {
        // CNG is 0.5s on / 3s off. Two on-bursts separated by an off-gap should
        // each produce a detection.
        let mut det = FaxDetector::new(8000);
        let burst = sine_wave(CNG_FREQ, 8000, 4000, 10000.0); // 500ms on
        let gap = vec![0i16; 4000]; // 500ms off (shortened from 3s for the test)

        let mut total = 0;
        total += det
            .process(&burst)
            .iter()
            .filter(|t| **t == FaxTone::Cng)
            .count();
        det.process(&gap);
        total += det
            .process(&burst)
            .iter()
            .filter(|t| **t == FaxTone::Cng)
            .count();
        assert_eq!(total, 2, "each CNG burst should fire once");
    }

    #[test]
    fn detects_ced_at_16khz() {
        let mut det = FaxDetector::new(16000);
        let tone = sine_wave(CED_FREQ, 16000, 8000, 10000.0); // 500ms
        assert!(det.process(&tone).contains(&FaxTone::Ced), "CED at 16kHz");
    }

    #[test]
    fn detects_ced_at_48khz() {
        let mut det = FaxDetector::new(48000);
        let tone = sine_wave(CED_FREQ, 48000, 24000, 10000.0); // 500ms
        assert!(det.process(&tone).contains(&FaxTone::Ced), "CED at 48kHz");
    }

    #[test]
    fn buffers_across_calls() {
        // Feeding sub-frame chunks should still accumulate to whole frames and
        // detect the tone (no events lost at chunk boundaries).
        let mut det = FaxDetector::new(8000);
        let tone = sine_wave(CED_FREQ, 8000, 4000, 10000.0);
        let mut fired = false;
        for chunk in tone.chunks(37) {
            // deliberately not a frame multiple
            if det.process(chunk).contains(&FaxTone::Ced) {
                fired = true;
            }
        }
        assert!(fired, "tone fed in odd chunks should still be detected");
    }

    #[test]
    fn reset_clears_state() {
        let mut det = FaxDetector::new(8000);
        let tone = sine_wave(CED_FREQ, 8000, 4000, 10000.0);
        assert!(det.process(&tone).contains(&FaxTone::Ced));
        det.reset();
        // After reset, a brief sub-debounce tone should not fire (state cleared).
        let brief = sine_wave(CED_FREQ, 8000, 400, 10000.0); // 50ms
        assert!(
            det.process(&brief).is_empty(),
            "reset should clear accumulated present-frame count"
        );
    }

    #[test]
    fn empty_input_no_panic() {
        let mut det = FaxDetector::new(8000);
        assert!(det.process(&[]).is_empty());
    }

    #[test]
    fn sub_frame_input_no_events() {
        let mut det = FaxDetector::new(8000);
        let tone = sine_wave(CED_FREQ, 8000, 100, 10000.0); // < one 160-sample frame
        assert!(det.process(&tone).is_empty());
    }
}
