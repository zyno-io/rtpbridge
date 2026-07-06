//! SRTP encrypt/decrypt for AES_CM_128_HMAC_SHA1_80 (RFC 3711).
//!
//! Supports SDES key exchange (a=crypto lines in SDP).

use std::collections::HashMap;

use aes::cipher::{KeyIvInit, StreamCipher};
use base64::Engine;
use hmac::{Hmac, KeyInit, Mac};
use sha1::Sha1;
use subtle::ConstantTimeEq;
use zeroize::Zeroize;

type Aes128Ctr = ctr::Ctr128BE<aes::Aes128>;
type HmacSha1 = Hmac<Sha1>;

const SRTP_AUTH_TAG_LEN: usize = 10; // 80 bits
const SRTP_MASTER_KEY_LEN: usize = 16; // 128 bits
const SRTP_MASTER_SALT_LEN: usize = 14; // 112 bits

/// Maximum distinct inbound SSRCs tracked per SRTP/SRTCP receive context. Bounds
/// memory against an authenticated peer spraying SSRCs; new SSRCs beyond this are
/// rejected — we never evict live replay state (RFC 3711 §3.3.2).
const MAX_RECV_SSRCS: usize = 64;

/// Per-SSRC SRTP rollover counter + replay window. RFC 3711 §3.2.1/§3.3 keeps
/// this state per SSRC; sharing it across SSRCs corrupts both when a peer
/// rotates SSRC mid-session (the new low sequence is rejected as "too old" or
/// authenticated against the wrong ROC).
#[derive(Default)]
struct SrtpStreamState {
    /// Rollover counter for extended sequence number
    roc: u32,
    /// Highest sequence number seen
    highest_seq: u16,
    seq_initialized: bool,
    /// Sliding replay window bitmap (64 packets behind highest_seq).
    /// Bit i is set if packet (highest_seq - i) has been seen; bit 0 = highest_seq.
    replay_window: u64,
}

impl SrtpStreamState {
    /// Update ROC for outbound (protect) — no replay check needed.
    fn update_roc(&mut self, seq: u16) {
        if !self.seq_initialized {
            self.highest_seq = seq;
            self.seq_initialized = true;
            self.replay_window = 1; // mark bit 0 (current seq)
            return;
        }

        // Simple ROC management (RFC 3711 appendix A)
        if seq < 0x8000 && self.highest_seq > 0x8000 {
            // Sequence number wrapped around — update highest_seq to prevent
            // re-triggering the wrap condition on the next packet.
            self.roc += 1;
            self.highest_seq = seq;
        } else if seq > self.highest_seq {
            self.highest_seq = seq;
        }
    }

    /// Estimate ROC for an incoming seq without mutating state (RFC 3711 §3.3.1).
    /// Returns (estimated_roc, extended_index).
    fn estimate_roc(&self, seq: u16) -> (u32, u64) {
        if !self.seq_initialized {
            return (0, seq as u64);
        }
        let roc = if self.highest_seq < 0x8000 {
            if seq > self.highest_seq.wrapping_add(0x8000) {
                // seq is far ahead — likely belongs to previous ROC
                self.roc.wrapping_sub(1)
            } else {
                self.roc
            }
        } else if seq < self.highest_seq.wrapping_sub(0x8000) {
            // seq wrapped around — next ROC
            self.roc.wrapping_add(1)
        } else {
            self.roc
        };
        let index = ((roc as u64) << 16) | seq as u64;
        (roc, index)
    }

    /// Check + update the replay window for an incoming packet (RFC 3711 §3.3.2).
    /// Returns Err if the packet is a replay. On success, updates ROC/seq/window.
    fn check_replay(&mut self, seq: u16) -> anyhow::Result<(u32, u64)> {
        if !self.seq_initialized {
            self.highest_seq = seq;
            self.seq_initialized = true;
            self.replay_window = 1;
            return Ok((0, seq as u64));
        }

        let (estimated_roc, index) = self.estimate_roc(seq);
        let highest_index = ((self.roc as u64) << 16) | self.highest_seq as u64;

        if index > highest_index {
            // New packet ahead of window — shift window
            let delta = index - highest_index;
            if delta < 64 {
                self.replay_window = (self.replay_window << delta) | 1;
            } else {
                self.replay_window = 1;
            }
            self.highest_seq = seq;
            self.roc = estimated_roc;
        } else {
            // Packet within or behind window
            let delta = highest_index - index;
            if delta >= 64 {
                anyhow::bail!("SRTP replay: packet too old (delta={delta})");
            }
            let bit = 1u64 << delta;
            if self.replay_window & bit != 0 {
                anyhow::bail!("SRTP replay: duplicate packet (seq={seq}, delta={delta})");
            }
            self.replay_window |= bit;
        }

        Ok((estimated_roc, index))
    }
}

/// SRTP session context for encrypt/decrypt
pub struct SrtpContext {
    /// Derived session encryption key (128 bits)
    pub(crate) cipher_key: [u8; 16],
    /// Derived session salt (112 bits)
    pub(crate) cipher_salt: [u8; 14],
    /// Derived session authentication key (160 bits)
    pub(crate) auth_key: [u8; 20],
    /// Per-SSRC rollover counter + replay window, keyed by RTP SSRC. A peer that
    /// rotates SSRC mid-session (hold/re-INVITE, failover, gateway) gets an
    /// independent context per RFC 3711 §3.2.1 instead of corrupting a single
    /// shared one. A receive context inserts an entry only after a packet
    /// authenticates (bounded by `MAX_RECV_SSRCS`); a transmit context holds one
    /// entry per local SSRC we send.
    streams: HashMap<u32, SrtpStreamState>,
}

impl SrtpContext {
    /// Create an SRTP context from a base64-encoded SDES key.
    /// The key material is: master_key (16 bytes) || master_salt (14 bytes) = 30 bytes.
    pub fn from_sdes_key(key_b64: &str) -> anyhow::Result<Self> {
        let mut key_material = base64_decode(key_b64)?;
        if key_material.len() < SRTP_MASTER_KEY_LEN + SRTP_MASTER_SALT_LEN {
            anyhow::bail!(
                "SRTP key material too short: {} bytes (need {})",
                key_material.len(),
                SRTP_MASTER_KEY_LEN + SRTP_MASTER_SALT_LEN
            );
        }

        let mut master_key: [u8; 16] = key_material[..16]
            .try_into()
            .map_err(|_| anyhow::anyhow!("SRTP master key slice conversion failed"))?;
        let mut master_salt: [u8; 14] = key_material[16..30]
            .try_into()
            .map_err(|_| anyhow::anyhow!("SRTP master salt slice conversion failed"))?;
        key_material.zeroize();

        // Derive session keys using SRTP KDF (RFC 3711 §4.3.1)
        let mut cipher_key = srtp_kdf(&master_key, &master_salt, 0x00, 16);
        let mut auth_key = srtp_kdf(&master_key, &master_salt, 0x01, 20);
        let mut cipher_salt_vec = srtp_kdf(&master_key, &master_salt, 0x02, 14);

        let mut ck = [0u8; 16];
        ck.copy_from_slice(&cipher_key);
        let mut cs = [0u8; 14];
        cs.copy_from_slice(&cipher_salt_vec);
        let mut ak = [0u8; 20];
        ak.copy_from_slice(&auth_key);

        // Zeroize intermediate key material
        cipher_key.zeroize();
        auth_key.zeroize();
        cipher_salt_vec.zeroize();
        master_key.zeroize();
        master_salt.zeroize();

        Ok(Self {
            cipher_key: ck,
            cipher_salt: cs,
            auth_key: ak,
            streams: HashMap::new(),
        })
    }

    /// Encrypt an RTP packet in-place, appending the auth tag.
    /// Returns the encrypted packet (header unchanged, payload encrypted, auth tag appended).
    pub fn protect(&mut self, rtp_packet: &[u8]) -> anyhow::Result<Vec<u8>> {
        if rtp_packet.len() < 12 {
            anyhow::bail!("RTP packet too short");
        }

        let ssrc =
            u32::from_be_bytes([rtp_packet[8], rtp_packet[9], rtp_packet[10], rtp_packet[11]]);
        let seq = u16::from_be_bytes([rtp_packet[2], rtp_packet[3]]);

        let st = self.streams.entry(ssrc).or_default();
        st.update_roc(seq);
        let roc = st.roc;
        let index = ((roc as u64) << 16) | seq as u64;

        // Find payload offset (skip fixed header + CSRC + header extension)
        let header_len = rtp_header_len(rtp_packet)
            .ok_or_else(|| anyhow::anyhow!("RTP packet too short for header"))?;

        let mut output = rtp_packet.to_vec();

        // Encrypt payload using AES-CM
        let iv = compute_iv(&self.cipher_salt, ssrc, index);
        let mut cipher = Aes128Ctr::new((&self.cipher_key).into(), (&iv).into());
        cipher.apply_keystream(&mut output[header_len..]);

        // Compute and append HMAC-SHA1 auth tag
        // Auth covers: RTP header + encrypted payload + ROC
        let mut mac = HmacSha1::new_from_slice(&self.auth_key)
            .map_err(|e| anyhow::anyhow!("HMAC init error: {e}"))?;
        mac.update(&output);
        mac.update(&roc.to_be_bytes());
        let tag = mac.finalize().into_bytes();
        output.extend_from_slice(&tag[..SRTP_AUTH_TAG_LEN]);

        Ok(output)
    }

    /// Reset the sequence/ROC/replay state, preserving the derived session keys.
    ///
    /// Use this when the remote peer restarts its RTP stream mid-session — e.g.,
    /// after a SIP hold where the phone sends RTCP BYE and resumes with a new
    /// SSRC + reset sequence number. Without this, the old `replay_window` /
    /// `highest_seq` would reject the peer's fresh low-seq packets as "too old"
    /// and decrypt would silently drop every packet until the sequence climbed
    /// back into range.
    pub fn reset_sequence_state(&mut self) {
        // Drop all per-SSRC state so any SSRC — the same one restarting with a
        // low sequence after a hold, or a fresh one — re-baselines on its next
        // packet. Called at a trusted SDP-renegotiation boundary; the derived
        // session keys are preserved.
        self.streams.clear();
    }

    /// Decrypt an SRTP packet, verifying the auth tag, checking for replay,
    /// and decrypting the payload.
    /// Returns the decrypted RTP packet (without auth tag).
    pub fn unprotect(&mut self, srtp_packet: &[u8]) -> anyhow::Result<Vec<u8>> {
        if srtp_packet.len() < 12 + SRTP_AUTH_TAG_LEN {
            anyhow::bail!("SRTP packet too short");
        }

        let auth_portion = &srtp_packet[..srtp_packet.len() - SRTP_AUTH_TAG_LEN];
        let received_tag = &srtp_packet[srtp_packet.len() - SRTP_AUTH_TAG_LEN..];

        let seq = u16::from_be_bytes([srtp_packet[2], srtp_packet[3]]);
        let ssrc = u32::from_be_bytes([
            srtp_packet[8],
            srtp_packet[9],
            srtp_packet[10],
            srtp_packet[11],
        ]);

        // Estimate ROC for the auth check from THIS SSRC's state (a new SSRC
        // starts at ROC 0). Computed without mutating or inserting state, so a
        // packet that fails auth can never spray the SSRC table.
        let estimated_roc = match self.streams.get(&ssrc) {
            Some(st) => st.estimate_roc(seq).0,
            None => 0,
        };

        // Verify HMAC-SHA1 auth tag (using estimated ROC)
        let mut mac = HmacSha1::new_from_slice(&self.auth_key)
            .map_err(|e| anyhow::anyhow!("HMAC init error: {e}"))?;
        mac.update(auth_portion);
        mac.update(&estimated_roc.to_be_bytes());
        let computed_tag = mac.finalize().into_bytes();

        if computed_tag[..SRTP_AUTH_TAG_LEN]
            .ct_eq(received_tag)
            .unwrap_u8()
            == 0
        {
            anyhow::bail!("SRTP auth tag mismatch (ssrc={ssrc:#x}, seq={seq})");
        }

        // Auth passed — commit replay state for this SSRC, creating the entry on
        // first sight. Bounded: reject a brand-new SSRC over the cap rather than
        // evicting a live stream's replay window.
        if !self.streams.contains_key(&ssrc) && self.streams.len() >= MAX_RECV_SSRCS {
            anyhow::bail!(
                "SRTP: too many distinct SSRCs ({}), rejecting {ssrc:#x}",
                self.streams.len()
            );
        }
        let (_roc, index) = self.streams.entry(ssrc).or_default().check_replay(seq)?;

        // Decrypt payload
        let header_len = rtp_header_len(auth_portion)
            .ok_or_else(|| anyhow::anyhow!("SRTP packet too short for header"))?;

        let mut output = auth_portion.to_vec();
        let iv = compute_iv(&self.cipher_salt, ssrc, index);
        let mut cipher = Aes128Ctr::new((&self.cipher_key).into(), (&iv).into());
        cipher.apply_keystream(&mut output[header_len..]);

        Ok(output)
    }
}

impl Drop for SrtpContext {
    fn drop(&mut self) {
        self.cipher_key.zeroize();
        self.cipher_salt.zeroize();
        self.auth_key.zeroize();
    }
}

/// Per-SSRC inbound SRTCP replay state, keyed by RTCP sender SSRC.
#[derive(Default)]
struct SrtcpRecvState {
    /// Highest SRTCP index seen (inbound replay protection)
    highest_recv_index: u32,
    recv_index_initialized: bool,
    /// Sliding replay window bitmap (64 indices behind highest_recv_index)
    replay_window: u64,
}

impl SrtcpRecvState {
    /// Check + update the replay window for an incoming SRTCP packet.
    /// Returns Err if the packet is a replay. On success, updates window state.
    fn check_replay(&mut self, index: u32) -> anyhow::Result<()> {
        if !self.recv_index_initialized {
            self.highest_recv_index = index;
            self.recv_index_initialized = true;
            self.replay_window = 1;
            return Ok(());
        }

        if index > self.highest_recv_index {
            let delta = index - self.highest_recv_index;
            if delta < 64 {
                self.replay_window = (self.replay_window << delta) | 1;
            } else {
                self.replay_window = 1;
            }
            self.highest_recv_index = index;
        } else {
            let delta = self.highest_recv_index - index;
            if delta >= 64 {
                anyhow::bail!("SRTCP replay: packet too old (index={index}, delta={delta})");
            }
            let bit = 1u64 << delta;
            if self.replay_window & bit != 0 {
                anyhow::bail!("SRTCP replay: duplicate packet (index={index})");
            }
            self.replay_window |= bit;
        }

        Ok(())
    }
}

/// SRTCP session context for encrypt/decrypt (RFC 3711 §3.4).
/// Uses the same master key as SRTP but derives separate session keys
/// with labels 0x03 (cipher), 0x04 (auth), 0x05 (salt).
pub struct SrtcpContext {
    pub(crate) cipher_key: [u8; 16],
    pub(crate) cipher_salt: [u8; 14],
    pub(crate) auth_key: [u8; 20],
    /// 31-bit SRTCP index counter (outbound). A single monotonic counter across
    /// any local SSRC rotation — intentionally not per-SSRC (see `protect_rtcp`
    /// and `rotate_outbound_ssrc`) so peers with a global SRTCP replay window
    /// aren't tripped by a reused low index.
    srtcp_index: u32,
    /// Per-SSRC inbound SRTCP replay state, keyed by RTCP sender SSRC. Inserted
    /// only after a packet authenticates (bounded by `MAX_RECV_SSRCS`).
    recv_streams: HashMap<u32, SrtcpRecvState>,
}

impl SrtcpContext {
    /// Create an SRTCP context from a base64-encoded SDES key (same key as SRTP).
    pub fn from_sdes_key(key_b64: &str) -> anyhow::Result<Self> {
        let mut key_material = base64_decode(key_b64)?;
        if key_material.len() < SRTP_MASTER_KEY_LEN + SRTP_MASTER_SALT_LEN {
            anyhow::bail!("SRTCP key material too short: {} bytes", key_material.len());
        }

        let mut master_key: [u8; 16] = key_material[..16]
            .try_into()
            .map_err(|_| anyhow::anyhow!("SRTCP master key slice conversion failed"))?;
        let mut master_salt: [u8; 14] = key_material[16..30]
            .try_into()
            .map_err(|_| anyhow::anyhow!("SRTCP master salt slice conversion failed"))?;
        key_material.zeroize();

        // SRTCP uses labels 0x03/0x04/0x05 (vs SRTP 0x00/0x01/0x02)
        let mut cipher_key = srtp_kdf(&master_key, &master_salt, 0x03, 16);
        let mut auth_key = srtp_kdf(&master_key, &master_salt, 0x04, 20);
        let mut cipher_salt_vec = srtp_kdf(&master_key, &master_salt, 0x05, 14);

        let mut ck = [0u8; 16];
        ck.copy_from_slice(&cipher_key);
        let mut cs = [0u8; 14];
        cs.copy_from_slice(&cipher_salt_vec);
        let mut ak = [0u8; 20];
        ak.copy_from_slice(&auth_key);

        // Zeroize intermediate key material
        cipher_key.zeroize();
        auth_key.zeroize();
        cipher_salt_vec.zeroize();
        master_key.zeroize();
        master_salt.zeroize();

        Ok(Self {
            cipher_key: ck,
            cipher_salt: cs,
            auth_key: ak,
            srtcp_index: 0,
            recv_streams: HashMap::new(),
        })
    }

    /// Encrypt an RTCP compound packet (RFC 3711 §3.4).
    /// Output: [header+SSRC(8 clear)] [encrypted payload] [E(1)+index(31)] [auth tag(10)]
    pub fn protect_rtcp(&mut self, rtcp_packet: &[u8]) -> anyhow::Result<Vec<u8>> {
        if rtcp_packet.len() < 8 {
            anyhow::bail!("RTCP packet too short");
        }

        // SSRC from first sub-packet header (bytes 4-7)
        let ssrc = u32::from_be_bytes([
            rtcp_packet[4],
            rtcp_packet[5],
            rtcp_packet[6],
            rtcp_packet[7],
        ]);

        let index = self.srtcp_index;
        self.srtcp_index = (self.srtcp_index + 1) & 0x7FFFFFFF;
        if self.srtcp_index == 0x70000000 {
            tracing::warn!("SRTCP index at 87.5% of keyspace — rekeying recommended");
        }

        let mut output = rtcp_packet.to_vec();

        // Encrypt everything after the first 8 bytes (header+SSRC stay in clear)
        if output.len() > 8 {
            let iv = compute_srtcp_iv(&self.cipher_salt, ssrc, index);
            let mut cipher = Aes128Ctr::new((&self.cipher_key).into(), (&iv).into());
            cipher.apply_keystream(&mut output[8..]);
        }

        // Append E-flag (1) + SRTCP index (31 bits)
        let e_and_index = 0x80000000u32 | index;
        output.extend_from_slice(&e_and_index.to_be_bytes());

        // Auth tag covers [encrypted RTCP + E+index]
        let mut mac = HmacSha1::new_from_slice(&self.auth_key)
            .map_err(|e| anyhow::anyhow!("HMAC init error: {e}"))?;
        mac.update(&output);
        let tag = mac.finalize().into_bytes();
        output.extend_from_slice(&tag[..SRTP_AUTH_TAG_LEN]);

        Ok(output)
    }

    /// Reset the inbound SRTCP replay state, preserving the derived session keys
    /// and the outbound `srtcp_index` counter.
    ///
    /// Use this when the remote peer restarts their RTCP stream mid-session — e.g.,
    /// after a SIP hold where the phone sends RTCP BYE and resumes with a fresh
    /// SRTCP index of 0. Without this, the old `replay_window` / `highest_recv_index`
    /// would reject the peer's restarted low-index packets as "too old".
    pub fn reset_recv_state(&mut self) {
        // Drop all per-SSRC inbound replay state so a restarted (same or new)
        // SRTCP source re-baselines; preserves keys and the outbound index.
        self.recv_streams.clear();
    }

    /// Decrypt an SRTCP packet, verifying auth and decrypting if E=1.
    pub fn unprotect_rtcp(&mut self, srtcp_packet: &[u8]) -> anyhow::Result<Vec<u8>> {
        // Minimum: 8 (header+SSRC) + 4 (E+index) + 10 (auth tag) = 22
        if srtcp_packet.len() < 22 {
            anyhow::bail!("SRTCP packet too short");
        }

        let auth_start = srtcp_packet.len() - SRTP_AUTH_TAG_LEN;
        let index_start = auth_start - 4;

        let received_tag = &srtcp_packet[auth_start..];
        let auth_portion = &srtcp_packet[..auth_start]; // includes E+index

        // Verify HMAC-SHA1 auth tag
        let mut mac = HmacSha1::new_from_slice(&self.auth_key)
            .map_err(|e| anyhow::anyhow!("HMAC init error: {e}"))?;
        mac.update(auth_portion);
        let computed_tag = mac.finalize().into_bytes();
        if computed_tag[..SRTP_AUTH_TAG_LEN]
            .ct_eq(received_tag)
            .unwrap_u8()
            == 0
        {
            anyhow::bail!("SRTCP auth tag mismatch");
        }

        // Extract E-flag and index
        let e_and_index = u32::from_be_bytes([
            srtcp_packet[index_start],
            srtcp_packet[index_start + 1],
            srtcp_packet[index_start + 2],
            srtcp_packet[index_start + 3],
        ]);
        let encrypted = (e_and_index & 0x80000000) != 0;
        let recv_index = e_and_index & 0x7FFFFFFF;

        // SRTCP replay state is keyed by the RTCP sender SSRC (bytes 4-7, always
        // in the clear) so a peer rotating SSRC isn't rejected against another
        // source's window. Auth has already passed above, so inserting here can't
        // be sprayed by unauthenticated traffic; still bounded to reject (not
        // evict) brand-new SSRCs over the cap.
        let ssrc = u32::from_be_bytes([
            srtcp_packet[4],
            srtcp_packet[5],
            srtcp_packet[6],
            srtcp_packet[7],
        ]);
        if !self.recv_streams.contains_key(&ssrc) && self.recv_streams.len() >= MAX_RECV_SSRCS {
            anyhow::bail!(
                "SRTCP: too many distinct SSRCs ({}), rejecting {ssrc:#x}",
                self.recv_streams.len()
            );
        }
        self.recv_streams
            .entry(ssrc)
            .or_default()
            .check_replay(recv_index)?;

        // Strip E+index and auth tag to get the RTCP compound packet
        let mut output = srtcp_packet[..index_start].to_vec();

        if encrypted && output.len() > 8 {
            let index = e_and_index & 0x7FFFFFFF;
            let iv = compute_srtcp_iv(&self.cipher_salt, ssrc, index);
            let mut cipher = Aes128Ctr::new((&self.cipher_key).into(), (&iv).into());
            cipher.apply_keystream(&mut output[8..]);
        }

        Ok(output)
    }
}

impl Drop for SrtcpContext {
    fn drop(&mut self) {
        self.cipher_key.zeroize();
        self.cipher_salt.zeroize();
        self.auth_key.zeroize();
    }
}

/// Compute IV for SRTCP (RFC 3711 §4.1.1, adapted for RTCP index)
///
/// IV = (k_s * 2^16) XOR (SSRC * 2^64) XOR (index * 2^16)
///
/// Same layout as SRTP IV: salt at bytes 0-13, SSRC at bytes 4-7,
/// 31-bit SRTCP index shifted left by 16 at bytes 8-13, bytes 14-15 = 0.
fn compute_srtcp_iv(salt: &[u8; 14], ssrc: u32, srtcp_index: u32) -> [u8; 16] {
    let mut iv = [0u8; 16];

    let ssrc_bytes = ssrc.to_be_bytes();
    // Shift 32-bit index left by 16 bits within u64, then place at bytes 8-15
    let shifted_index = ((srtcp_index as u64) << 16).to_be_bytes();

    // SSRC at bytes 4-7
    iv[4] = ssrc_bytes[0];
    iv[5] = ssrc_bytes[1];
    iv[6] = ssrc_bytes[2];
    iv[7] = ssrc_bytes[3];
    // Shifted index at bytes 8-15 (bytes 14-15 are zero from the shift)
    iv[8..16].copy_from_slice(&shifted_index);

    // XOR with salt (14 bytes at iv[0..14])
    for i in 0..14 {
        iv[i] ^= salt[i];
    }

    iv
}

/// SRTP Key Derivation Function (RFC 3711 §4.3.1)
/// Uses AES-CM as PRF to derive session keys from master key + salt.
fn srtp_kdf(master_key: &[u8; 16], master_salt: &[u8; 14], label: u8, out_len: usize) -> Vec<u8> {
    // RFC 3711 §4.3.1: key_id = label (1 byte) || r (6 bytes), where r = 0 for default KDR.
    // x = key_id right-aligned in a 14-byte (112-bit) field to match the salt length.
    // Layout: bytes 0..7 = zero-padding, byte 7 = label, bytes 8..14 = r (all zero).
    let mut x = [0u8; 14];
    x[7] = label;

    // IV = (master_salt XOR x) || 0x0000
    let mut iv = [0u8; 16];
    for i in 0..14 {
        iv[i] = master_salt[i] ^ x[i];
    }

    // Generate keystream using AES-CM
    let mut output = vec![0u8; out_len];
    let mut cipher = Aes128Ctr::new(master_key.into(), (&iv).into());
    cipher.apply_keystream(&mut output);
    output
}

/// Compute the IV for AES-CM encryption of an RTP packet (RFC 3711 §4.1.1)
///
/// IV = (k_s * 2^16) XOR (SSRC * 2^64) XOR (i * 2^16)
///
/// All values are 128-bit. The 112-bit salt occupies bytes 0-13 (shifted left
/// by 16 bits). SSRC occupies bytes 4-7. The 48-bit packet index occupies
/// bytes 8-13 (also shifted left by 16 bits). Bytes 14-15 are always zero.
fn compute_iv(salt: &[u8; 14], ssrc: u32, index: u64) -> [u8; 16] {
    let mut iv = [0u8; 16];

    let ssrc_bytes = ssrc.to_be_bytes();
    // Shift index left by 16 bits: 48-bit index lands at bytes 8-13, bytes 14-15 = 0
    let shifted_index = (index << 16).to_be_bytes();

    // Place SSRC at bytes 4-7
    iv[4] = ssrc_bytes[0];
    iv[5] = ssrc_bytes[1];
    iv[6] = ssrc_bytes[2];
    iv[7] = ssrc_bytes[3];
    // Place shifted index at bytes 8-15 (bytes 14-15 are zero from the shift)
    iv[8..16].copy_from_slice(&shifted_index);

    // XOR with salt (14 bytes at iv[0..14])
    for i in 0..14 {
        iv[i] ^= salt[i];
    }

    iv
}

/// Compute the full RTP header length including CSRC list and header extensions.
/// Returns None if the packet is too short to contain the declared header.
fn rtp_header_len(packet: &[u8]) -> Option<usize> {
    if packet.len() < 12 {
        return None;
    }
    let cc = (packet[0] & 0x0F) as usize;
    let mut len = 12 + cc * 4;
    if packet.len() < len {
        return None;
    }
    // Check extension bit (byte 0, bit 4)
    if packet[0] & 0x10 != 0 {
        if packet.len() < len + 4 {
            return None;
        }
        let ext_words = u16::from_be_bytes([packet[len + 2], packet[len + 3]]) as usize;
        len = ext_words
            .checked_mul(4)
            .and_then(|b| b.checked_add(4))
            .and_then(|b| b.checked_add(len))?;
        if packet.len() < len {
            return None;
        }
    }
    Some(len)
}

fn base64_decode(input: &str) -> anyhow::Result<Vec<u8>> {
    base64::engine::general_purpose::STANDARD
        .decode(input)
        .map_err(|e| anyhow::anyhow!("Invalid base64: {e}"))
}

pub(crate) fn base64_encode(data: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(data)
}

#[cfg(test)]
#[path = "srtp_tests.rs"]
mod tests;
