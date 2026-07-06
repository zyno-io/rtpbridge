use crate::media::codec::AudioCodec;
use std::net::SocketAddr;

/// Codec info for SDP generation
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SdpCodec {
    pub pt: u8,
    pub name: &'static str,
    pub clock_rate: u32,
    pub channels: Option<u8>,
    pub fmtp: Option<&'static str>,
}

/// Well-known codec definitions
pub const CODEC_PCMU: SdpCodec = SdpCodec {
    pt: 0,
    name: "PCMU",
    clock_rate: 8000,
    channels: None,
    fmtp: None,
};

pub const CODEC_G722: SdpCodec = SdpCodec {
    pt: 9,
    name: "G722",
    clock_rate: 8000, // SDP says 8000 even though it's actually 16kHz
    channels: None,
    fmtp: None,
};

pub const CODEC_OPUS: SdpCodec = SdpCodec {
    pt: 111,
    name: "opus",
    clock_rate: 48000,
    channels: Some(2), // RFC 7587 §7 mandates channels=2 in rtpmap even for mono; stereo=0 in fmtp is the actual mono/stereo signal
    fmtp: Some("minptime=10;useinbandfec=1;stereo=0;sprop-stereo=0"),
};

pub const CODEC_TELEPHONE_EVENT: SdpCodec = SdpCodec {
    pt: 101,
    name: "telephone-event",
    clock_rate: 8000,
    channels: None,
    fmtp: Some("0-16"),
};

/// Audio-quality ranking used when answering an offer. Higher = better
/// fidelity. Ranked by real audio bandwidth, NOT the SDP clock rate — G.722
/// advertises an 8 kHz clock but carries 16 kHz wideband audio.
/// Unknown names, `telephone-event`, and L16 rank lowest.
fn codec_quality(c: &SdpCodec) -> u8 {
    match AudioCodec::from_name(c.name) {
        Some(AudioCodec::Opus) => 3, // 48 kHz fullband
        Some(AudioCodec::G722) => 2, // 16 kHz wideband
        Some(AudioCodec::Pcmu) => 1, // 8 kHz narrowband
        _ => 0,
    }
}

/// Select the media codec to use when answering an offer.
///
/// Rather than honoring the offerer's first-listed preference (the bare
/// RFC 3264 default), we pick the highest-quality codec the offerer supports,
/// so a bridged leg stays as wideband as the far end allows. `telephone-event`
/// is never chosen as the media codec; ties keep the first-listed codec.
pub fn select_answer_codec(codecs: &[SdpCodec]) -> Option<&SdpCodec> {
    codecs
        .iter()
        .filter(|c| c.name != "telephone-event")
        .reduce(|best, c| {
            if codec_quality(c) > codec_quality(best) {
                c
            } else {
                best
            }
        })
}

/// Codecs to advertise in a plain-RTP offer.
///
/// With no caller preference (`prefer` is `None`), codecs are advertised
/// highest audio quality first (Opus > G.722 > PCMU) so an answerer doing the
/// RFC 3264 default — take the offerer's first-listed mutually-supported codec —
/// lands on the best codec both legs share. This is the offer-side mirror of
/// [`select_answer_codec`]; a PCMU-first default would instead push SIP peers
/// onto narrowband even when Opus or G.722 are available.
///
/// When `prefer` is `Some`, it carries the caller's preferred codec order (the
/// control-plane `codecs` field, documented as "preferred codec order"): codecs
/// are advertised in exactly that order, matched case-insensitively, with
/// unknown and duplicate names skipped.
///
/// `telephone-event` is always advertised last for DTMF (RFC 4733), regardless
/// of `prefer`.
pub fn offer_codec_list(prefer: Option<&[String]>) -> Vec<SdpCodec> {
    let known = [CODEC_OPUS, CODEC_G722, CODEC_PCMU];
    let mut codecs: Vec<SdpCodec> = match prefer {
        Some(names) => {
            let mut ordered: Vec<SdpCodec> = Vec::with_capacity(names.len());
            for name in names {
                if let Some(c) = known.iter().find(|c| name.eq_ignore_ascii_case(c.name))
                    && !ordered.iter().any(|e| e.pt == c.pt)
                {
                    ordered.push(c.clone());
                }
            }
            ordered
        }
        None => known.to_vec(),
    };
    codecs.push(CODEC_TELEPHONE_EVENT);
    codecs
}

/// Parsed SDP info relevant to plain RTP
#[derive(Debug, Clone)]
pub struct ParsedSdp {
    pub remote_addr: Option<SocketAddr>,
    pub codecs: Vec<SdpCodec>,
    pub telephone_event_pt: Option<u8>,
    /// Negotiated telephone-event rtpmap clock (RFC 4733). `None` if no
    /// telephone-event was advertised; consumers default to 8000. Tracked
    /// independently of the media codec clock since DTMF event durations are
    /// expressed in this clock, not the audio codec's.
    pub telephone_event_clock_rate: Option<u32>,
    pub crypto: Option<SdpCrypto>,
    pub is_webrtc: bool,
    pub direction: Option<String>,
    pub rtcp_mux: bool,
    /// Media protocol from m= line (e.g., "RTP/AVP", "RTP/SAVP", "UDP/TLS/RTP/SAVPF")
    pub media_protocol: Option<String>,
    /// True if this is OSRTP: RTP/AVP profile with a=crypto present (RFC 8643)
    /// The endpoint should use SRTP if crypto is available, but the profile is "plain"
    pub is_osrtp: bool,
}

/// SRTP SDES crypto attribute
#[derive(Debug, Clone)]
pub struct SdpCrypto {
    pub tag: u32,
    pub suite: String,
    pub key_b64: String,
}

/// Parse relevant fields from an SDP string
pub fn parse_sdp(sdp: &str) -> ParsedSdp {
    let mut result = ParsedSdp {
        remote_addr: None,
        codecs: Vec::new(),
        telephone_event_pt: None,
        telephone_event_clock_rate: None,
        crypto: None,
        is_webrtc: false,
        direction: None,
        rtcp_mux: false,
        media_protocol: None,
        is_osrtp: false,
    };

    let mut session_c_addr: Option<std::net::IpAddr> = None;
    let mut audio_c_addr: Option<std::net::IpAddr> = None;
    let mut m_port: Option<u16> = None;
    let mut pts: Vec<u8> = Vec::new();
    // Parsed rtpmap entries: PT → (name, clock_rate, channels)
    let mut rtpmap: std::collections::HashMap<u8, (String, u32, Option<u8>)> =
        std::collections::HashMap::new();
    // Track which media section we're in:
    // None = session level (before any m= line)
    // Some(true) = inside m=audio section
    // Some(false) = inside a non-audio m= section (e.g. m=video)
    // Attributes from non-audio sections are ignored to prevent cross-section PT collisions.
    let mut media_section: Option<bool> = None;

    for line in sdp.lines() {
        let line = line.trim();

        if let Some(rest) = line
            .strip_prefix("c=IN IP4 ")
            .or_else(|| line.strip_prefix("c=IN IP6 "))
        {
            let addr = rest.split_whitespace().next().and_then(|a| a.parse().ok());
            match media_section {
                None => session_c_addr = addr,     // session-level c=
                Some(true) => audio_c_addr = addr, // audio media-level c=
                Some(false) => {}                  // non-audio media — ignore
            }
        } else if line.starts_with("m=") && !line.starts_with("m=audio ") {
            // Non-audio media section — stop collecting attributes
            media_section = Some(false);
            continue;
        } else if let Some(rest) = line.strip_prefix("m=audio ") {
            media_section = Some(true);
            let parts: Vec<&str> = rest.split_whitespace().collect();
            if let Some(port_str) = parts.first() {
                m_port = port_str.parse().ok();
            }
            // Capture media protocol (e.g., "RTP/AVP", "RTP/SAVP")
            if parts.len() >= 2 {
                result.media_protocol = Some(parts[1].to_string());
            }
            // Collect payload types from m= line (cap to prevent DoS from huge SDP)
            const MAX_SDP_CODECS: usize = 32;
            let total_pts = parts
                .iter()
                .skip(2)
                .filter(|s| s.parse::<u8>().is_ok())
                .count();
            for pt_str in parts.iter().skip(2) {
                if pts.len() >= MAX_SDP_CODECS {
                    if total_pts > MAX_SDP_CODECS {
                        tracing::warn!(
                            total = total_pts,
                            max = MAX_SDP_CODECS,
                            "SDP contains more codecs than supported, truncating"
                        );
                    }
                    break;
                }
                // skip port and proto
                if let Ok(pt) = pt_str.parse::<u8>() {
                    pts.push(pt);
                }
            }
        } else if media_section == Some(false) {
            // Ignore attributes from non-audio media sections.
            // Session-level WebRTC indicators (fingerprint/ice-ufrag) are still checked below.
            if line.starts_with("a=fingerprint:") || line.starts_with("a=ice-ufrag:") {
                result.is_webrtc = true;
            }
        } else if let Some(rest) = line.strip_prefix("a=rtpmap:") {
            // e.g., "111 opus/48000/2"
            let parts: Vec<&str> = rest.splitn(2, ' ').collect();
            if parts.len() == 2
                && let Ok(pt) = parts[0].parse::<u8>()
            {
                let codec_parts: Vec<&str> = parts[1].split('/').collect();
                let name = codec_parts[0];
                let clock_rate = codec_parts
                    .get(1)
                    .and_then(|s| s.parse::<u32>().ok())
                    .unwrap_or(0);
                let channels = codec_parts.get(2).and_then(|s| s.parse::<u8>().ok());
                if rtpmap.len() < 32 {
                    rtpmap.insert(pt, (name.to_string(), clock_rate, channels));
                }
                if name.eq_ignore_ascii_case("telephone-event") {
                    result.telephone_event_pt = Some(pt);
                    // Track the negotiated DTMF clock; default a malformed/zero
                    // rate to the 8000 SIP convention.
                    result.telephone_event_clock_rate =
                        Some(if clock_rate > 0 { clock_rate } else { 8000 });
                }
            }
        } else if let Some(rest) = line.strip_prefix("a=crypto:") {
            // e.g., "1 AES_CM_128_HMAC_SHA1_80 inline:base64key..."
            let parts: Vec<&str> = rest.splitn(3, ' ').collect();
            if parts.len() == 3 {
                let tag = parts[0].parse().unwrap_or(1);
                let suite = parts[1].to_string();
                let key_material = parts[2].strip_prefix("inline:").unwrap_or(parts[2]);
                // Strip RFC 4568 lifetime/MKI parameters after '|'
                let key_b64 = key_material
                    .split('|')
                    .next()
                    .unwrap_or(key_material)
                    .to_string();
                // Only accept the supported cipher suite
                if suite == "AES_CM_128_HMAC_SHA1_80" {
                    result.crypto = Some(SdpCrypto {
                        tag,
                        suite,
                        key_b64,
                    });
                }
            }
        } else if line.starts_with("a=fingerprint:") || line.starts_with("a=ice-ufrag:") {
            result.is_webrtc = true;
        } else if line == "a=sendrecv" {
            result.direction = Some("sendrecv".into());
        } else if line == "a=recvonly" {
            result.direction = Some("recvonly".into());
        } else if line == "a=sendonly" {
            result.direction = Some("sendonly".into());
        } else if line == "a=inactive" {
            result.direction = Some("inactive".into());
        } else if line == "a=rtcp-mux" {
            result.rtcp_mux = true;
        }
    }

    // Prefer audio media-level c= over session-level c= (RFC 4566 §5.7)
    let c_addr = audio_c_addr.or(session_c_addr);
    if let (Some(addr), Some(port)) = (c_addr, m_port) {
        // Port 0 means the media stream is rejected/inactive (RFC 3264 §6).
        // Leave remote_addr as None so downstream code treats it as receive-only.
        if port != 0 {
            result.remote_addr = Some(SocketAddr::new(addr, port));
        }
    }

    // Map PTs to codecs using well-known PTs and rtpmap entries
    for pt in pts {
        match pt {
            0 => result.codecs.push(CODEC_PCMU),
            9 => result.codecs.push(CODEC_G722),
            pt if pt >= 96 => {
                if let Some((name, clock_rate, _channels)) = rtpmap.get(&pt) {
                    if name.eq_ignore_ascii_case("telephone-event") {
                        let mut te = CODEC_TELEPHONE_EVENT;
                        te.pt = pt;
                        if *clock_rate > 0 {
                            te.clock_rate = *clock_rate;
                        }
                        result.codecs.push(te);
                    } else if name.eq_ignore_ascii_case("opus") && *clock_rate == 48000 {
                        let mut opus = CODEC_OPUS;
                        opus.pt = pt;
                        result.codecs.push(opus);
                    } else if name.eq_ignore_ascii_case("PCMU") && *clock_rate == 8000 {
                        let mut pcmu = CODEC_PCMU;
                        pcmu.pt = pt;
                        result.codecs.push(pcmu);
                    } else if name.eq_ignore_ascii_case("G722") && *clock_rate == 8000 {
                        let mut g722 = CODEC_G722;
                        g722.pt = pt;
                        result.codecs.push(g722);
                    }
                    // Unknown dynamic codecs with unrecognized name/rate are silently skipped
                }
                // Dynamic PT with no rtpmap entry: skip (can't determine codec)
            }
            _ => {}
        }
    }

    // Detect OSRTP (RFC 8643): RTP/AVP profile with a=crypto present.
    // The client offers plain RTP but includes SRTP keys opportunistically.
    // We should use SRTP if the keys are present.
    if result.crypto.is_some()
        && !result.is_webrtc
        && let Some(ref proto) = result.media_protocol
        && proto == "RTP/AVP"
    {
        result.is_osrtp = true;
    }

    result
}

/// Generate an SDP offer for a plain RTP endpoint
pub fn generate_sdp_offer(
    local_addr: SocketAddr,
    rtp_port: u16,
    codecs: &[&SdpCodec],
    crypto: Option<&SdpCrypto>,
    session_id: u64,
) -> String {
    generate_sdp(local_addr, rtp_port, codecs, crypto, session_id)
}

/// Generate an SDP answer for a plain RTP endpoint
pub fn generate_sdp_answer(
    local_addr: SocketAddr,
    rtp_port: u16,
    codecs: &[&SdpCodec],
    crypto: Option<&SdpCrypto>,
    session_id: u64,
) -> String {
    generate_sdp(local_addr, rtp_port, codecs, crypto, session_id)
}

fn generate_sdp(
    local_addr: SocketAddr,
    rtp_port: u16,
    codecs: &[&SdpCodec],
    crypto: Option<&SdpCrypto>,
    session_id: u64,
) -> String {
    let ip = local_addr.ip();
    let ip_ver = if ip.is_ipv4() { "IP4" } else { "IP6" };
    let proto = if crypto.is_some() {
        "RTP/SAVP"
    } else {
        "RTP/AVP"
    };

    // Collect all PTs including telephone-event
    let mut all_codecs: Vec<&SdpCodec> = codecs.to_vec();
    // Always add telephone-event if not already present
    if !all_codecs.iter().any(|c| c.name == "telephone-event") {
        all_codecs.push(&CODEC_TELEPHONE_EVENT);
    }

    let pt_list: String = all_codecs
        .iter()
        .map(|c| c.pt.to_string())
        .collect::<Vec<_>>()
        .join(" ");

    let mut sdp = String::new();
    sdp.push_str("v=0\r\n");
    sdp.push_str(&format!("o=rtpbridge {session_id} 1 IN {ip_ver} {ip}\r\n"));
    sdp.push_str("s=rtpbridge\r\n");
    sdp.push_str(&format!("c=IN {ip_ver} {ip}\r\n"));
    sdp.push_str("t=0 0\r\n");
    sdp.push_str(&format!("m=audio {rtp_port} {proto} {pt_list}\r\n"));

    // rtpmap for each codec
    for codec in &all_codecs {
        // Advertise each codec at its own clock. telephone-event therefore stays
        // at its narrowband 8000 in our offers (`CODEC_TELEPHONE_EVENT`) — the
        // SIP convention — and echoes the offered rate on answers. DTMF timing
        // is keyed off this negotiated telephone-event clock (tracked on the
        // endpoint and used by the DTMF path), NOT the audio codec clock, so
        // leading the offer with Opus no longer drags telephone-event to 48000.
        let rate = codec.clock_rate;
        if let Some(ch) = codec.channels {
            sdp.push_str(&format!(
                "a=rtpmap:{} {}/{}/{}\r\n",
                codec.pt, codec.name, rate, ch
            ));
        } else {
            sdp.push_str(&format!(
                "a=rtpmap:{} {}/{}\r\n",
                codec.pt, codec.name, rate
            ));
        }
        if let Some(fmtp) = codec.fmtp {
            sdp.push_str(&format!("a=fmtp:{} {}\r\n", codec.pt, fmtp));
        }
    }

    // Crypto
    if let Some(c) = crypto {
        sdp.push_str(&format!(
            "a=crypto:{} {} inline:{}\r\n",
            c.tag, c.suite, c.key_b64
        ));
    }

    sdp.push_str("a=sendrecv\r\n");
    sdp.push_str("a=rtcp-mux\r\n");
    sdp.push_str("a=ptime:20\r\n");

    sdp
}

#[cfg(test)]
#[path = "sdp_tests.rs"]
mod tests;
