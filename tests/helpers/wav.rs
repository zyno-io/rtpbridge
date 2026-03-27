#![allow(dead_code)]

use std::io::Write;
use std::path::Path;

/// Generate a minimal 8 kHz mono 16-bit PCM WAV file with a sine tone.
pub fn generate_test_wav(path: impl AsRef<Path>, duration_secs: f64, frequency_hz: f64) {
    let sample_rate: u32 = 8000;
    let num_samples = (sample_rate as f64 * duration_secs) as usize;
    let bits_per_sample: u16 = 16;
    let num_channels: u16 = 1;
    let byte_rate = sample_rate * (bits_per_sample as u32 / 8) * num_channels as u32;
    let block_align = num_channels * (bits_per_sample / 8);
    let data_size = (num_samples * 2) as u32;

    let mut file = std::fs::File::create(path.as_ref()).unwrap();
    file.write_all(b"RIFF").unwrap();
    file.write_all(&(36 + data_size).to_le_bytes()).unwrap();
    file.write_all(b"WAVE").unwrap();
    file.write_all(b"fmt ").unwrap();
    file.write_all(&16u32.to_le_bytes()).unwrap();
    file.write_all(&1u16.to_le_bytes()).unwrap(); // PCM
    file.write_all(&num_channels.to_le_bytes()).unwrap();
    file.write_all(&sample_rate.to_le_bytes()).unwrap();
    file.write_all(&byte_rate.to_le_bytes()).unwrap();
    file.write_all(&block_align.to_le_bytes()).unwrap();
    file.write_all(&bits_per_sample.to_le_bytes()).unwrap();
    file.write_all(b"data").unwrap();
    file.write_all(&data_size.to_le_bytes()).unwrap();

    for i in 0..num_samples {
        let t = i as f64 / sample_rate as f64;
        let sample = (f64::sin(2.0 * std::f64::consts::PI * frequency_hz * t) * 16000.0) as i16;
        file.write_all(&sample.to_le_bytes()).unwrap();
    }
}
