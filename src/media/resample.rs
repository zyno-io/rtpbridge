/// Linear interpolation resampler for converting between telephony sample rates.
/// Supports 8000 Hz (PCMU), 16000 Hz (G.722), and 48000 Hz (Opus).
///
/// Note: Uses simple linear interpolation without an anti-aliasing filter.
/// This is intentional for low-latency telephony use where sample rate ratios
/// are small integer multiples. For higher-quality resampling (e.g., music),
/// consider a polyphase FIR filter.
pub struct Resampler {
    from_rate: u32,
    to_rate: u32,
    /// Fractional sample position accumulator for precise resampling
    frac_pos: f64,
}

impl Resampler {
    pub fn new(from_rate: u32, to_rate: u32) -> Self {
        assert!(from_rate > 0, "from_rate must be > 0");
        assert!(to_rate > 0, "to_rate must be > 0");
        Self {
            from_rate,
            to_rate,
            frac_pos: 0.0,
        }
    }

    /// Returns true if no resampling is needed (same rate)
    pub fn is_passthrough(&self) -> bool {
        self.from_rate == self.to_rate
    }

    /// Resample input samples to output rate using linear interpolation.
    /// Writes resampled samples into `output`, reusing the allocation.
    pub fn process(&mut self, input: &[i16], output: &mut Vec<i16>) {
        output.clear();

        if self.is_passthrough() {
            output.extend_from_slice(input);
            return;
        }

        if input.is_empty() {
            return;
        }

        let ratio = self.from_rate as f64 / self.to_rate as f64;
        let out_len = ((input.len() as f64) / ratio).ceil() as usize;
        output.reserve(out_len);

        let mut pos = self.frac_pos;
        let last_valid = (input.len() - 1) as f64;
        while pos <= last_valid {
            let idx = pos as usize;
            let frac = pos - idx as f64;

            let s0 = input[idx] as f64;
            let s1 = if idx + 1 < input.len() {
                input[idx + 1] as f64
            } else {
                s0
            };
            output.push((s0 + frac * (s1 - s0)) as i16);

            pos += ratio;
        }

        // Save fractional position relative to the end of this buffer
        // so the next call continues from the correct sub-sample offset.
        self.frac_pos = pos - input.len() as f64;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: call process and return the output vec for test convenience.
    fn run(r: &mut Resampler, input: &[i16]) -> Vec<i16> {
        let mut out = Vec::new();
        r.process(input, &mut out);
        out
    }

    #[test]
    fn test_passthrough() {
        let mut r = Resampler::new(8000, 8000);
        assert!(r.is_passthrough());
        let input = vec![1, 2, 3, 4, 5];
        assert_eq!(run(&mut r, &input), input);
    }

    #[test]
    fn test_upsample_8k_to_16k() {
        let mut r = Resampler::new(8000, 16000);
        let input: Vec<i16> = (0..80).map(|i| (i * 100) as i16).collect();
        let output = run(&mut r, &input);
        assert!(
            output.len() >= 158 && output.len() <= 162,
            "expected ~160 samples, got {}",
            output.len()
        );
    }

    #[test]
    fn test_downsample_48k_to_8k() {
        let mut r = Resampler::new(48000, 8000);
        let input: Vec<i16> = (0..960)
            .map(|i| ((i as f64 * 0.1).sin() * 10000.0) as i16)
            .collect();
        let output = run(&mut r, &input);
        assert!(
            output.len() >= 158 && output.len() <= 162,
            "expected ~160 samples, got {}",
            output.len()
        );
    }

    #[test]
    fn test_upsample_8k_to_48k() {
        let mut r = Resampler::new(8000, 48000);
        let input: Vec<i16> = (0..160)
            .map(|i| ((i as f64 * 0.05).sin() * 10000.0) as i16)
            .collect();
        let output = run(&mut r, &input);
        assert!(
            output.len() >= 950 && output.len() <= 970,
            "expected ~960 samples, got {}",
            output.len()
        );
    }

    #[test]
    fn test_empty_input() {
        let mut r = Resampler::new(8000, 16000);
        assert!(run(&mut r, &[]).is_empty());
    }

    #[test]
    fn test_continuity_across_calls() {
        let mut r1 = Resampler::new(8000, 48000);
        let mut r2 = Resampler::new(8000, 48000);

        let input: Vec<i16> = (0..320)
            .map(|i| ((i as f64 * 0.05).sin() * 10000.0) as i16)
            .collect();

        let out_big = run(&mut r1, &input);
        let out_a = run(&mut r2, &input[..160]);
        let out_b = run(&mut r2, &input[160..]);

        let total_small = out_a.len() + out_b.len();
        assert!(
            (total_small as i64 - out_big.len() as i64).unsigned_abs() <= 2,
            "continuity: big={}, small_total={}",
            out_big.len(),
            total_small
        );

        let combined: Vec<i16> = out_a.iter().chain(out_b.iter()).copied().collect();
        let check_len = combined.len().min(out_big.len());
        for i in 0..check_len {
            assert!(
                (combined[i] as i32 - out_big[i] as i32).abs() <= 50,
                "sample mismatch at index {}: combined={}, big={}",
                i,
                combined[i],
                out_big[i]
            );
        }
    }

    #[test]
    fn test_resample_8k_to_16k() {
        let mut r = Resampler::new(8000, 16000);
        let input: Vec<i16> = (0..160)
            .map(|i| {
                ((i as f64 * 2.0 * std::f64::consts::PI * 440.0 / 8000.0).sin() * 10000.0) as i16
            })
            .collect();
        let output = run(&mut r, &input);
        assert!(
            output.len() >= 318 && output.len() <= 322,
            "expected ~320 samples, got {}",
            output.len()
        );
    }

    #[test]
    fn test_resample_48k_to_8k() {
        let mut r = Resampler::new(48000, 8000);
        let input: Vec<i16> = (0..960)
            .map(|i| {
                ((i as f64 * 2.0 * std::f64::consts::PI * 440.0 / 48000.0).sin() * 10000.0) as i16
            })
            .collect();
        let output = run(&mut r, &input);
        assert!(
            output.len() >= 158 && output.len() <= 162,
            "expected ~160 samples, got {}",
            output.len()
        );
    }

    #[test]
    fn test_resample_same_rate() {
        let mut r = Resampler::new(8000, 8000);
        let input: Vec<i16> = (0..160).map(|i| (i * 100) as i16).collect();
        let output = run(&mut r, &input);
        assert_eq!(output.len(), 160, "same-rate should preserve sample count");
        assert_eq!(output, input, "same-rate should return identical samples");
    }

    #[test]
    fn test_resample_empty_input() {
        let mut r = Resampler::new(8000, 16000);
        let output = run(&mut r, &[]);
        assert!(output.is_empty(), "empty input should produce empty output");
    }

    #[test]
    fn test_resample_single_sample() {
        let mut r = Resampler::new(8000, 16000);
        let output = run(&mut r, &[1000]);
        assert_eq!(
            output.len(),
            1,
            "single sample upsample should produce exactly 1 sample, got {}",
            output.len()
        );
        assert_eq!(output[0], 1000, "single sample value should be preserved");
    }

    #[test]
    fn test_last_sample_included() {
        let mut r = Resampler::new(8000, 8000);
        let input: Vec<i16> = vec![0, 1000, 2000, 3000, 4000];
        let output = run(&mut r, &input);
        assert_eq!(
            output, input,
            "same-rate should preserve all samples including last"
        );
    }

    #[test]
    #[should_panic(expected = "from_rate must be > 0")]
    fn test_zero_from_rate_panics() {
        Resampler::new(0, 8000);
    }

    #[test]
    #[should_panic(expected = "to_rate must be > 0")]
    fn test_zero_to_rate_panics() {
        Resampler::new(8000, 0);
    }
}
