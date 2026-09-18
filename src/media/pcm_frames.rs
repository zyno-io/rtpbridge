//! Bounded 20 ms reframing. Short packets accumulate; long packets yield every
//! complete frame. Discontinuities are explicit resets, never implicit padding.
use anyhow::Result;
use std::collections::VecDeque;

pub struct PcmFrames {
    samples: VecDeque<i16>,
    frame: usize,
}
impl PcmFrames {
    pub fn new(rate: u32) -> Self {
        Self {
            samples: VecDeque::new(),
            frame: (rate / 50) as usize,
        }
    }
    pub fn clear(&mut self) {
        self.samples.clear();
    }
    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }
    pub fn push(&mut self, pcm: &[i16]) -> Result<Vec<Vec<i16>>> {
        anyhow::ensure!(
            self.frame > 0 && pcm.len() <= self.frame * 6,
            "audio packet exceeds 120 ms"
        );
        // The retained tail is always shorter than one frame.
        self.samples.extend(pcm.iter().copied());
        let mut frames = Vec::with_capacity(self.samples.len() / self.frame);
        while self.samples.len() >= self.frame {
            frames.push(self.samples.drain(..self.frame).collect());
        }
        Ok(frames)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn every_sample_survives_short_and_long_packets() {
        for rate in [8000, 16000, 48000] {
            let mut framer = PcmFrames::new(rate);
            let input: Vec<_> = (0..rate / 5).map(|n| (n % 32768) as i16).collect();
            let mut output = Vec::new();
            for chunk in input.chunks((rate / 400) as usize) {
                for frame in framer.push(chunk).unwrap() {
                    output.extend(frame);
                }
            }
            assert_eq!(output, input);
            assert!(framer.is_empty());
            let long = vec![17; (rate * 120 / 1000) as usize];
            assert_eq!(framer.push(&long).unwrap().concat(), long);
            assert!(framer.push(&vec![0; (rate / 5) as usize]).is_err());
        }
    }
}
