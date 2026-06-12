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
                if let Some(c) = known.iter().find(|c| name.eq_ignore_ascii_case(c.name)) {
                    if !ordered.iter().any(|e| e.pt == c.pt) {
                        ordered.push(c.clone());
                    }
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
mod tests {
    use super::*;

    #[test]
    fn test_generate_and_parse_sdp() {
        let addr: SocketAddr = "192.168.1.1:5060".parse().unwrap();
        let codecs = vec![&CODEC_PCMU, &CODEC_G722];
        let sdp = generate_sdp_offer(addr, 30000, &codecs, None, 12345);

        assert!(sdp.contains("m=audio 30000 RTP/AVP"));
        assert!(sdp.contains("a=rtpmap:0 PCMU/8000"));
        assert!(sdp.contains("a=rtpmap:9 G722/8000"));
        assert!(sdp.contains("a=rtpmap:101 telephone-event/8000"));
        assert!(sdp.contains("a=rtcp-mux"));

        let parsed = parse_sdp(&sdp);
        assert!(!parsed.is_webrtc);
        assert!(parsed.telephone_event_pt.is_some());
        assert_eq!(parsed.remote_addr.unwrap().port(), 30000);
    }

    #[test]
    fn test_select_answer_codec_prefers_highest_quality() {
        // Offer lists narrowband first; we should still pick Opus.
        let codecs = vec![CODEC_PCMU, CODEC_G722, CODEC_OPUS];
        assert_eq!(select_answer_codec(&codecs).unwrap().name, "opus");

        // Without Opus, G722 wins over PCMU regardless of order.
        let codecs = vec![CODEC_PCMU, CODEC_G722];
        assert_eq!(select_answer_codec(&codecs).unwrap().name, "G722");
        let codecs = vec![CODEC_G722, CODEC_PCMU];
        assert_eq!(select_answer_codec(&codecs).unwrap().name, "G722");

        // telephone-event is never selected as the media codec.
        let codecs = vec![CODEC_TELEPHONE_EVENT, CODEC_PCMU];
        assert_eq!(select_answer_codec(&codecs).unwrap().name, "PCMU");

        // Only telephone-event (or empty) yields no media codec.
        assert!(select_answer_codec(&[CODEC_TELEPHONE_EVENT]).is_none());
        assert!(select_answer_codec(&[]).is_none());
    }

    #[test]
    fn test_offer_codec_list_is_quality_ordered() {
        // Default offer advertises Opus first, then G.722, then PCMU, with
        // telephone-event last — never PCMU-first.
        let names: Vec<&str> = offer_codec_list(None).iter().map(|c| c.name).collect();
        assert_eq!(names, vec!["opus", "G722", "PCMU", "telephone-event"]);

        // The list is non-increasing in codec_quality (regression guard against
        // a future PCMU-first reorder).
        let list = offer_codec_list(None);
        for pair in list.windows(2) {
            assert!(
                codec_quality(&pair[0]) >= codec_quality(&pair[1]),
                "offer codec order must be quality-descending: {} before {}",
                pair[0].name,
                pair[1].name
            );
        }
    }

    #[test]
    fn test_offer_codec_list_honors_caller_preference() {
        // A caller-specified order is advertised verbatim (RFC 3264 "preferred
        // codec order"), matched case-insensitively, with telephone-event last —
        // the quality default does NOT override an explicit preference.
        let prefer = vec!["pcmu".to_string(), "opus".to_string()];
        let names: Vec<&str> = offer_codec_list(Some(&prefer))
            .iter()
            .map(|c| c.name)
            .collect();
        assert_eq!(names, vec!["PCMU", "opus", "telephone-event"]);

        // Reversing the preference flips the advertised order.
        let prefer = vec!["opus".to_string(), "pcmu".to_string()];
        let names: Vec<&str> = offer_codec_list(Some(&prefer))
            .iter()
            .map(|c| c.name)
            .collect();
        assert_eq!(names, vec!["opus", "PCMU", "telephone-event"]);

        // Unknown and duplicate names are skipped; a single codec keeps DTMF.
        let prefer = vec!["g729".to_string(), "PCMU".to_string(), "pcmu".to_string()];
        let names: Vec<&str> = offer_codec_list(Some(&prefer))
            .iter()
            .map(|c| c.name)
            .collect();
        assert_eq!(names, vec!["PCMU", "telephone-event"]);
    }

    #[test]
    fn test_generated_offer_lists_opus_first() {
        // End-to-end: the m= line advertises Opus's PT (111) ahead of the
        // narrowband PTs so the answerer's first-match selection picks Opus.
        let addr: SocketAddr = "192.168.1.1:5060".parse().unwrap();
        let list = offer_codec_list(None);
        let refs: Vec<&SdpCodec> = list.iter().collect();
        let sdp = generate_sdp_offer(addr, 30000, &refs, None, 12345);

        let m_line = sdp
            .lines()
            .find(|l| l.starts_with("m=audio"))
            .expect("m=audio line present");
        assert_eq!(m_line, "m=audio 30000 RTP/AVP 111 9 0 101");

        // Opus leads (48 kHz) but telephone-event stays at the SIP-friendly
        // 8 kHz; DTMF timing keys off the negotiated telephone-event clock, not
        // the audio codec, so this no longer breaks DTMF for Opus.
        assert!(sdp.contains("a=rtpmap:111 opus/48000/2"));
        assert!(
            sdp.contains("a=rtpmap:101 telephone-event/8000"),
            "telephone-event must be advertised at 8000 even when Opus leads:\n{sdp}"
        );
        assert!(!sdp.contains("telephone-event/48000"));
    }

    #[test]
    fn test_parse_telephone_event_clock_rate() {
        let sdp8 = "v=0\r\no=- 1 1 IN IP4 1.2.3.4\r\ns=-\r\nc=IN IP4 1.2.3.4\r\n\
            t=0 0\r\nm=audio 5000 RTP/AVP 0 101\r\na=rtpmap:0 PCMU/8000\r\n\
            a=rtpmap:101 telephone-event/8000\r\n";
        let parsed = parse_sdp(sdp8);
        assert_eq!(parsed.telephone_event_pt, Some(101));
        assert_eq!(parsed.telephone_event_clock_rate, Some(8000));

        // A WebRTC-style offer advertises telephone-event at 48000.
        let sdp48 = "v=0\r\no=- 1 1 IN IP4 1.2.3.4\r\ns=-\r\nc=IN IP4 1.2.3.4\r\n\
            t=0 0\r\nm=audio 5000 RTP/AVP 111 101\r\na=rtpmap:111 opus/48000/2\r\n\
            a=rtpmap:101 telephone-event/48000\r\n";
        let parsed = parse_sdp(sdp48);
        assert_eq!(parsed.telephone_event_clock_rate, Some(48000));

        // No telephone-event advertised → None (consumers default to 8000).
        let sdp_none = "v=0\r\no=- 1 1 IN IP4 1.2.3.4\r\ns=-\r\nc=IN IP4 1.2.3.4\r\n\
            t=0 0\r\nm=audio 5000 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\n";
        assert_eq!(parse_sdp(sdp_none).telephone_event_clock_rate, None);
    }

    #[test]
    fn test_answer_echoes_offered_telephone_event_clock() {
        let addr: SocketAddr = "192.168.1.1:5060".parse().unwrap();

        // Answering with Opus media but a telephone-event offered at 8000 keeps
        // telephone-event at 8000 — the te clock is independent of the media
        // codec (proves Approach B, not a media-coupled rate).
        let codecs = vec![&CODEC_OPUS, &CODEC_TELEPHONE_EVENT];
        let ans = generate_sdp_answer(addr, 30000, &codecs, None, 1);
        assert!(ans.contains("opus/48000"));
        assert!(ans.contains("a=rtpmap:101 telephone-event/8000"));
        assert!(!ans.contains("telephone-event/48000"));

        // When the remote actually offered telephone-event/48000, the answer
        // echoes 48000 — proving this is B (echo negotiated), not hard-coded 8000.
        let mut te48 = CODEC_TELEPHONE_EVENT;
        te48.clock_rate = 48000;
        let codecs = vec![&CODEC_OPUS, &te48];
        let ans = generate_sdp_answer(addr, 30000, &codecs, None, 1);
        assert!(ans.contains("a=rtpmap:101 telephone-event/48000"));
    }

    #[test]
    fn test_parse_srtp_sdp() {
        let sdp = "v=0\r\n\
            o=- 123 1 IN IP4 10.0.0.1\r\n\
            s=-\r\n\
            c=IN IP4 10.0.0.1\r\n\
            t=0 0\r\n\
            m=audio 20000 RTP/SAVP 0 101\r\n\
            a=rtpmap:0 PCMU/8000\r\n\
            a=rtpmap:101 telephone-event/8000\r\n\
            a=fmtp:101 0-16\r\n\
            a=crypto:1 AES_CM_128_HMAC_SHA1_80 inline:dGVzdGtleQ==\r\n\
            a=sendrecv\r\n";

        let parsed = parse_sdp(sdp);
        assert!(!parsed.is_webrtc);
        assert!(parsed.crypto.is_some());
        let crypto = parsed.crypto.unwrap();
        assert_eq!(crypto.suite, "AES_CM_128_HMAC_SHA1_80");
        assert_eq!(crypto.key_b64, "dGVzdGtleQ==");
        assert_eq!(parsed.telephone_event_pt, Some(101));
    }

    #[test]
    fn test_detect_webrtc() {
        let sdp = "v=0\r\n\
            o=- 123 1 IN IP4 10.0.0.1\r\n\
            s=-\r\n\
            c=IN IP4 10.0.0.1\r\n\
            t=0 0\r\n\
            m=audio 9 UDP/TLS/RTP/SAVPF 111\r\n\
            a=ice-ufrag:abc123\r\n\
            a=ice-pwd:xyz789\r\n\
            a=fingerprint:sha-256 AA:BB:CC\r\n";

        let parsed = parse_sdp(sdp);
        assert!(parsed.is_webrtc);
    }

    #[test]
    fn test_osrtp_detection() {
        // OSRTP (RFC 8643): RTP/AVP profile with a=crypto present
        let sdp = "v=0\r\n\
            o=- 123 1 IN IP4 10.0.0.1\r\n\
            s=-\r\n\
            c=IN IP4 10.0.0.1\r\n\
            t=0 0\r\n\
            m=audio 20000 RTP/AVP 0 101\r\n\
            a=rtpmap:0 PCMU/8000\r\n\
            a=rtpmap:101 telephone-event/8000\r\n\
            a=crypto:1 AES_CM_128_HMAC_SHA1_80 inline:dGVzdGtleQ==\r\n\
            a=sendrecv\r\n";

        let parsed = parse_sdp(sdp);
        assert!(!parsed.is_webrtc);
        assert!(parsed.is_osrtp, "should detect OSRTP");
        assert!(parsed.crypto.is_some());
        assert_eq!(parsed.media_protocol.as_deref(), Some("RTP/AVP"));
    }

    #[test]
    fn test_savp_is_not_osrtp() {
        // RTP/SAVP with crypto is standard SRTP, not OSRTP
        let sdp = "v=0\r\n\
            o=- 123 1 IN IP4 10.0.0.1\r\n\
            s=-\r\n\
            c=IN IP4 10.0.0.1\r\n\
            t=0 0\r\n\
            m=audio 20000 RTP/SAVP 0 101\r\n\
            a=rtpmap:0 PCMU/8000\r\n\
            a=crypto:1 AES_CM_128_HMAC_SHA1_80 inline:dGVzdGtleQ==\r\n\
            a=sendrecv\r\n";

        let parsed = parse_sdp(sdp);
        assert!(!parsed.is_osrtp, "RTP/SAVP is standard SRTP, not OSRTP");
        assert!(parsed.crypto.is_some());
        assert_eq!(parsed.media_protocol.as_deref(), Some("RTP/SAVP"));
    }

    #[test]
    fn test_plain_rtp_no_crypto() {
        // Plain RTP/AVP without crypto
        let sdp = "v=0\r\n\
            o=- 123 1 IN IP4 10.0.0.1\r\n\
            s=-\r\n\
            c=IN IP4 10.0.0.1\r\n\
            t=0 0\r\n\
            m=audio 20000 RTP/AVP 0\r\n\
            a=rtpmap:0 PCMU/8000\r\n\
            a=sendrecv\r\n";

        let parsed = parse_sdp(sdp);
        assert!(!parsed.is_osrtp);
        assert!(parsed.crypto.is_none());
        assert_eq!(parsed.media_protocol.as_deref(), Some("RTP/AVP"));
    }

    #[test]
    fn test_parse_empty_sdp() {
        let parsed = parse_sdp("");
        assert!(
            parsed.codecs.is_empty(),
            "empty SDP should produce no codecs"
        );
        assert!(
            parsed.remote_addr.is_none(),
            "empty SDP should have no remote address"
        );
        assert!(!parsed.is_webrtc, "empty SDP should not be WebRTC");
        assert!(parsed.crypto.is_none(), "empty SDP should have no crypto");
        assert!(
            parsed.media_protocol.is_none(),
            "empty SDP should have no media protocol"
        );
        assert!(
            parsed.direction.is_none(),
            "empty SDP should have no direction"
        );
    }

    #[test]
    fn test_parse_sdp_no_media_line() {
        // Valid SDP session-level headers but no m= line
        let sdp = "v=0\r\no=- 0 0 IN IP4 0.0.0.0\r\ns=-\r\n";
        let parsed = parse_sdp(sdp);
        assert!(
            parsed.codecs.is_empty(),
            "SDP with no m= line should produce no codecs"
        );
        assert!(
            parsed.remote_addr.is_none(),
            "SDP with no m= line should have no remote address (no port)"
        );
        assert!(
            parsed.media_protocol.is_none(),
            "SDP with no m= line should have no media protocol"
        );
    }

    #[test]
    fn test_malformed_crypto_missing_key() {
        // a=crypto line with only tag and suite, no key material
        let sdp = "v=0\r\n\
            o=- 123 1 IN IP4 10.0.0.1\r\n\
            s=-\r\n\
            c=IN IP4 10.0.0.1\r\n\
            t=0 0\r\n\
            m=audio 20000 RTP/SAVP 0\r\n\
            a=rtpmap:0 PCMU/8000\r\n\
            a=crypto:1 AES_CM_128_HMAC_SHA1_80\r\n\
            a=sendrecv\r\n";

        let parsed = parse_sdp(sdp);
        // The parser requires 3 parts in the crypto line; with only 2 parts,
        // crypto should be None (silently skipped).
        assert!(
            parsed.crypto.is_none(),
            "malformed crypto line missing key should be rejected"
        );
    }

    #[test]
    fn test_malformed_crypto_bad_base64_key() {
        // a=crypto line with invalid base64 in the key — parser stores it raw,
        // but the SRTP context should reject it later. Here we just verify
        // the SDP parser doesn't panic.
        let sdp = "v=0\r\n\
            o=- 123 1 IN IP4 10.0.0.1\r\n\
            s=-\r\n\
            c=IN IP4 10.0.0.1\r\n\
            t=0 0\r\n\
            m=audio 20000 RTP/SAVP 0\r\n\
            a=rtpmap:0 PCMU/8000\r\n\
            a=crypto:1 AES_CM_128_HMAC_SHA1_80 inline:!!!NOT-BASE64!!!\r\n\
            a=sendrecv\r\n";

        let parsed = parse_sdp(sdp);
        // The SDP parser should parse the line without panicking; the bad
        // base64 is stored and will fail at SRTP context creation time.
        assert!(parsed.crypto.is_some(), "crypto line should be parsed");
        let crypto = parsed.crypto.unwrap();
        assert_eq!(crypto.suite, "AES_CM_128_HMAC_SHA1_80");
        assert_eq!(crypto.key_b64, "!!!NOT-BASE64!!!");
    }

    #[test]
    fn test_unsupported_crypto_suite_rejected() {
        // Unknown cipher suite — only AES_CM_128_HMAC_SHA1_80 is accepted
        let sdp = "v=0\r\n\
            o=- 123 1 IN IP4 10.0.0.1\r\n\
            s=-\r\n\
            c=IN IP4 10.0.0.1\r\n\
            t=0 0\r\n\
            m=audio 20000 RTP/SAVP 0\r\n\
            a=rtpmap:0 PCMU/8000\r\n\
            a=crypto:1 FAKE_SUITE_256 inline:dGVzdGtleQ==\r\n\
            a=sendrecv\r\n";

        let parsed = parse_sdp(sdp);
        assert!(
            parsed.crypto.is_none(),
            "unsupported cipher suite should be rejected"
        );
    }

    #[test]
    fn test_supported_suite_preferred_over_unsupported() {
        // Multiple crypto lines: unsupported first, supported second
        let sdp = "v=0\r\n\
            o=- 123 1 IN IP4 10.0.0.1\r\n\
            s=-\r\n\
            c=IN IP4 10.0.0.1\r\n\
            t=0 0\r\n\
            m=audio 20000 RTP/SAVP 0\r\n\
            a=rtpmap:0 PCMU/8000\r\n\
            a=crypto:1 AES_256_CM_HMAC_SHA1_80 inline:YmFka2V5\r\n\
            a=crypto:2 AES_CM_128_HMAC_SHA1_80 inline:Z29vZGtleQ==\r\n\
            a=sendrecv\r\n";

        let parsed = parse_sdp(sdp);
        assert!(parsed.crypto.is_some(), "should accept the supported suite");
        let crypto = parsed.crypto.unwrap();
        assert_eq!(crypto.suite, "AES_CM_128_HMAC_SHA1_80");
        assert_eq!(crypto.key_b64, "Z29vZGtleQ==");
        assert_eq!(crypto.tag, 2);
    }

    #[test]
    fn test_malformed_crypto_empty_line() {
        // Completely empty a=crypto: value
        let sdp = "v=0\r\n\
            o=- 123 1 IN IP4 10.0.0.1\r\n\
            s=-\r\n\
            c=IN IP4 10.0.0.1\r\n\
            t=0 0\r\n\
            m=audio 20000 RTP/SAVP 0\r\n\
            a=rtpmap:0 PCMU/8000\r\n\
            a=crypto:\r\n\
            a=sendrecv\r\n";

        let parsed = parse_sdp(sdp);
        assert!(
            parsed.crypto.is_none(),
            "empty crypto line should be rejected"
        );
    }

    #[test]
    fn test_parse_sdp_with_ipv6() {
        let sdp = "v=0\r\n\
            o=- 123 1 IN IP6 ::1\r\n\
            s=-\r\n\
            c=IN IP6 ::1\r\n\
            t=0 0\r\n\
            m=audio 30000 RTP/AVP 0\r\n\
            a=rtpmap:0 PCMU/8000\r\n\
            a=sendrecv\r\n";

        let parsed = parse_sdp(sdp);
        assert!(
            parsed.remote_addr.is_some(),
            "SDP with IPv6 should parse remote address"
        );
        let addr = parsed.remote_addr.unwrap();
        assert!(addr.ip().is_ipv6(), "parsed address should be IPv6");
        assert_eq!(addr.ip().to_string(), "::1", "IPv6 address should be ::1");
        assert_eq!(addr.port(), 30000, "port should be 30000");
        assert_eq!(parsed.codecs.len(), 1, "should have 1 codec");
        assert_eq!(parsed.codecs[0].name, "PCMU", "codec should be PCMU");
    }

    #[test]
    fn test_parse_sdp_multiple_media_lines() {
        // SDP with audio + video m= lines — we should only parse audio
        let sdp = "v=0\r\n\
            o=- 200 1 IN IP4 192.168.1.1\r\n\
            s=-\r\n\
            c=IN IP4 192.168.1.1\r\n\
            t=0 0\r\n\
            m=audio 40000 RTP/AVP 0 101\r\n\
            a=rtpmap:0 PCMU/8000\r\n\
            a=rtpmap:101 telephone-event/8000\r\n\
            a=sendrecv\r\n\
            m=video 40002 RTP/AVP 96\r\n\
            a=rtpmap:96 VP8/90000\r\n\
            a=sendrecv\r\n";

        let parsed = parse_sdp(sdp);
        // Should parse the first audio m= line
        assert!(parsed.remote_addr.is_some());
        assert_eq!(parsed.remote_addr.unwrap().port(), 40000);
        // Should include PCMU from the audio line
        assert!(
            parsed.codecs.iter().any(|c| c.name == "PCMU"),
            "should find PCMU codec from audio m= line"
        );
        assert_eq!(parsed.telephone_event_pt, Some(101));
    }

    #[test]
    fn test_parse_sdp_per_media_connection_lines() {
        // Audio and video have different c= addresses — audio c= must win for remote_addr
        let sdp = "v=0\r\n\
            o=- 400 1 IN IP4 10.0.0.1\r\n\
            s=-\r\n\
            c=IN IP4 10.0.0.99\r\n\
            t=0 0\r\n\
            m=audio 30000 RTP/AVP 0\r\n\
            c=IN IP4 10.0.0.1\r\n\
            a=rtpmap:0 PCMU/8000\r\n\
            a=sendrecv\r\n\
            m=video 30002 RTP/AVP 96\r\n\
            c=IN IP4 10.0.0.2\r\n\
            a=rtpmap:96 VP8/90000\r\n\
            a=sendrecv\r\n";

        let parsed = parse_sdp(sdp);
        let addr = parsed.remote_addr.expect("should have remote addr");
        assert_eq!(
            addr.ip().to_string(),
            "10.0.0.1",
            "audio media-level c= should override session c= and not be overwritten by video c="
        );
        assert_eq!(addr.port(), 30000, "port should come from audio m= line");
    }

    #[test]
    fn test_parse_sdp_session_c_used_when_no_audio_c() {
        // No media-level c= on audio — should fall back to session-level
        let sdp = "v=0\r\n\
            o=- 500 1 IN IP4 10.0.0.1\r\n\
            s=-\r\n\
            c=IN IP4 192.168.1.100\r\n\
            t=0 0\r\n\
            m=audio 25000 RTP/AVP 0\r\n\
            a=rtpmap:0 PCMU/8000\r\n\
            m=video 25002 RTP/AVP 96\r\n\
            c=IN IP4 10.0.0.5\r\n\
            a=rtpmap:96 VP8/90000\r\n";

        let parsed = parse_sdp(sdp);
        let addr = parsed.remote_addr.expect("should have remote addr");
        assert_eq!(
            addr.ip().to_string(),
            "192.168.1.100",
            "should use session-level c= when audio has no media-level c="
        );
        assert_eq!(addr.port(), 25000);
    }

    #[test]
    fn test_parse_sdp_duplicate_codec_definitions() {
        // SDP that lists the same PT twice in rtpmap — last one wins
        let sdp = "v=0\r\n\
            o=- 300 1 IN IP4 10.0.0.1\r\n\
            s=-\r\n\
            c=IN IP4 10.0.0.1\r\n\
            t=0 0\r\n\
            m=audio 50000 RTP/AVP 0 9\r\n\
            a=rtpmap:0 PCMU/8000\r\n\
            a=rtpmap:9 G722/8000\r\n\
            a=sendrecv\r\n";

        let parsed = parse_sdp(sdp);
        assert_eq!(parsed.codecs.len(), 2);
        assert_eq!(parsed.codecs[0].name, "PCMU");
        assert_eq!(parsed.codecs[1].name, "G722");
    }

    #[test]
    fn test_parse_sdp_two_audio_m_lines() {
        // Two m=audio lines — codecs from both should be merged
        let sdp = "v=0\r\n\
            o=- 600 1 IN IP4 10.0.0.1\r\n\
            s=-\r\n\
            c=IN IP4 10.0.0.1\r\n\
            t=0 0\r\n\
            m=audio 30000 RTP/AVP 0\r\n\
            a=rtpmap:0 PCMU/8000\r\n\
            a=sendrecv\r\n\
            m=audio 30002 RTP/AVP 9\r\n\
            a=rtpmap:9 G722/8000\r\n\
            a=sendrecv\r\n";

        let parsed = parse_sdp(sdp);
        // Both codecs should be present (merged from both m= lines)
        let has_pcmu = parsed.codecs.iter().any(|c| c.name == "PCMU");
        let has_g722 = parsed.codecs.iter().any(|c| c.name == "G722");
        assert!(has_pcmu, "should have PCMU from first m=audio line");
        assert!(has_g722, "should have G722 from second m=audio line");
    }

    #[test]
    fn test_parse_sdp_unsupported_codecs_only() {
        // SDP offering only unsupported codecs (G.729 PT 18, PCMA PT 8)
        let sdp = "v=0\r\n\
            o=- 700 1 IN IP4 10.0.0.1\r\n\
            s=-\r\n\
            c=IN IP4 10.0.0.1\r\n\
            t=0 0\r\n\
            m=audio 30000 RTP/AVP 8 18\r\n\
            a=rtpmap:8 PCMA/8000\r\n\
            a=rtpmap:18 G729/8000\r\n\
            a=sendrecv\r\n";

        let parsed = parse_sdp(sdp);
        assert!(
            parsed.codecs.is_empty(),
            "SDP with only unsupported codecs should produce empty codec list, got {:?}",
            parsed.codecs.iter().map(|c| &c.name).collect::<Vec<_>>()
        );
        // remote_addr should still be parsed (SDP is structurally valid)
        assert!(
            parsed.remote_addr.is_some(),
            "remote address should still be parsed"
        );
    }

    #[test]
    fn test_parse_sdp_duplicate_supported_crypto() {
        // Two a=crypto lines with the same supported suite — last one wins
        // (parser overwrites on each matching line)
        let sdp = "v=0\r\n\
            o=- 800 1 IN IP4 10.0.0.1\r\n\
            s=-\r\n\
            c=IN IP4 10.0.0.1\r\n\
            t=0 0\r\n\
            m=audio 20000 RTP/SAVP 0\r\n\
            a=rtpmap:0 PCMU/8000\r\n\
            a=crypto:1 AES_CM_128_HMAC_SHA1_80 inline:Zmlyc3RrZXk=\r\n\
            a=crypto:2 AES_CM_128_HMAC_SHA1_80 inline:c2Vjb25ka2V5\r\n\
            a=sendrecv\r\n";

        let parsed = parse_sdp(sdp);
        assert!(parsed.crypto.is_some(), "should accept a crypto line");
        let crypto = parsed.crypto.unwrap();
        assert_eq!(
            crypto.tag, 2,
            "last matching crypto line (tag 2) should be selected"
        );
        assert_eq!(crypto.key_b64, "c2Vjb25ka2V5");
    }

    #[test]
    fn test_parse_sdp_port_zero_rejected() {
        // m=audio 0 means media stream rejected (RFC 3264 §6)
        let sdp = "v=0\r\n\
            o=- 900 1 IN IP4 10.0.0.1\r\n\
            s=-\r\n\
            c=IN IP4 10.0.0.1\r\n\
            t=0 0\r\n\
            m=audio 0 RTP/AVP 0\r\n\
            a=rtpmap:0 PCMU/8000\r\n\
            a=sendrecv\r\n";

        let parsed = parse_sdp(sdp);
        assert!(
            parsed.remote_addr.is_none(),
            "port 0 should result in no remote_addr (stream rejected)"
        );
        // Codecs should still be parsed (the SDP is structurally valid)
        assert!(
            !parsed.codecs.is_empty(),
            "codecs should still be parsed even with port 0"
        );
    }
}
