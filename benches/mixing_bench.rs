use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use rtpbridge::{
    control::protocol::EndpointId,
    media::codec::{self, AudioCodec},
    session::{endpoint::RoutedRtpPacket, mixer::DestinationMixer, source_audio::SourceAudio},
};

fn mixing(c: &mut Criterion) {
    let mut group = c.benchmark_group("conference");
    group.sample_size(10);
    group.warm_up_time(std::time::Duration::from_secs(1));
    group.measurement_time(std::time::Duration::from_secs(2));
    for family in ["Pcmu", "G722", "Opus", "Mixed"] {
        for count in [2usize, 3, 10, 20] {
            let codecs: Vec<_> = (0..count)
                .map(|i| match family {
                    "Pcmu" => AudioCodec::Pcmu,
                    "G722" => AudioCodec::G722,
                    "Opus" => AudioCodec::Opus,
                    _ => [AudioCodec::Pcmu, AudioCodec::G722, AudioCodec::Opus][i % 3],
                })
                .collect();
            let ids: Vec<_> = (0..count).map(|_| EndpointId::new_v4()).collect();
            let payloads: Vec<_> = codecs
                .iter()
                .map(|&codec| {
                    let pcm: Vec<_> = (0..codec.ptime_samples())
                        .map(|i| ((i as f64 * 0.05).sin() * 5000.0) as i16)
                        .collect();
                    let mut encoder = codec::make_encoder(codec).unwrap();
                    let mut payload = Vec::new();
                    encoder.encode(&pcm, &mut payload).unwrap();
                    payload
                })
                .collect();
            let mut rates: Vec<_> = codecs.iter().map(|codec| codec.sample_rate()).collect();
            rates.sort_unstable();
            rates.dedup();
            // Same executable, codec implementations and workload. This retains
            // the former per-destination decoding path as a comparison control.
            for shared in [false, true] {
                let mut mixers: Vec<_> = codecs
                    .iter()
                    .map(|&codec| DestinationMixer::new(codec, 0).unwrap())
                    .collect();
                let mut decoders: Vec<_> = codecs
                    .iter()
                    .map(|&codec| SourceAudio::new(codec).unwrap())
                    .collect();
                let mut frame = 0u32;
                let mode = if shared { "shared" } else { "per_destination" };
                group.bench_with_input(
                    BenchmarkId::new(format!("{family}_{mode}"), count),
                    &count,
                    |b, &count| {
                        b.iter(|| {
                            for source in 0..count {
                                let codec = codecs[source];
                                let payload = &payloads[source];
                                if shared {
                                    let packet = RoutedRtpPacket {
                                        source_endpoint_id: ids[source],
                                        payload_type: 0,
                                        sequence_number: frame as u16,
                                        timestamp: frame.wrapping_mul(codec.rtp_clock_rate() / 50),
                                        ssrc: source as u32,
                                        marker: frame == 0,
                                        payload: payload.clone(),
                                    };
                                    let pcm = decoders[source].decode(&packet).unwrap();
                                    let frames = decoders[source].frames(&pcm, &rates).unwrap();
                                    for (destination, mixer) in mixers.iter_mut().enumerate() {
                                        if destination != source {
                                            mixer
                                                .feed_pcm(
                                                    ids[source],
                                                    frames[0].rates
                                                        [&codecs[destination].sample_rate()]
                                                        .clone(),
                                                )
                                                .unwrap();
                                        }
                                    }
                                } else {
                                    for (destination, mixer) in mixers.iter_mut().enumerate() {
                                        if destination != source {
                                            mixer.feed(ids[source], codec, payload).unwrap();
                                        }
                                    }
                                }
                            }
                            for mixer in &mut mixers {
                                mixer.flush_tick().unwrap();
                                for packet in mixer.drain() {
                                    std::hint::black_box(packet);
                                }
                            }
                            frame = frame.wrapping_add(1);
                        })
                    },
                );
            }
        }
    }
    group.finish();
}
criterion_group!(benches, mixing);
criterion_main!(benches);
