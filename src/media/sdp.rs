use crate::media::codec::AudioCodec;
use std::net::{IpAddr, SocketAddr};

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

/// Select the single RFC 4733 mapping to negotiate for an audio codec.
///
/// Some SIP carriers offer multiple `telephone-event` payload types at
/// different clock rates. An RTP endpoint currently has one DTMF payload type
/// and clock, so prefer the mapping whose clock matches the selected audio
/// codec and otherwise retain the offerer's first mapping.
pub fn select_telephone_event_codec(
    codecs: &[SdpCodec],
    media_clock_rate: u32,
) -> Option<&SdpCodec> {
    let first = codecs.iter().find(|c| c.name == "telephone-event")?;
    codecs
        .iter()
        .find(|c| c.name == "telephone-event" && c.clock_rate == media_clock_rate)
        .or(Some(first))
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
    pub remote_rtcp_addr: Option<SocketAddr>,
    pub invalid_rtcp: bool,
    pub codecs: Vec<SdpCodec>,
    pub telephone_event_pt: Option<u8>,
    /// Negotiated telephone-event rtpmap clock (RFC 4733). `None` if no
    /// telephone-event was advertised; consumers default to 8000. Tracked
    /// independently of the media codec clock since DTMF event durations are
    /// expressed in this clock, not the audio codec's.
    pub telephone_event_clock_rate: Option<u32>,
    pub crypto: Option<SdpCrypto>,
    /// Crypto was advertised, including unsupported/malformed attributes.
    pub crypto_present: bool,
    /// Number of audio sections with nonzero ports in the remote SDP.
    pub audio_sections: usize,
    /// Media sections in offer order, including rejected and non-audio sections.
    pub media_sections: Vec<SdpMediaSection>,
    /// The audio section represented by this endpoint.
    pub selected_audio_section: Option<usize>,
    pub is_webrtc: bool,
    pub direction: Option<String>,
    pub rtcp_mux: bool,
    /// Media protocol from m= line (e.g., "RTP/AVP", "RTP/SAVP", "UDP/TLS/RTP/SAVPF")
    pub media_protocol: Option<String>,
    /// True if this is OSRTP: RTP/AVP profile with a=crypto present (RFC 8643)
    /// The endpoint should use SRTP if crypto is available, but the profile is "plain"
    pub is_osrtp: bool,
}

impl ParsedSdp {
    /// Validate an answer to an offer originated by this single-media endpoint.
    pub fn validate_plain_transport(&self) -> anyhow::Result<()> {
        if self.audio_sections != 1 {
            anyhow::bail!("SDP must contain exactly one active audio section");
        }
        self.validate_selected_plain_media()
    }

    /// Validate a remote offer before allocating or updating an endpoint. One
    /// audio section is selected; the others are rejected in the SDP answer.
    pub fn validate_plain_offer(&self) -> anyhow::Result<()> {
        if self.audio_sections == 0 {
            anyhow::bail!("SDP must contain an active audio section");
        }
        self.validate_selected_plain_media()
    }

    fn validate_selected_plain_media(&self) -> anyhow::Result<()> {
        if self.selected_audio_section.is_none()
            || self.media_sections.iter().any(|section| {
                section.media.is_empty()
                    || section.protocol.is_empty()
                    || section.formats.is_empty()
            })
        {
            anyhow::bail!("invalid SDP media section");
        }
        let secure = match self.media_protocol.as_deref() {
            Some("RTP/AVP" | "RTP/AVPF") => false,
            Some("RTP/SAVP" | "RTP/SAVPF") => true,
            _ => anyhow::bail!("unsupported plain RTP transport profile"),
        };
        if (secure || self.crypto_present) && self.crypto.is_none() {
            anyhow::bail!("SDP media security requires a supported valid SDES crypto attribute");
        }
        if let Some(crypto) = &self.crypto {
            crate::media::srtp::SrtpContext::from_sdes_key_with_lifetime(
                &crypto.key_b64,
                crypto
                    .key_lifetime_packets
                    .unwrap_or(crate::media::srtp::MAX_SRTP_PACKETS_PER_MASTER_KEY),
            )
            .map_err(|_| anyhow::anyhow!("SDP media security has invalid key material"))?;
        }
        if self.invalid_rtcp {
            anyhow::bail!("invalid SDP RTCP destination");
        }
        if self.remote_addr.is_none() {
            anyhow::bail!("SDP has no connection address");
        }
        if let (Some(rtp), Some(rtcp)) = (self.remote_addr, self.remote_rtcp_addr)
            && rtp.is_ipv6() != rtcp.is_ipv6()
        {
            anyhow::bail!("RTCP address family must match RTP");
        }
        Ok(())
    }
}

/// Fields needed to preserve each offered media section in an SDP answer.
#[derive(Debug, Clone)]
pub struct SdpMediaSection {
    pub media: String,
    pub protocol: String,
    pub formats: Vec<String>,
    active: bool,
    crypto_present: bool,
    crypto_supported: bool,
}

fn audio_section_preference(section: &SdpMediaSection) -> Option<u8> {
    if section.media != "audio" || !section.active {
        return None;
    }
    Some(match section.protocol.as_str() {
        "RTP/SAVP" | "RTP/SAVPF" if section.crypto_supported => 4,
        "RTP/AVP" | "RTP/AVPF" if section.crypto_supported => 3,
        "RTP/SAVP" | "RTP/SAVPF" => 2,
        "RTP/AVP" | "RTP/AVPF" if section.crypto_present => 2,
        "RTP/AVP" | "RTP/AVPF" => 1,
        _ => 0,
    })
}

/// SRTP SDES crypto attribute
#[derive(Debug, Clone)]
pub struct SdpCrypto {
    pub tag: u32,
    pub suite: String,
    pub key_b64: String,
    /// Optional RFC 4568 inline-key lifetime, measured as a maximum packet
    /// count for each SRTP and SRTCP stream using the master key.
    pub key_lifetime_packets: Option<u64>,
}

fn parse_sdes_lifetime(value: &str) -> Option<u64> {
    let lifetime = if let Some(exponent) = value.strip_prefix("2^") {
        if exponent.is_empty()
            || !exponent.bytes().all(|byte| byte.is_ascii_digit())
            || (exponent.len() > 1 && exponent.starts_with('0'))
        {
            return None;
        }
        let exponent = exponent.parse::<u32>().ok()?;
        1u64.checked_shl(exponent)?
    } else {
        if value.is_empty()
            || !value.bytes().all(|byte| byte.is_ascii_digit())
            || (value.len() > 1 && value.starts_with('0'))
        {
            return None;
        }
        value.parse::<u64>().ok()?
    };
    (1..=crate::media::srtp::MAX_SRTP_PACKETS_PER_MASTER_KEY)
        .contains(&lifetime)
        .then_some(lifetime)
}

/// Parse RFC 4568's supported subset of an inline SDES key. We support an
/// optional key lifetime and reject MKIs and session parameters, whose packet
/// framing and security semantics rtpbridge does not implement.
fn parse_sdes_inline_key(value: &str) -> Option<(String, Option<u64>)> {
    let value = value.strip_prefix("inline:")?;
    let mut fields = value.split('|');
    let key_b64 = fields.next()?;
    let lifetime = match (fields.next(), fields.next()) {
        (None, None) => None,
        (Some(lifetime), None) => Some(parse_sdes_lifetime(lifetime)?),
        _ => return None,
    };
    Some((key_b64.to_string(), lifetime))
}

fn parse_sdes_crypto(value: &str) -> Option<SdpCrypto> {
    let parts: Vec<_> = value.split_whitespace().collect();
    if parts.len() != 3 || parts[1] != "AES_CM_128_HMAC_SHA1_80" {
        return None;
    }
    let tag = parts[0].parse::<u32>().ok().filter(|tag| *tag > 0)?;
    let (key_b64, key_lifetime_packets) = parse_sdes_inline_key(parts[2])?;
    Some(SdpCrypto {
        tag,
        suite: parts[1].into(),
        key_b64,
        key_lifetime_packets,
    })
}

fn supported_sdes_key(crypto: &SdpCrypto) -> bool {
    crate::media::srtp::SrtpContext::from_sdes_key_with_lifetime(
        &crypto.key_b64,
        crypto
            .key_lifetime_packets
            .unwrap_or(crate::media::srtp::MAX_SRTP_PACKETS_PER_MASTER_KEY),
    )
    .is_ok()
}

/// Parse relevant fields from an SDP string
pub fn parse_sdp(sdp: &str) -> ParsedSdp {
    let mut result = ParsedSdp {
        remote_addr: None,
        remote_rtcp_addr: None,
        invalid_rtcp: false,
        codecs: Vec::new(),
        telephone_event_pt: None,
        telephone_event_clock_rate: None,
        crypto: None,
        crypto_present: false,
        audio_sections: 0,
        media_sections: Vec::new(),
        selected_audio_section: None,
        is_webrtc: false,
        direction: None,
        rtcp_mux: false,
        media_protocol: None,
        is_osrtp: false,
    };

    // Choose the section before parsing its attributes. A secure RTP profile
    // (or opportunistic SRTP with crypto) wins over plain RTP regardless of
    // offer order. Ties keep the offerer's first section.
    let mut current_section: Option<usize> = None;
    for line in sdp.lines().map(str::trim) {
        if let Some(rest) = line.strip_prefix("m=") {
            let parts: Vec<&str> = rest.split_whitespace().collect();
            let active = parts
                .get(1)
                .and_then(|port| port.parse::<u16>().ok())
                .is_some_and(|port| port > 0);
            result.media_sections.push(SdpMediaSection {
                media: parts.first().copied().unwrap_or_default().to_string(),
                protocol: parts.get(2).copied().unwrap_or_default().to_string(),
                formats: parts
                    .iter()
                    .skip(3)
                    .map(|part| (*part).to_string())
                    .collect(),
                active,
                crypto_present: false,
                crypto_supported: false,
            });
            current_section = Some(result.media_sections.len() - 1);
        } else if let Some(value) = line.strip_prefix("a=crypto:")
            && let Some(index) = current_section
        {
            let section = &mut result.media_sections[index];
            section.crypto_present = true;
            if let Some(crypto) = parse_sdes_crypto(value) {
                section.crypto_supported = supported_sdes_key(&crypto);
            }
        }
    }
    let mut best_preference = None;
    for (index, section) in result.media_sections.iter().enumerate() {
        if let Some(preference) = audio_section_preference(section) {
            result.audio_sections += 1;
            if best_preference.is_none_or(|best| preference > best) {
                result.selected_audio_section = Some(index);
                best_preference = Some(preference);
            }
        }
    }

    let mut session_c_addr: Option<std::net::IpAddr> = None;
    let mut audio_c_addr: Option<std::net::IpAddr> = None;
    let mut m_port: Option<u16> = None;
    let mut rtcp_port = None;
    let mut rtcp_ip = None;
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
    let mut current_section: Option<usize> = None;
    for line in sdp.lines() {
        let line = line.trim();

        if line.starts_with("m=") {
            current_section = Some(current_section.map_or(0, |index| index + 1));
        }

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
            let parts: Vec<&str> = rest.split_whitespace().collect();
            let active_port = parts
                .first()
                .and_then(|port| port.parse::<u16>().ok())
                .filter(|port| *port > 0);
            if active_port.is_none() {
                // RFC 3264 rejects a media stream with port zero. Do not let a
                // later rejected audio section overwrite the selected active one.
                media_section = Some(false);
                continue;
            }

            if current_section != result.selected_audio_section {
                // Only the preferred audio section contributes RTP state.
                // The other sections are rejected in the answer.
                media_section = Some(false);
                continue;
            }

            media_section = Some(true);
            m_port = active_port;
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
            }
        } else if let Some(rest) = line.strip_prefix("a=crypto:") {
            result.crypto_present = true;
            if let Some(crypto) = parse_sdes_crypto(rest) {
                result.crypto = Some(crypto);
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
        } else if let Some(rest) = line.strip_prefix("a=rtcp:") {
            let parts: Vec<_> = rest.split_whitespace().collect();
            rtcp_port = parts
                .first()
                .and_then(|port| port.parse::<u16>().ok())
                .filter(|port| *port > 0);
            rtcp_ip = parts.get(3).and_then(|ip| ip.parse::<IpAddr>().ok());
            let valid_address = matches!(
                (parts.get(1), parts.get(2), rtcp_ip),
                (Some(&"IN"), Some(&"IP4"), Some(IpAddr::V4(_)))
                    | (Some(&"IN"), Some(&"IP6"), Some(IpAddr::V6(_)))
            );
            if rtcp_port.is_none() || !(parts.len() == 1 || parts.len() == 4 && valid_address) {
                result.invalid_rtcp = true;
            }
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

    if let (Some(ip), Some(port)) = (rtcp_ip.or(c_addr), rtcp_port) {
        result.remote_rtcp_addr = Some(SocketAddr::new(ip, port));
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

    let selected_media_clock = select_answer_codec(&result.codecs)
        .map(|codec| codec.clock_rate)
        .unwrap_or(8000);
    if let Some(codec) = select_telephone_event_codec(&result.codecs, selected_media_clock) {
        result.telephone_event_pt = Some(codec.pt);
        result.telephone_event_clock_rate = Some(codec.clock_rate);
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
    generate_sdp(
        local_addr, rtp_port, codecs, crypto, session_id, false, None,
    )
}

/// Generate an opportunistic-SRTP offer (RFC 8643): advertise RTP/AVP while
/// including SDES keying material, allowing the answerer to select either
/// SRTP (by returning a crypto attribute) or plain RTP (by omitting it).
pub fn generate_osrtp_sdp_offer(
    local_addr: SocketAddr,
    rtp_port: u16,
    codecs: &[&SdpCodec],
    crypto: &SdpCrypto,
    session_id: u64,
) -> String {
    generate_sdp(
        local_addr,
        rtp_port,
        codecs,
        Some(crypto),
        session_id,
        true,
        None,
    )
}

/// Generate an SDP answer for a plain RTP endpoint
#[cfg(test)]
pub fn generate_sdp_answer(
    local_addr: SocketAddr,
    rtp_port: u16,
    codecs: &[&SdpCodec],
    crypto: Option<&SdpCrypto>,
    session_id: u64,
) -> String {
    generate_sdp(
        local_addr, rtp_port, codecs, crypto, session_id, false, None,
    )
}

/// Answer every offered media section in its original position. The selected
/// audio section uses this endpoint's port; all other sections use port zero.
pub fn generate_sdp_answer_for_offer(
    local_addr: SocketAddr,
    rtp_port: u16,
    codecs: &[&SdpCodec],
    crypto: Option<&SdpCrypto>,
    session_id: u64,
    offer: &ParsedSdp,
) -> anyhow::Result<String> {
    let selected = offer
        .selected_audio_section
        .ok_or_else(|| anyhow::anyhow!("SDP has no selected audio section"))?;
    if !matches!(offer.media_sections.get(selected), Some(section) if section.media == "audio") {
        anyhow::bail!("SDP selected media section is not audio");
    }
    Ok(generate_sdp(
        local_addr,
        rtp_port,
        codecs,
        crypto,
        session_id,
        false,
        Some((&offer.media_sections, selected)),
    ))
}

fn generate_sdp(
    local_addr: SocketAddr,
    rtp_port: u16,
    codecs: &[&SdpCodec],
    crypto: Option<&SdpCrypto>,
    session_id: u64,
    use_rtp_avp_profile: bool,
    answer_sections: Option<(&[SdpMediaSection], usize)>,
) -> String {
    let ip = local_addr.ip();
    let ip_ver = if ip.is_ipv4() { "IP4" } else { "IP6" };
    let proto = match answer_sections {
        Some((sections, selected)) => sections[selected].protocol.as_str(),
        None if crypto.is_some() && !use_rtp_avp_profile => "RTP/SAVP",
        None => "RTP/AVP",
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
    let mut media_sdp = format!("m=audio {rtp_port} {proto} {pt_list}\r\n");

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
            media_sdp.push_str(&format!(
                "a=rtpmap:{} {}/{}/{}\r\n",
                codec.pt, codec.name, rate, ch
            ));
        } else {
            media_sdp.push_str(&format!(
                "a=rtpmap:{} {}/{}\r\n",
                codec.pt, codec.name, rate
            ));
        }
        if let Some(fmtp) = codec.fmtp {
            media_sdp.push_str(&format!("a=fmtp:{} {}\r\n", codec.pt, fmtp));
        }
    }

    // Crypto
    if let Some(c) = crypto {
        let lifetime = c
            .key_lifetime_packets
            .map(|packets| format!("|{packets}"))
            .unwrap_or_default();
        media_sdp.push_str(&format!(
            "a=crypto:{} {} inline:{}{}\r\n",
            c.tag, c.suite, c.key_b64, lifetime
        ));
    }

    media_sdp.push_str("a=sendrecv\r\n");
    media_sdp.push_str("a=rtcp-mux\r\n");
    media_sdp.push_str("a=ptime:20\r\n");

    if let Some((sections, selected)) = answer_sections {
        for (index, section) in sections.iter().enumerate() {
            if index == selected {
                sdp.push_str(&media_sdp);
            } else {
                sdp.push_str(&format!(
                    "m={} 0 {} {}\r\n",
                    section.media,
                    section.protocol,
                    section.formats.join(" ")
                ));
            }
        }
    } else {
        sdp.push_str(&media_sdp);
    }

    sdp
}

#[cfg(test)]
#[path = "sdp_tests.rs"]
mod tests;
