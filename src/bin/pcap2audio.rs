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
use std::io::{Read, Seek, SeekFrom, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::PathBuf;
use std::time::Duration;

use clap::{Parser, ValueEnum};
use serde::Serialize;

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
    /// Write machine-readable decoded-timeline metadata beside the WAV.
    #[arg(long)]
    metadata: Option<PathBuf>,
}

#[derive(Serialize)]
struct RenderMetadata {
    /// Epoch of sample zero: the earliest decodable RTP packet, matching decode_channel's origin.
    audio_origin_epoch_ms: u128,
    /// Duration of the rendered WAV timeline before any later transcoding.
    decoded_duration_ms: u128,
    sample_rate: u32,
    channels: u16,
}

/// One recorded RTP packet within a channel.
struct RtpPacket {
    /// Incremented on each descriptor for an existing channel. Concatenated recordings replay
    /// descriptors, so sequence/timestamp origins must never be ordered across this boundary.
    epoch: u32,
    seq: u16,
    ts: u32,
    /// PCAP capture (wall-clock) time — the timeline anchor, and the only timing
    /// signal for sources whose RTP timestamps are synthesized downstream
    /// (bridge/websocket, recorded pre-SynthClock with ts=0).
    capture: Duration,
    codec: AudioCodec,
    payload: Vec<u8>, // populated only by small in-memory unit fixtures
    offset: u64,
    length: usize,
}

/// Accumulated state for one endpoint's stream.
struct Channel {
    codec: Option<AudioCodec>,
    pt: Option<u8>,
    first_capture: Option<Duration>,
    packets: Vec<RtpPacket>,
    epoch: u32,
}

const MAX_PACKETS: usize = 1_000_000;
const MAX_CHANNELS: usize = 32;
const MAX_DURATION_SECS: u64 = 3600;
const MAX_OUTPUT_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_TEMP_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// Validate each captured length before allocating. A forged caplen must not
/// make the PCAP library allocate gigabytes before discovering a truncated file.
struct BoundedPcap {
    file: std::fs::File,
    little: bool,
    nanos: bool,
    count: usize,
    bytes: u64,
}
impl BoundedPcap {
    fn new(mut file: std::fs::File) -> anyhow::Result<Self> {
        anyhow::ensure!(file.metadata()?.is_file(), "input must be a regular file");
        anyhow::ensure!(
            file.metadata()?.len() <= MAX_TEMP_BYTES,
            "input exceeds 2 GiB"
        );
        let mut header = [0; 24];
        file.read_exact(&mut header)?;
        let (little, nanos) = match &header[..4] {
            [0xd4, 0xc3, 0xb2, 0xa1] => (true, false),
            [0xa1, 0xb2, 0xc3, 0xd4] => (false, false),
            [0x4d, 0x3c, 0xb2, 0xa1] => (true, true),
            [0xa1, 0xb2, 0x3c, 0x4d] => (false, true),
            _ => anyhow::bail!("not a supported PCAP file"),
        };
        let network = header[20..24].try_into()?;
        let network = if little {
            u32::from_le_bytes(network)
        } else {
            u32::from_be_bytes(network)
        };
        anyhow::ensure!(network == 1, "PCAP must use Ethernet link type");
        Ok(Self {
            file,
            little,
            nanos,
            count: 0,
            bytes: 24,
        })
    }
    fn next_packet(&mut self) -> anyhow::Result<Option<pcap_file::pcap::PcapPacket<'static>>> {
        let mut header = [0; 16];
        if self.file.read(&mut header[..1])? == 0 {
            return Ok(None);
        }
        self.file.read_exact(&mut header[1..])?;
        self.count += 1;
        anyhow::ensure!(self.count <= MAX_PACKETS, "capture exceeds packet limit");
        let read = |offset| {
            let bytes = header[offset..offset + 4].try_into().unwrap();
            if self.little {
                u32::from_le_bytes(bytes)
            } else {
                u32::from_be_bytes(bytes)
            }
        };
        let length = read(8);
        self.bytes += 16 + length as u64;
        anyhow::ensure!(
            length <= 65536 && self.bytes <= MAX_TEMP_BYTES,
            "capture exceeds size limit"
        );
        let fraction = read(4);
        anyhow::ensure!(
            fraction < if self.nanos { 1_000_000_000 } else { 1_000_000 },
            "invalid capture timestamp"
        );
        let timestamp = Duration::new(
            read(0) as u64,
            if self.nanos {
                fraction
            } else {
                fraction * 1000
            },
        );
        let mut data = vec![0; length as usize];
        self.file.read_exact(&mut data)?;
        Ok(Some(pcap_file::pcap::PcapPacket {
            timestamp,
            orig_len: read(12),
            data: std::borrow::Cow::Owned(data),
        }))
    }
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
    anyhow::ensure!(
        matches!(args.rate, 8000 | 16000 | 48000),
        "rate must be 8000, 16000 or 48000"
    );
    let mut reader = BoundedPcap::new(file)?;
    let mut encoded = tempfile::tempfile()?;
    let mut encoded_bytes = 0u64;

    // (src,dst) frame -> endpoint id, updated by descriptors in capture order.
    let mut addr_to_endpoint: HashMap<(SocketAddr, SocketAddr), String> = HashMap::new();
    // endpoint id -> channel.
    let mut channels: HashMap<String, Channel> = HashMap::new();
    let mut order: Vec<String> = Vec::new(); // endpoint ids in first-appearance order
    let mut unbound_rtp = 0u64;
    let mut undecodable_desc = 0u64;

    while let Some(pkt) = reader.next_packet()? {
        let Some((src, dst, payload)) = parse_frame(&pkt.data) else {
            continue;
        };
        match classify(payload) {
            PacketKind::Descriptor => {
                let Some(desc) = StreamDescriptor::parse(payload) else {
                    undecodable_desc += 1;
                    continue;
                };
                anyhow::ensure!(desc.endpoint_id.len() <= 128, "endpoint id too long");
                anyhow::ensure!(
                    addr_to_endpoint.len() < 4096 || addr_to_endpoint.contains_key(&(src, dst)),
                    "too many address mappings"
                );
                anyhow::ensure!(
                    channels.len() < MAX_CHANNELS || channels.contains_key(&desc.endpoint_id),
                    "too many audio channels"
                );
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
                        epoch: 0,
                    }
                });
                if !ch.packets.is_empty() {
                    ch.epoch = ch.epoch.saturating_add(1);
                }
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
                anyhow::ensure!(body.len() <= 16 * 1024, "audio payload exceeds limit");
                let offset = encoded_bytes;
                encoded_bytes += body.len() as u64;
                anyhow::ensure!(
                    encoded_bytes <= MAX_TEMP_BYTES,
                    "encoded spool exceeds limit"
                );
                encoded.write_all(body)?;
                ch.packets.push(RtpPacket {
                    epoch: ch.epoch,
                    seq,
                    ts,
                    capture: pkt.timestamp,
                    codec,
                    payload: Vec::new(),
                    offset,
                    length: body.len(),
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

    let mut rendered = Vec::new();
    let mut temporary_bytes = encoded_bytes;
    for endpoint_id in &order {
        let ch = channels.get_mut(endpoint_id).unwrap();
        if ch.packets.is_empty() {
            continue;
        }
        let mut spool = tempfile::tempfile()?;
        let samples = decode_channel_to(
            ch,
            args.rate,
            origin,
            &mut encoded,
            &mut spool,
            (MAX_TEMP_BYTES - temporary_bytes) / 2,
        )?;
        temporary_bytes = temporary_bytes
            .checked_add(samples * 2)
            .ok_or_else(|| anyhow::anyhow!("temporary size overflow"))?;
        anyhow::ensure!(
            temporary_bytes <= MAX_TEMP_BYTES,
            "decoded spools exceed 2 GiB"
        );
        spool.rewind()?;
        if samples > 0 {
            rendered.push((endpoint_id.clone(), spool, samples));
        }
    }
    anyhow::ensure!(!rendered.is_empty(), "no decodable audio found");
    let channels_out = match args.mode {
        Mode::Multichannel => rendered.len() as u16,
        Mode::Stereo => 2,
    };
    let frame_count = rendered
        .iter()
        .map(|(_, _, samples)| *samples)
        .max()
        .unwrap_or(0);
    let data_bytes = frame_count
        .checked_mul(channels_out as u64 * 2)
        .ok_or_else(|| anyhow::anyhow!("WAV size overflow"))?;
    anyhow::ensure!(
        data_bytes <= MAX_OUTPUT_BYTES && data_bytes <= u32::MAX as u64 - 36,
        "WAV exceeds output size limit"
    );
    anyhow::ensure!(
        temporary_bytes + data_bytes + 44 <= MAX_TEMP_BYTES,
        "conversion exceeds temporary storage limit"
    );
    let parent = args
        .output
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or(std::path::Path::new("."));
    let mut output = tempfile::NamedTempFile::new_in(parent)?;
    {
        let mut writer = std::io::BufWriter::new(output.as_file_mut());
        write_wav_header(&mut writer, data_bytes as u32, channels_out, args.rate)?;
        let mut position = 0;
        while position < frame_count {
            let count = (frame_count - position).min(4096) as usize;
            let mut blocks = Vec::with_capacity(rendered.len());
            for (_, spool, length) in &mut rendered {
                let actual = length.saturating_sub(position).min(count as u64) as usize;
                let mut bytes = vec![0u8; count * 2];
                spool.read_exact(&mut bytes[..actual * 2])?;
                blocks.push(bytes);
            }
            for index in 0..count {
                let sample = |channel: usize| {
                    i16::from_le_bytes([blocks[channel][index * 2], blocks[channel][index * 2 + 1]])
                };
                match args.mode {
                    Mode::Multichannel => {
                        for channel in 0..blocks.len() {
                            writer.write_all(&sample(channel).to_le_bytes())?;
                        }
                    }
                    Mode::Stereo => {
                        writer.write_all(&sample(0).to_le_bytes())?;
                        let sum: i64 = (1..blocks.len())
                            .map(|channel| sample(channel) as i64)
                            .sum();
                        writer.write_all(
                            &(sum.clamp(i16::MIN as i64, i16::MAX as i64) as i16).to_le_bytes(),
                        )?;
                    }
                }
            }
            position += count as u64;
        }
        writer.flush()?;
    }
    output.persist(&args.output)?;
    if let Some(metadata_path) = args.metadata {
        let metadata = RenderMetadata {
            audio_origin_epoch_ms: origin.as_millis(),
            decoded_duration_ms: (frame_count as u128 * 1000) / args.rate as u128,
            sample_rate: args.rate,
            channels: channels_out,
        };
        let serialized = serde_json::to_vec(&metadata)?;
        std::fs::write(metadata_path, serialized)?;
    }
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
fn decode_channel_to(
    ch: &mut Channel,
    out_rate: u32,
    origin: Duration,
    encoded: &mut std::fs::File,
    destination: &mut impl Write,
    max_samples: u64,
) -> anyhow::Result<u64> {
    let maximum = (out_rate as u64 * MAX_DURATION_SECS).min(max_samples);
    // Reorder by RTP sequence so stateful decoders (Opus/G.722) get in-order input.
    // The recording is arrival-ordered, so we unwrap the 16-bit sequence into a
    // monotonic key in arrival order (handling wraps and post-renegotiation resets)
    // and stable-sort by it. Degenerate (all-zero) sequence numbers — e.g.
    // bridge/websocket sources whose timeline is synthesized downstream — keep
    // arrival order.
    let first_capture = ch.first_capture.unwrap_or(origin);
    let lead = first_capture.saturating_sub(origin);
    let lead_samples = (lead.as_secs_f64() * out_rate as f64).round() as usize;
    anyhow::ensure!(
        lead_samples as u64 <= maximum,
        "capture duration exceeds one hour"
    );
    let mut destination = std::io::BufWriter::new(destination);
    write_silence(&mut destination, lead_samples as u64)?;
    let mut written = lead_samples as u64;

    let mut epoch_start = 0;
    while epoch_start < ch.packets.len() {
        let epoch = ch.packets[epoch_start].epoch;
        let epoch_end = ch.packets[epoch_start..]
            .iter()
            .position(|packet| packet.epoch != epoch)
            .map(|offset| epoch_start + offset)
            .unwrap_or(ch.packets.len());
        let epoch_packets = &ch.packets[epoch_start..epoch_end];
        let keys = unwrap_sequence(epoch_packets);
        let mut highest = keys.first().copied().unwrap_or(0);
        for &key in &keys {
            anyhow::ensure!(
                key >= highest - 512 && key <= highest + 16384,
                "sequence discontinuity exceeds supported reorder window; split recording epochs"
            );
            highest = highest.max(key);
        }
        let mut idx: Vec<usize> = (0..epoch_packets.len()).collect();
        idx.sort_by_key(|&i| keys[i]);
        let first_epoch_capture = epoch_packets
            .iter()
            .map(|packet| packet.capture)
            .min()
            .unwrap_or(first_capture);
        let first_ts = idx
            .first()
            .map(|&index| epoch_packets[index].ts)
            .unwrap_or(0);
        let mut cur_codec: Option<AudioCodec> = None;
        let mut decoder: Option<Box<dyn rtpbridge::media::codec::AudioDecoder>> = None;
        let mut resampler: Option<Resampler> = None;
        let mut pcm = Vec::new();
        let mut out = Vec::new();

        for index in idx {
            let p = &epoch_packets[index];
            // (Re)build the decoder/resampler when the codec changes.
            if cur_codec != Some(p.codec) {
                decoder = Some(make_decoder(p.codec)?);
                resampler = Some(Resampler::new(p.codec.sample_rate(), out_rate));
                cur_codec = Some(p.codec);
            }
            let dec = decoder.as_mut().unwrap();
            let mut payload = p.payload.clone();
            if payload.is_empty() && p.length > 0 {
                encoded.seek(SeekFrom::Start(p.offset))?;
                payload.resize(p.length, 0);
                encoded.read_exact(&mut payload)?;
            }
            if dec.decode(&payload, &mut pcm).is_err() {
                continue;
            }
            anyhow::ensure!(
                pcm.len() <= p.codec.ptime_samples() * 6,
                "decoded packet exceeds 120 ms"
            );
            resampler.as_mut().unwrap().process(&pcm, &mut out);

            // Position within an independently decoded epoch by RTP timestamp at the codec's RTP clock (8 kHz for G.722, not
            // its 16 kHz audio rate). When the RTP timestamp doesn't advance — sources
            // recorded before their timeline is stamped (bridge/websocket), or a
            // duplicate — fall back to PCAP capture wall-clock so real inter-packet gaps
            // are preserved rather than collapsed. Never goes backwards.
            let rel_ticks = p.ts.wrapping_sub(first_ts);
            let capture_target =
                (p.capture.saturating_sub(origin).as_secs_f64() * out_rate as f64).round() as usize;
            let rtp_target = if rel_ticks != 0 && rel_ticks <= 0x8000_0000 {
                ((rel_ticks as u64 * out_rate as u64) / p.codec.rtp_clock_rate() as u64) as usize
                    + (first_epoch_capture.saturating_sub(origin).as_secs_f64() * out_rate as f64)
                        .round() as usize
            } else {
                capture_target
            };
            // A restarted/foreign RTP epoch must never create an enormous sparse output. Capture time
            // is authoritative between recording segments; reject timestamp placement that disagrees by >2s.
            let target = if rtp_target.abs_diff(capture_target) > (out_rate as usize * 2) {
                capture_target
            } else {
                rtp_target
            };
            let target = (target as u64).max(written);
            let end = target
                .checked_add(out.len() as u64)
                .ok_or_else(|| anyhow::anyhow!("timeline overflow"))?;
            anyhow::ensure!(end <= maximum, "capture duration exceeds one hour");
            write_silence(&mut destination, target - written)?;
            for sample in &out {
                destination.write_all(&sample.to_le_bytes())?;
            }
            written = end;
        }
        epoch_start = epoch_end;
    }

    destination.flush()?;
    Ok(written)
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
fn write_silence(writer: &mut impl Write, samples: u64) -> anyhow::Result<()> {
    let zero = [0u8; 8192];
    let mut bytes = samples
        .checked_mul(2)
        .ok_or_else(|| anyhow::anyhow!("silence overflow"))?;
    while bytes > 0 {
        let count = bytes.min(zero.len() as u64) as usize;
        writer.write_all(&zero[..count])?;
        bytes -= count as u64;
    }
    Ok(())
}

fn write_wav_header(
    f: &mut impl Write,
    data_bytes: u32,
    channels: u16,
    rate: u32,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        data_bytes <= u32::MAX - 36 && channels <= MAX_CHANNELS as u16,
        "WAV size overflow"
    );
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
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pkt(seq: u16) -> RtpPacket {
        RtpPacket {
            epoch: 0,
            seq,
            ts: 0,
            capture: Duration::ZERO,
            codec: AudioCodec::Pcmu,
            payload: Vec::new(),
            offset: 0,
            length: 0,
        }
    }

    #[test]
    fn forged_capture_length_is_rejected_before_allocation() {
        let mut file = tempfile::tempfile().unwrap();
        let mut header = vec![0xd4, 0xc3, 0xb2, 0xa1];
        header.resize(24, 0);
        header[20] = 1;
        file.write_all(&header).unwrap();
        let mut record = [0u8; 16];
        record[8..12].copy_from_slice(&u32::MAX.to_le_bytes());
        file.write_all(&record).unwrap();
        file.rewind().unwrap();
        let mut reader = BoundedPcap::new(file).unwrap();
        assert!(reader.next_packet().is_err());
    }

    #[test]
    fn huge_capture_gap_fails_without_writing_the_gap() {
        let mut first = pkt(1);
        first.payload = vec![0xff; 160];
        let mut last = pkt(2);
        last.payload = vec![0xff; 160];
        last.capture = Duration::from_secs(86400);
        let mut channel = Channel {
            codec: Some(AudioCodec::Pcmu),
            pt: Some(0),
            first_capture: Some(Duration::ZERO),
            packets: vec![first, last],
            epoch: 0,
        };
        let mut encoded = tempfile::tempfile().unwrap();
        let mut output = Vec::new();
        let result = decode_channel_to(
            &mut channel,
            8000,
            Duration::ZERO,
            &mut encoded,
            &mut output,
            8000 * MAX_DURATION_SECS,
        );
        assert!(result.is_err());
        assert!(output.len() <= 320);
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
    fn decode_concatenated_recording_epochs_keep_capture_gap() {
        // A second collected PCAP replays descriptors and can restart both RTP sequence and timestamp.
        // Its capture-time gap must survive without sorting it into the first recording or allocating
        // based on unrelated timestamp origins.
        let channel = Channel {
            codec: Some(AudioCodec::Pcmu),
            pt: Some(0),
            first_capture: Some(Duration::ZERO),
            epoch: 1,
            packets: vec![
                RtpPacket {
                    epoch: 0,
                    seq: 65_000,
                    ts: 3_000_000_000,
                    capture: Duration::ZERO,
                    codec: AudioCodec::Pcmu,
                    payload: vec![0xff; 160],
                    offset: 0,
                    length: 0,
                },
                RtpPacket {
                    epoch: 1,
                    seq: 7,
                    ts: 17,
                    capture: Duration::from_secs(3),
                    codec: AudioCodec::Pcmu,
                    payload: vec![0xff; 160],
                    offset: 0,
                    length: 0,
                },
            ],
        };
        let mut channel = channel;
        let mut decoded = Vec::new();
        let mut encoded = tempfile::tempfile().unwrap();
        decode_channel_to(
            &mut channel,
            8000,
            Duration::ZERO,
            &mut encoded,
            &mut decoded,
            8000 * MAX_DURATION_SECS,
        )
        .expect("decodes independent epochs");
        let decoded: Vec<_> = decoded.chunks_exact(2).collect();
        assert!(
            decoded.len() >= 24_160,
            "capture-time gap retained: {} samples",
            decoded.len()
        );
        assert!(
            decoded.len() < 40_000,
            "foreign RTP timestamps must not allocate a huge output: {} samples",
            decoded.len()
        );
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
