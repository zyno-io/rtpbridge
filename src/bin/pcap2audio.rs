//! Decode an rtpbridge codec-tagged PCAP recording into a WAV file.
//!
//! Each recorded endpoint is a "channel": a `RBP1` descriptor packet declaring the
//! codec, followed by that endpoint's RTP. This tool demuxes by the frame
//! `(src,dst)` pair (bound to an endpoint by descriptors in capture order), decodes
//! each channel, aligns them on a common wall-clock timeline (PCAP capture time +
//! RTP-timestamp gap fill), resamples to a common rate, and writes WAV — either
//! one channel per endpoint (`multichannel`) or a stereo downmix where the first
//! endpoint is left and all others are summed into right (`stereo`).
//!
//! Conversion to Opus/MP3/etc. is left to external tooling (e.g. ffmpeg).

use std::collections::HashMap;
use std::io::Write;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::PathBuf;
use std::time::Duration;

use clap::{Parser, ValueEnum};

use rtpbridge::media::codec::{AudioCodec, make_decoder};
use rtpbridge::media::resample::Resampler;
use rtpbridge::recording::meta::{PacketKind, StreamDescriptor, classify};

#[derive(Clone, Copy, Debug, ValueEnum)]
enum Mode {
    /// One WAV channel per endpoint, ordered by first appearance.
    Multichannel,
    /// Stereo: left = first endpoint, right = sum of all others.
    Stereo,
}

#[derive(Parser, Debug)]
#[command(
    name = "pcap2audio",
    about = "Decode an rtpbridge codec-tagged PCAP recording into a WAV file"
)]
struct Args {
    /// Input PCAP file.
    input: PathBuf,
    /// Output WAV file.
    #[arg(short, long)]
    output: PathBuf,
    /// Output layout.
    #[arg(long, value_enum, default_value = "stereo")]
    mode: Mode,
    /// Output sample rate (Hz).
    #[arg(long, default_value_t = 48000)]
    rate: u32,
}

/// One recorded RTP packet within a channel.
struct RtpPacket {
    seq: u16,
    ts: u32,
    /// PCAP capture (wall-clock) time — the timeline anchor, and the only timing
    /// signal for sources whose RTP timestamps are synthesized downstream
    /// (bridge/websocket, recorded pre-SynthClock with ts=0).
    capture: Duration,
    codec: AudioCodec,
    payload: Vec<u8>,
}

/// Accumulated state for one endpoint's stream.
struct Channel {
    codec: Option<AudioCodec>,
    pt: Option<u8>,
    first_capture: Option<Duration>,
    packets: Vec<RtpPacket>,
}

fn main() {
    if let Err(e) = run() {
        eprintln!("pcap2audio: {e}");
        std::process::exit(1);
    }
}

fn run() -> anyhow::Result<()> {
    let args = Args::parse();

    let file = std::fs::File::open(&args.input)
        .map_err(|e| anyhow::anyhow!("opening {}: {e}", args.input.display()))?;
    let mut reader = pcap_file::pcap::PcapReader::new(file)
        .map_err(|e| anyhow::anyhow!("not a valid PCAP: {e}"))?;

    // (src,dst) frame -> endpoint id, updated by descriptors in capture order.
    let mut addr_to_endpoint: HashMap<(SocketAddr, SocketAddr), String> = HashMap::new();
    // endpoint id -> channel.
    let mut channels: HashMap<String, Channel> = HashMap::new();
    let mut order: Vec<String> = Vec::new(); // endpoint ids in first-appearance order
    let mut unbound_rtp = 0u64;
    let mut undecodable_desc = 0u64;

    while let Some(pkt) = reader.next_packet() {
        let pkt = pkt.map_err(|e| anyhow::anyhow!("reading packet: {e}"))?;
        let Some((src, dst, payload)) = parse_frame(&pkt.data) else {
            continue;
        };
        match classify(payload) {
            PacketKind::Descriptor => {
                let Some(desc) = StreamDescriptor::parse(payload) else {
                    undecodable_desc += 1;
                    continue;
                };
                let Some(codec) = codec_from_descriptor(&desc) else {
                    // Unsupported codec (e.g. PCMA): bind the frame so its media is
                    // recognised-and-skipped rather than counted as unbound.
                    addr_to_endpoint.insert((src, dst), desc.endpoint_id.clone());
                    continue;
                };
                addr_to_endpoint.insert((src, dst), desc.endpoint_id.clone());
                let ch = channels.entry(desc.endpoint_id.clone()).or_insert_with(|| {
                    order.push(desc.endpoint_id.clone());
                    Channel {
                        codec: None,
                        pt: None,
                        first_capture: None,
                        packets: Vec::new(),
                    }
                });
                ch.codec = Some(codec);
                ch.pt = Some(desc.pt);
            }
            PacketKind::Rtcp => {}
            PacketKind::Rtp => {
                let Some(endpoint_id) = addr_to_endpoint.get(&(src, dst)) else {
                    unbound_rtp += 1;
                    continue;
                };
                let Some(ch) = channels.get_mut(endpoint_id) else {
                    continue;
                };
                let (Some(codec), Some(want_pt)) = (ch.codec, ch.pt) else {
                    continue;
                };
                let Some((pt, seq, ts, body)) = parse_rtp(payload) else {
                    continue;
                };
                // Skip anything that isn't the declared audio PT (telephone-event, CN).
                if pt != want_pt {
                    continue;
                }
                if ch.first_capture.is_none() {
                    ch.first_capture = Some(pkt.timestamp);
                }
                ch.packets.push(RtpPacket {
                    seq,
                    ts,
                    capture: pkt.timestamp,
                    codec,
                    payload: body.to_vec(),
                });
            }
        }
    }

    if unbound_rtp > 0 {
        eprintln!("pcap2audio: skipped {unbound_rtp} RTP packets with no descriptor");
    }
    if undecodable_desc > 0 {
        eprintln!("pcap2audio: skipped {undecodable_desc} malformed descriptors");
    }

    // Decode each channel to PCM at the output rate, with leading silence so all
    // channels share one wall-clock origin.
    let origin = channels
        .values()
        .filter_map(|c| c.first_capture)
        .min()
        .ok_or_else(|| anyhow::anyhow!("no decodable audio found in {}", args.input.display()))?;

    let mut rendered: Vec<(String, Vec<i16>)> = Vec::new();
    for endpoint_id in &order {
        let ch = channels.get_mut(endpoint_id).unwrap();
        if ch.packets.is_empty() {
            continue;
        }
        let pcm = decode_channel(ch, args.rate, origin)?;
        if !pcm.is_empty() {
            rendered.push((endpoint_id.clone(), pcm));
        }
    }

    if rendered.is_empty() {
        anyhow::bail!("no decodable audio found in {}", args.input.display());
    }

    let interleaved = match args.mode {
        Mode::Multichannel => {
            let mapping: Vec<String> = rendered.iter().map(|(id, _)| id.clone()).collect();
            eprintln!("pcap2audio: {} channels:", mapping.len());
            for (i, id) in mapping.iter().enumerate() {
                eprintln!("  channel {i} = endpoint {id}");
            }
            interleave(rendered.iter().map(|(_, p)| p.as_slice()).collect())
        }
        Mode::Stereo => {
            let left = &rendered[0].1;
            let others: Vec<&[i16]> = rendered[1..].iter().map(|(_, p)| p.as_slice()).collect();
            let right = sum_saturating(&others);
            interleave(vec![left.as_slice(), right.as_slice()])
        }
    };

    let channels_out = match args.mode {
        Mode::Multichannel => rendered.len() as u16,
        Mode::Stereo => 2,
    };
    write_wav(&args.output, &interleaved, channels_out, args.rate)?;
    eprintln!(
        "pcap2audio: wrote {} ({} ch @ {} Hz)",
        args.output.display(),
        channels_out,
        args.rate
    );
    Ok(())
}

/// Decode one channel's packets into PCM at `out_rate`, prefixed with leading
/// silence so its first sample lands at `(first_capture - origin)`.
fn decode_channel(ch: &mut Channel, out_rate: u32, origin: Duration) -> anyhow::Result<Vec<i16>> {
    // Reorder by RTP sequence so stateful decoders (Opus/G.722) get in-order input.
    // The recording is arrival-ordered, so we unwrap the 16-bit sequence into a
    // monotonic key in arrival order (handling wraps and post-renegotiation resets)
    // and stable-sort by it. Degenerate (all-zero) sequence numbers — e.g.
    // bridge/websocket sources whose timeline is synthesized downstream — keep
    // arrival order.
    let keys = unwrap_sequence(&ch.packets);
    let mut idx: Vec<usize> = (0..ch.packets.len()).collect();
    idx.sort_by_key(|&i| keys[i]);
    let packets: Vec<&RtpPacket> = idx.iter().map(|&i| &ch.packets[i]).collect();

    let first_capture = ch.first_capture.unwrap_or(origin);
    let first_ts = packets.first().map(|p| p.ts).unwrap_or(0);

    let lead = first_capture.saturating_sub(origin);
    let lead_samples = (lead.as_secs_f64() * out_rate as f64).round() as usize;
    let mut buf: Vec<i16> = vec![0; lead_samples];

    let mut cur_codec: Option<AudioCodec> = None;
    let mut decoder: Option<Box<dyn rtpbridge::media::codec::AudioDecoder>> = None;
    let mut resampler: Option<Resampler> = None;
    let mut pcm = Vec::new();
    let mut out = Vec::new();

    for p in &packets {
        // (Re)build the decoder/resampler when the codec changes.
        if cur_codec != Some(p.codec) {
            decoder = Some(make_decoder(p.codec)?);
            resampler = Some(Resampler::new(p.codec.sample_rate(), out_rate));
            cur_codec = Some(p.codec);
        }
        let dec = decoder.as_mut().unwrap();
        if dec.decode(&p.payload, &mut pcm).is_err() {
            continue;
        }
        resampler.as_mut().unwrap().process(&pcm, &mut out);

        // Position by RTP timestamp at the codec's RTP clock (8 kHz for G.722, not
        // its 16 kHz audio rate). When the RTP timestamp doesn't advance — sources
        // recorded before their timeline is stamped (bridge/websocket), or a
        // duplicate — fall back to PCAP capture wall-clock so real inter-packet gaps
        // are preserved rather than collapsed. Never goes backwards.
        let rel_ticks = p.ts.wrapping_sub(first_ts);
        let target = if rel_ticks != 0 && rel_ticks <= 0x8000_0000 {
            ((rel_ticks as u64 * out_rate as u64) / p.codec.rtp_clock_rate() as u64) as usize
                + lead_samples
        } else {
            let cap = p.capture.saturating_sub(first_capture);
            (cap.as_secs_f64() * out_rate as f64).round() as usize + lead_samples
        };
        let target = target.max(buf.len());
        if target > buf.len() {
            buf.resize(target, 0); // silence across the gap
        }
        buf.extend_from_slice(&out);
    }

    Ok(buf)
}

/// Unwrap 16-bit RTP sequence numbers (in arrival order) into a monotonic i64 key
/// by accumulating the **signed** 16-bit delta between consecutive packets. This
/// correctly handles forward wrap, small reordering, AND reordering across the wrap
/// boundary (a delayed pre-wrap packet arriving after a post-wrap one), as long as
/// adjacent packets are within ±2^15 of each other (true for RTP with bounded
/// jitter). Sorting by the result reconstructs sequence order; equal keys (e.g.
/// all-zero degenerate sequences) keep arrival order under a stable sort.
fn unwrap_sequence(packets: &[RtpPacket]) -> Vec<i64> {
    let mut keys = Vec::with_capacity(packets.len());
    let mut prev: Option<u16> = None;
    let mut ext: i64 = 0;
    for p in packets {
        match prev {
            None => ext = p.seq as i64,
            Some(pv) => ext += p.seq.wrapping_sub(pv) as i16 as i64,
        }
        keys.push(ext);
        prev = Some(p.seq);
    }
    keys
}

/// Map a descriptor's codec name to an `AudioCodec`. Returns `None` for codecs not
/// supported by this build (e.g. PCMA).
fn codec_from_descriptor(d: &StreamDescriptor) -> Option<AudioCodec> {
    match d.codec.as_str() {
        "PCMU" => Some(AudioCodec::Pcmu),
        "G722" => Some(AudioCodec::G722),
        "opus" => Some(AudioCodec::Opus),
        // Guard a malformed descriptor: a 0 sample rate would later panic the
        // resampler. Treat it as unsupported (the channel is skipped).
        "L16" if d.clock_rate > 0 => Some(AudioCodec::L16 {
            sample_rate: d.clock_rate,
        }),
        _ => None,
    }
}

/// Strip Ethernet/IPv4|IPv6/UDP framing, returning `(src, dst, udp_payload)`.
fn parse_frame(data: &[u8]) -> Option<(SocketAddr, SocketAddr, &[u8])> {
    if data.len() < 14 {
        return None;
    }
    let ethertype = u16::from_be_bytes([data[12], data[13]]);
    match ethertype {
        0x0800 => {
            // IPv4
            if data.len() < 34 {
                return None;
            }
            let ihl = (data[14] & 0x0F) as usize * 4;
            let ip_end = 14 + ihl;
            if data[23] != 17 || data.len() < ip_end + 8 {
                return None; // not UDP / truncated
            }
            let src_ip = Ipv4Addr::new(data[26], data[27], data[28], data[29]);
            let dst_ip = Ipv4Addr::new(data[30], data[31], data[32], data[33]);
            let src_port = u16::from_be_bytes([data[ip_end], data[ip_end + 1]]);
            let dst_port = u16::from_be_bytes([data[ip_end + 2], data[ip_end + 3]]);
            let payload = &data[ip_end + 8..];
            Some((
                SocketAddr::new(IpAddr::V4(src_ip), src_port),
                SocketAddr::new(IpAddr::V4(dst_ip), dst_port),
                payload,
            ))
        }
        0x86DD => {
            // IPv6 (no extension headers expected from our writer)
            if data.len() < 62 || data[20] != 17 {
                return None;
            }
            let src_ip = Ipv6Addr::from(<[u8; 16]>::try_from(&data[22..38]).ok()?);
            let dst_ip = Ipv6Addr::from(<[u8; 16]>::try_from(&data[38..54]).ok()?);
            let src_port = u16::from_be_bytes([data[54], data[55]]);
            let dst_port = u16::from_be_bytes([data[56], data[57]]);
            let payload = &data[62..];
            Some((
                SocketAddr::new(IpAddr::V6(src_ip), src_port),
                SocketAddr::new(IpAddr::V6(dst_ip), dst_port),
                payload,
            ))
        }
        _ => None,
    }
}

/// Parse an RTP header, returning `(pt, seq, ts, body)`. Handles CSRC and one
/// extension header.
fn parse_rtp(p: &[u8]) -> Option<(u8, u16, u32, &[u8])> {
    if p.len() < 12 || (p[0] >> 6) != 2 {
        return None;
    }
    let cc = (p[0] & 0x0F) as usize;
    let pt = p[1] & 0x7F;
    let seq = u16::from_be_bytes([p[2], p[3]]);
    let ts = u32::from_be_bytes([p[4], p[5], p[6], p[7]]);
    let mut offset = 12 + cc * 4;
    if p[0] & 0x10 != 0 {
        // Extension header: 4-byte prefix + length words.
        if p.len() < offset + 4 {
            return None;
        }
        let ext_words = u16::from_be_bytes([p[offset + 2], p[offset + 3]]) as usize;
        offset += 4 + ext_words * 4;
    }
    if p.len() < offset {
        return None;
    }
    Some((pt, seq, ts, &p[offset..]))
}

/// Sum several mono PCM buffers with saturation.
fn sum_saturating(bufs: &[&[i16]]) -> Vec<i16> {
    let len = bufs.iter().map(|b| b.len()).max().unwrap_or(0);
    let mut out = vec![0i16; len];
    for b in bufs {
        for (o, &s) in out.iter_mut().zip(b.iter()) {
            *o = o.saturating_add(s);
        }
    }
    out
}

/// Interleave mono channels into one buffer (channels padded to the max length).
fn interleave(chans: Vec<&[i16]>) -> Vec<i16> {
    let n = chans.len();
    let len = chans.iter().map(|c| c.len()).max().unwrap_or(0);
    let mut out = vec![0i16; len * n];
    for (ci, c) in chans.iter().enumerate() {
        for (i, &s) in c.iter().enumerate() {
            out[i * n + ci] = s;
        }
    }
    out
}

/// Write interleaved 16-bit PCM as a WAV file.
fn write_wav(path: &PathBuf, samples: &[i16], channels: u16, rate: u32) -> anyhow::Result<()> {
    let mut f = std::io::BufWriter::new(std::fs::File::create(path)?);
    let data_bytes = (samples.len() * 2) as u32;
    let byte_rate = rate * channels as u32 * 2;
    let block_align = channels * 2;
    f.write_all(b"RIFF")?;
    f.write_all(&(36 + data_bytes).to_le_bytes())?;
    f.write_all(b"WAVE")?;
    f.write_all(b"fmt ")?;
    f.write_all(&16u32.to_le_bytes())?;
    f.write_all(&1u16.to_le_bytes())?; // PCM
    f.write_all(&channels.to_le_bytes())?;
    f.write_all(&rate.to_le_bytes())?;
    f.write_all(&byte_rate.to_le_bytes())?;
    f.write_all(&block_align.to_le_bytes())?;
    f.write_all(&16u16.to_le_bytes())?;
    f.write_all(b"data")?;
    f.write_all(&data_bytes.to_le_bytes())?;
    for s in samples {
        f.write_all(&s.to_le_bytes())?;
    }
    f.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pkt(seq: u16) -> RtpPacket {
        RtpPacket {
            seq,
            ts: 0,
            capture: Duration::ZERO,
            codec: AudioCodec::Pcmu,
            payload: Vec::new(),
        }
    }

    #[test]
    fn unwrap_sequence_handles_wrap() {
        let pkts: Vec<RtpPacket> = [65533u16, 65534, 65535, 0, 1, 2]
            .iter()
            .map(|&s| pkt(s))
            .collect();
        let keys = unwrap_sequence(&pkts);
        // Keys must be strictly increasing across the 16-bit wrap boundary.
        for w in keys.windows(2) {
            assert!(w[1] > w[0], "monotonic across wrap: {keys:?}");
        }
        assert_eq!(keys[3] - keys[2], 1, "65535 -> 0 advances by one");
    }

    #[test]
    fn unwrap_sequence_handles_reorder_across_wrap() {
        // 65535 is delayed and arrives AFTER the post-wrap 0; sorting by the
        // unwrapped key must still reconstruct 65534, 65535, 0, 1.
        let pkts: Vec<RtpPacket> = [65534u16, 0, 65535, 1].iter().map(|&s| pkt(s)).collect();
        let keys = unwrap_sequence(&pkts);
        let mut idx: Vec<usize> = (0..pkts.len()).collect();
        idx.sort_by_key(|&i| keys[i]);
        let ordered: Vec<u16> = idx.iter().map(|&i| pkts[i].seq).collect();
        assert_eq!(ordered, vec![65534, 65535, 0, 1]);
    }

    #[test]
    fn unwrap_sequence_keeps_small_reorder_in_epoch() {
        // A small backward step (in-window jitter) must NOT be treated as a wrap.
        let pkts: Vec<RtpPacket> = [100u16, 102, 101, 103].iter().map(|&s| pkt(s)).collect();
        let keys = unwrap_sequence(&pkts);
        assert_eq!(keys, vec![100, 102, 101, 103]);
    }

    #[test]
    fn parse_rtp_skips_csrc_and_extension() {
        // V=2, CC=1, X=1, PT=0; 1 CSRC (4 bytes); ext header (4-byte prefix + 1 word).
        let mut p = vec![0x91u8, 0x00, 0x00, 0x05]; // byte0: V=2,X=1,CC=1
        p.extend_from_slice(&[0, 0, 0, 0]); // timestamp
        p.extend_from_slice(&[0, 0, 0, 0]); // ssrc
        p.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]); // 1 CSRC
        p.extend_from_slice(&[0xBE, 0xDE, 0x00, 0x01]); // ext: profile + length=1 word
        p.extend_from_slice(&[1, 2, 3, 4]); // 1 ext word
        p.extend_from_slice(&[0xAA, 0xBB]); // body
        let (pt, seq, _ts, body) = parse_rtp(&p).expect("parses");
        assert_eq!(pt, 0);
        assert_eq!(seq, 5);
        assert_eq!(body, &[0xAA, 0xBB]);
    }

    #[test]
    fn parse_frame_ipv4_udp() {
        // Minimal Eth + IPv4(20) + UDP(8) + 2-byte payload.
        let mut f = vec![0u8; 14];
        f[12] = 0x08;
        f[13] = 0x00; // IPv4
        let mut ip = vec![0u8; 20];
        ip[0] = 0x45; // version 4, IHL 5
        ip[9] = 17; // UDP
        ip[12..16].copy_from_slice(&[10, 0, 0, 1]); // src
        ip[16..20].copy_from_slice(&[10, 255, 0, 1]); // dst
        f.extend_from_slice(&ip);
        f.extend_from_slice(&[0x27, 0x10, 0x27, 0x10, 0, 0, 0, 0]); // UDP ports 10000/10000
        f.extend_from_slice(&[0xAB, 0xCD]); // payload
        let (src, dst, payload) = parse_frame(&f).expect("parses");
        assert_eq!(src.to_string(), "10.0.0.1:10000");
        assert_eq!(dst.to_string(), "10.255.0.1:10000");
        assert_eq!(payload, &[0xAB, 0xCD]);
    }
}
