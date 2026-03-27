use std::f64::consts::PI;

use criterion::{Criterion, criterion_group, criterion_main};
use rtpbridge::media::codec::{
    AudioDecoder, AudioEncoder, G722Decoder, G722Encoder, OpusDecoder, OpusEncoder, PcmuDecoder,
    PcmuEncoder,
};

/// Generate a sine-wave PCM buffer at the given sample rate.
fn sine_pcm(num_samples: usize, sample_rate: f64) -> Vec<i16> {
    (0..num_samples)
        .map(|i| (f64::sin(i as f64 * 2.0 * PI * 440.0 / sample_rate) * 16000.0) as i16)
        .collect()
}

fn bench_pcmu_encode(c: &mut Criterion) {
    let pcm = sine_pcm(160, 8000.0); // 20ms @ 8kHz
    let mut encoder = PcmuEncoder::new();
    let mut out = Vec::new();
    c.bench_function("pcmu_encode_20ms", |b| {
        b.iter(|| {
            encoder.encode(&pcm, &mut out).unwrap();
        })
    });
}

fn bench_pcmu_decode(c: &mut Criterion) {
    // Encode first to get valid PCMU data
    let pcm = sine_pcm(160, 8000.0);
    let mut encoder = PcmuEncoder::new();
    let mut encoded = Vec::new();
    encoder.encode(&pcm, &mut encoded).unwrap();

    let mut decoder = PcmuDecoder::new();
    let mut out = Vec::new();
    c.bench_function("pcmu_decode_20ms", |b| {
        b.iter(|| {
            decoder.decode(&encoded, &mut out).unwrap();
        })
    });
}

fn bench_g722_encode(c: &mut Criterion) {
    let pcm = sine_pcm(320, 16000.0); // 20ms @ 16kHz
    let mut encoder = G722Encoder::new();
    let mut out = Vec::new();
    c.bench_function("g722_encode_20ms", |b| {
        b.iter(|| {
            encoder.encode(&pcm, &mut out).unwrap();
        })
    });
}

fn bench_g722_decode(c: &mut Criterion) {
    let pcm = sine_pcm(320, 16000.0);
    let mut encoder = G722Encoder::new();
    let mut encoded = Vec::new();
    encoder.encode(&pcm, &mut encoded).unwrap();

    let mut decoder = G722Decoder::new();
    let mut out = Vec::new();
    c.bench_function("g722_decode_20ms", |b| {
        b.iter(|| {
            decoder.decode(&encoded, &mut out).unwrap();
        })
    });
}

fn bench_opus_encode(c: &mut Criterion) {
    let pcm = sine_pcm(960, 48000.0); // 20ms @ 48kHz
    let mut encoder = OpusEncoder::new().unwrap();
    let mut out = Vec::new();
    c.bench_function("opus_encode_20ms", |b| {
        b.iter(|| {
            encoder.encode(&pcm, &mut out).unwrap();
        })
    });
}

fn bench_opus_decode(c: &mut Criterion) {
    let pcm = sine_pcm(960, 48000.0);
    let mut encoder = OpusEncoder::new().unwrap();
    let mut encoded = Vec::new();
    encoder.encode(&pcm, &mut encoded).unwrap();

    let mut decoder = OpusDecoder::new().unwrap();
    let mut out = Vec::new();
    c.bench_function("opus_decode_20ms", |b| {
        b.iter(|| {
            decoder.decode(&encoded, &mut out).unwrap();
        })
    });
}

fn bench_transcode_pcmu_to_g722(c: &mut Criterion) {
    use rtpbridge::media::codec::AudioCodec;
    use rtpbridge::media::transcode::TranscodePipeline;

    // Prepare a PCMU-encoded frame
    let pcm = sine_pcm(160, 8000.0);
    let mut encoder = PcmuEncoder::new();
    let mut pcmu_data = Vec::new();
    encoder.encode(&pcm, &mut pcmu_data).unwrap();

    let mut pipeline = TranscodePipeline::new(AudioCodec::Pcmu, AudioCodec::G722).unwrap();
    c.bench_function("transcode_pcmu_to_g722_20ms", |b| {
        b.iter(|| {
            pipeline.process(&pcmu_data).unwrap();
        })
    });
}

fn bench_transcode_pcmu_to_opus(c: &mut Criterion) {
    use rtpbridge::media::codec::AudioCodec;
    use rtpbridge::media::transcode::TranscodePipeline;

    let pcm = sine_pcm(160, 8000.0);
    let mut encoder = PcmuEncoder::new();
    let mut pcmu_data = Vec::new();
    encoder.encode(&pcm, &mut pcmu_data).unwrap();

    let mut pipeline = TranscodePipeline::new(AudioCodec::Pcmu, AudioCodec::Opus).unwrap();
    c.bench_function("transcode_pcmu_to_opus_20ms", |b| {
        b.iter(|| {
            pipeline.process(&pcmu_data).unwrap();
        })
    });
}

fn bench_transcode_opus_to_pcmu(c: &mut Criterion) {
    use rtpbridge::media::codec::AudioCodec;
    use rtpbridge::media::transcode::TranscodePipeline;

    // 48kHz → 8kHz: decode Opus + 6x downsample + encode PCMU
    let pcm = sine_pcm(960, 48000.0);
    let mut encoder = OpusEncoder::new().unwrap();
    let mut opus_data = Vec::new();
    encoder.encode(&pcm, &mut opus_data).unwrap();

    let mut pipeline = TranscodePipeline::new(AudioCodec::Opus, AudioCodec::Pcmu).unwrap();
    c.bench_function("transcode_opus_to_pcmu_20ms", |b| {
        b.iter(|| {
            pipeline.process(&opus_data).unwrap();
        })
    });
}

fn bench_transcode_opus_to_g722(c: &mut Criterion) {
    use rtpbridge::media::codec::AudioCodec;
    use rtpbridge::media::transcode::TranscodePipeline;

    // 48kHz → 16kHz: decode Opus + 3x downsample + encode G.722
    let pcm = sine_pcm(960, 48000.0);
    let mut encoder = OpusEncoder::new().unwrap();
    let mut opus_data = Vec::new();
    encoder.encode(&pcm, &mut opus_data).unwrap();

    let mut pipeline = TranscodePipeline::new(AudioCodec::Opus, AudioCodec::G722).unwrap();
    c.bench_function("transcode_opus_to_g722_20ms", |b| {
        b.iter(|| {
            pipeline.process(&opus_data).unwrap();
        })
    });
}

fn bench_transcode_g722_to_opus(c: &mut Criterion) {
    use rtpbridge::media::codec::AudioCodec;
    use rtpbridge::media::transcode::TranscodePipeline;

    // 16kHz → 48kHz: decode G.722 + 3x upsample + encode Opus
    let pcm = sine_pcm(320, 16000.0);
    let mut encoder = G722Encoder::new();
    let mut g722_data = Vec::new();
    encoder.encode(&pcm, &mut g722_data).unwrap();

    let mut pipeline = TranscodePipeline::new(AudioCodec::G722, AudioCodec::Opus).unwrap();
    c.bench_function("transcode_g722_to_opus_20ms", |b| {
        b.iter(|| {
            pipeline.process(&g722_data).unwrap();
        })
    });
}

criterion_group!(
    benches,
    bench_pcmu_encode,
    bench_pcmu_decode,
    bench_g722_encode,
    bench_g722_decode,
    bench_opus_encode,
    bench_opus_decode,
    bench_transcode_pcmu_to_g722,
    bench_transcode_pcmu_to_opus,
    bench_transcode_opus_to_pcmu,
    bench_transcode_opus_to_g722,
    bench_transcode_g722_to_opus,
);
criterion_main!(benches);
