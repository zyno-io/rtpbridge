use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use rtpbridge::media::srtp::{SrtcpContext, SrtpContext};

/// Fixed 30-byte key (16-byte master key + 14-byte salt) base64-encoded.
/// 30 bytes of 0x41 ('A') => "QUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFB"
const TEST_KEY_B64: &str = "QUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFB";

/// Build a minimal RTP packet with a 160-byte payload.
fn make_test_rtp_packet(seq: u16) -> Vec<u8> {
    let mut pkt = vec![0u8; 172]; // 12-byte header + 160-byte payload
    pkt[0] = 0x80; // V=2
    pkt[1] = 0; // PT=0 (PCMU)
    pkt[2] = (seq >> 8) as u8;
    pkt[3] = (seq & 0xFF) as u8;
    // timestamp and SSRC left as zero for benchmark simplicity
    pkt
}

fn bench_srtp_protect(c: &mut Criterion) {
    c.bench_function("srtp_protect_172B", |b| {
        b.iter_batched(
            || {
                // Setup: fresh context + packet for each batch
                let ctx = SrtpContext::from_sdes_key(TEST_KEY_B64).unwrap();
                let pkt = make_test_rtp_packet(1);
                (ctx, pkt)
            },
            |(mut ctx, pkt)| {
                let _protected = ctx.protect(&pkt).unwrap();
            },
            BatchSize::SmallInput,
        )
    });
}

fn bench_srtp_unprotect(c: &mut Criterion) {
    c.bench_function("srtp_unprotect_182B", |b| {
        b.iter_batched(
            || {
                // Setup: protect a packet so we have valid SRTP data to unprotect
                let mut protect_ctx = SrtpContext::from_sdes_key(TEST_KEY_B64).unwrap();
                let unprotect_ctx = SrtpContext::from_sdes_key(TEST_KEY_B64).unwrap();
                let pkt = make_test_rtp_packet(1);
                let protected = protect_ctx.protect(&pkt).unwrap();
                (unprotect_ctx, protected)
            },
            |(mut ctx, protected)| {
                let _unprotected = ctx.unprotect(&protected).unwrap();
            },
            BatchSize::SmallInput,
        )
    });
}

fn bench_srtp_key_derivation(c: &mut Criterion) {
    c.bench_function("srtp_key_derivation", |b| {
        b.iter(|| {
            let _ctx = SrtpContext::from_sdes_key(TEST_KEY_B64).unwrap();
        })
    });
}

/// Build a minimal RTCP Sender Report packet.
fn make_test_rtcp_packet() -> Vec<u8> {
    let mut pkt = vec![0u8; 28]; // minimal SR: header(4) + SSRC(4) + NTP(8) + RTP_TS(4) + counts(8)
    pkt[0] = 0x80; // V=2, P=0, RC=0
    pkt[1] = 200; // PT=200 (SR)
    let length: u16 = 6; // (28/4) - 1 = 6 words
    pkt[2] = (length >> 8) as u8;
    pkt[3] = (length & 0xFF) as u8;
    pkt
}

fn bench_srtcp_protect(c: &mut Criterion) {
    c.bench_function("srtcp_protect", |b| {
        b.iter_batched(
            || {
                let ctx = SrtcpContext::from_sdes_key(TEST_KEY_B64).unwrap();
                let pkt = make_test_rtcp_packet();
                (ctx, pkt)
            },
            |(mut ctx, pkt)| {
                let _protected = ctx.protect_rtcp(&pkt).unwrap();
            },
            BatchSize::SmallInput,
        )
    });
}

fn bench_srtcp_unprotect(c: &mut Criterion) {
    c.bench_function("srtcp_unprotect", |b| {
        b.iter_batched(
            || {
                let mut protect_ctx = SrtcpContext::from_sdes_key(TEST_KEY_B64).unwrap();
                let unprotect_ctx = SrtcpContext::from_sdes_key(TEST_KEY_B64).unwrap();
                let pkt = make_test_rtcp_packet();
                let protected = protect_ctx.protect_rtcp(&pkt).unwrap();
                (unprotect_ctx, protected)
            },
            |(mut ctx, protected)| {
                let _unprotected = ctx.unprotect_rtcp(&protected).unwrap();
            },
            BatchSize::SmallInput,
        )
    });
}

criterion_group!(
    benches,
    bench_srtp_protect,
    bench_srtp_unprotect,
    bench_srtp_key_derivation,
    bench_srtcp_protect,
    bench_srtcp_unprotect,
);
criterion_main!(benches);
