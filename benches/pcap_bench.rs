use std::time::SystemTime;

use criterion::{Criterion, criterion_group, criterion_main};
use rtpbridge::recording::pcap_writer::{
    RecordPacket, build_pcap_frame, create_pcap_writer, write_record_packet,
};

fn bench_pcap_write_single(c: &mut Criterion) {
    let src: std::net::SocketAddr = "10.0.0.1:5000".parse().unwrap();
    let dst: std::net::SocketAddr = "10.0.0.2:6000".parse().unwrap();
    let payload = vec![0x80; 172]; // typical RTP packet size

    c.bench_function("pcap_write_single_172B", |b| {
        b.iter_batched(
            || {
                let buf = Vec::with_capacity(1024 * 1024);
                let writer = create_pcap_writer(buf).unwrap();
                let pkt = RecordPacket {
                    src_addr: src,
                    dst_addr: dst,
                    payload: payload.clone(),
                    timestamp: SystemTime::now(),
                };
                (writer, pkt)
            },
            |(mut writer, pkt)| {
                write_record_packet(&mut writer, &pkt).unwrap();
            },
            criterion::BatchSize::SmallInput,
        )
    });
}

fn bench_pcap_write_burst(c: &mut Criterion) {
    let src: std::net::SocketAddr = "10.0.0.1:5000".parse().unwrap();
    let dst: std::net::SocketAddr = "10.0.0.2:6000".parse().unwrap();
    let payload = vec![0x80; 172];

    c.bench_function("pcap_write_burst_100pkt", |b| {
        b.iter(|| {
            let buf = Vec::with_capacity(64 * 1024);
            let mut writer = create_pcap_writer(buf).unwrap();
            for _ in 0u8..100 {
                let pkt = RecordPacket {
                    src_addr: src,
                    dst_addr: dst,
                    payload: payload.clone(),
                    timestamp: SystemTime::now(),
                };
                write_record_packet(&mut writer, &pkt).unwrap();
            }
        })
    });
}

fn bench_build_pcap_frame(c: &mut Criterion) {
    let src: std::net::SocketAddr = "10.0.0.1:5000".parse().unwrap();
    let dst: std::net::SocketAddr = "10.0.0.2:6000".parse().unwrap();
    let payload = vec![0x80; 172];

    c.bench_function("build_pcap_frame_172B", |b| {
        b.iter(|| {
            build_pcap_frame(src, dst, &payload);
        })
    });
}

criterion_group!(
    benches,
    bench_pcap_write_single,
    bench_pcap_write_burst,
    bench_build_pcap_frame,
);
criterion_main!(benches);
