//! SRTP encrypt/decrypt for AES_CM_128_HMAC_SHA1_80 (RFC 3711).
//!
//! Supports SDES key exchange (a=crypto lines in SDP).

use aes::cipher::{KeyIvInit, StreamCipher};
use base64::Engine;
use hmac::{Hmac, Mac};
use sha1::Sha1;
use subtle::ConstantTimeEq;
use zeroize::Zeroize;

type Aes128Ctr = ctr::Ctr128BE<aes::Aes128>;
type HmacSha1 = Hmac<Sha1>;

const SRTP_AUTH_TAG_LEN: usize = 10; // 80 bits
const SRTP_MASTER_KEY_LEN: usize = 16; // 128 bits
const SRTP_MASTER_SALT_LEN: usize = 14; // 112 bits

/// SRTP session context for encrypt/decrypt
pub struct SrtpContext {
    /// Derived session encryption key (128 bits)
    pub(crate) cipher_key: [u8; 16],
    /// Derived session salt (112 bits)
    pub(crate) cipher_salt: [u8; 14],
    /// Derived session authentication key (160 bits)
    pub(crate) auth_key: [u8; 20],
    /// Rollover counter for extended sequence number
    roc: u32,
    /// Highest sequence number seen
    highest_seq: u16,
    seq_initialized: bool,
    /// Sliding replay window bitmap (64 packets behind highest_seq).
    /// Bit i is set if packet (highest_seq - i) has been seen.
    /// Bit 0 = highest_seq itself.
    replay_window: u64,
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
            roc: 0,
            highest_seq: 0,
            seq_initialized: false,
            replay_window: 0,
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

        self.update_roc(seq);
        let index = ((self.roc as u64) << 16) | seq as u64;

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
        mac.update(&self.roc.to_be_bytes());
        let tag = mac.finalize().into_bytes();
        output.extend_from_slice(&tag[..SRTP_AUTH_TAG_LEN]);

        Ok(output)
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

        // Estimate ROC for auth check (don't commit state yet)
        let (estimated_roc, _) = self.estimate_roc(seq);

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

        // Auth passed — now check replay window (commits ROC/seq state)
        let (_roc, index) = self.check_replay(seq)?;

        // Decrypt payload
        let header_len = rtp_header_len(auth_portion)
            .ok_or_else(|| anyhow::anyhow!("SRTP packet too short for header"))?;

        let mut output = auth_portion.to_vec();
        let iv = compute_iv(&self.cipher_salt, ssrc, index);
        let mut cipher = Aes128Ctr::new((&self.cipher_key).into(), (&iv).into());
        cipher.apply_keystream(&mut output[header_len..]);

        Ok(output)
    }

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

impl Drop for SrtpContext {
    fn drop(&mut self) {
        self.cipher_key.zeroize();
        self.cipher_salt.zeroize();
        self.auth_key.zeroize();
    }
}

/// SRTCP session context for encrypt/decrypt (RFC 3711 §3.4).
/// Uses the same master key as SRTP but derives separate session keys
/// with labels 0x03 (cipher), 0x04 (auth), 0x05 (salt).
pub struct SrtcpContext {
    pub(crate) cipher_key: [u8; 16],
    pub(crate) cipher_salt: [u8; 14],
    pub(crate) auth_key: [u8; 20],
    /// 31-bit SRTCP index counter (outbound)
    srtcp_index: u32,
    /// Highest SRTCP index seen (inbound replay protection)
    highest_recv_index: u32,
    recv_index_initialized: bool,
    /// Sliding replay window bitmap (64 indices behind highest_recv_index)
    replay_window: u64,
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
            highest_recv_index: 0,
            recv_index_initialized: false,
            replay_window: 0,
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

        // Check SRTCP replay window
        self.check_srtcp_replay(recv_index)?;

        // Strip E+index and auth tag to get the RTCP compound packet
        let mut output = srtcp_packet[..index_start].to_vec();

        if encrypted && output.len() > 8 {
            let ssrc = u32::from_be_bytes([output[4], output[5], output[6], output[7]]);
            let index = e_and_index & 0x7FFFFFFF;
            let iv = compute_srtcp_iv(&self.cipher_salt, ssrc, index);
            let mut cipher = Aes128Ctr::new((&self.cipher_key).into(), (&iv).into());
            cipher.apply_keystream(&mut output[8..]);
        }

        Ok(output)
    }

    /// Check + update the replay window for an incoming SRTCP packet.
    /// Returns Err if the packet is a replay. On success, updates window state.
    fn check_srtcp_replay(&mut self, index: u32) -> anyhow::Result<()> {
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
mod tests {
    use super::*;

    fn make_test_key() -> String {
        // 30 bytes of test key material, base64 encoded
        let key_material = [
            0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B, 0x0C, 0x0D, 0x0E,
            0x0F, 0x10, // 16-byte master key
            0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1A, 0x1B, 0x1C, 0x1D,
            0x1E, // 14-byte master salt
        ];
        // Encode to base64
        crate::session::endpoint_rtp::base64_encode(&key_material)
    }

    #[test]
    fn test_srtp_roundtrip() {
        let key = make_test_key();
        let mut protect_ctx = SrtpContext::from_sdes_key(&key).unwrap();
        let mut unprotect_ctx = SrtpContext::from_sdes_key(&key).unwrap();

        // Build a simple RTP packet
        let rtp = crate::media::rtp::RtpHeader::build(0, 1, 160, 0x12345678, false, &[0xAA; 160]);

        // Protect (encrypt + auth)
        let srtp = protect_ctx.protect(&rtp).unwrap();
        assert_eq!(srtp.len(), rtp.len() + SRTP_AUTH_TAG_LEN);

        // Payload should be different (encrypted)
        assert_ne!(&srtp[12..12 + 160], &rtp[12..12 + 160]);

        // Unprotect (verify + decrypt)
        let decrypted = unprotect_ctx.unprotect(&srtp).unwrap();
        assert_eq!(decrypted, rtp);
    }

    #[test]
    fn test_srtp_auth_failure() {
        let key = make_test_key();
        let mut protect_ctx = SrtpContext::from_sdes_key(&key).unwrap();
        let mut unprotect_ctx = SrtpContext::from_sdes_key(&key).unwrap();

        let rtp = crate::media::rtp::RtpHeader::build(0, 1, 160, 0x12345678, false, &[0xBB; 80]);
        let mut srtp = protect_ctx.protect(&rtp).unwrap();

        // Tamper with a byte
        srtp[20] ^= 0xFF;

        // Should fail auth
        assert!(unprotect_ctx.unprotect(&srtp).is_err());
    }

    #[test]
    fn test_srtp_multiple_packets() {
        let key = make_test_key();
        let mut protect_ctx = SrtpContext::from_sdes_key(&key).unwrap();
        let mut unprotect_ctx = SrtpContext::from_sdes_key(&key).unwrap();

        for seq in 0..10u16 {
            let rtp = crate::media::rtp::RtpHeader::build(
                0,
                seq,
                seq as u32 * 160,
                0xABCD,
                false,
                &[seq as u8; 160],
            );
            let srtp = protect_ctx.protect(&rtp).unwrap();
            let decrypted = unprotect_ctx.unprotect(&srtp).unwrap();
            assert_eq!(decrypted, rtp, "roundtrip failed for seq {}", seq);
        }
    }

    #[test]
    fn test_srtp_replay_rejected() {
        let key = make_test_key();
        let mut protect_ctx = SrtpContext::from_sdes_key(&key).unwrap();
        let mut unprotect_ctx = SrtpContext::from_sdes_key(&key).unwrap();

        let rtp = crate::media::rtp::RtpHeader::build(0, 1, 160, 0x12345678, false, &[0xCC; 160]);
        let srtp = protect_ctx.protect(&rtp).unwrap();

        // First unprotect should succeed
        let decrypted = unprotect_ctx.unprotect(&srtp).unwrap();
        assert_eq!(decrypted, rtp);

        // Replaying the same packet should fail
        let result = unprotect_ctx.unprotect(&srtp);
        assert!(result.is_err(), "replay should be rejected");
        assert!(result.unwrap_err().to_string().contains("replay"));
    }

    #[test]
    fn test_srtp_old_packet_outside_window_rejected() {
        let key = make_test_key();
        let mut protect_ctx = SrtpContext::from_sdes_key(&key).unwrap();
        let mut unprotect_ctx = SrtpContext::from_sdes_key(&key).unwrap();

        // Send packet seq=0
        let rtp0 = crate::media::rtp::RtpHeader::build(0, 0, 0, 0xABCD, false, &[0x00; 80]);
        let srtp0 = protect_ctx.protect(&rtp0).unwrap();

        // Advance well past the 64-packet window
        for seq in 1..=100u16 {
            let rtp = crate::media::rtp::RtpHeader::build(
                0,
                seq,
                seq as u32 * 160,
                0xABCD,
                false,
                &[seq as u8; 80],
            );
            let srtp = protect_ctx.protect(&rtp).unwrap();
            unprotect_ctx.unprotect(&srtp).unwrap();
        }

        // Now try to unprotect seq=0 — should be rejected (outside window)
        let result = unprotect_ctx.unprotect(&srtp0);
        assert!(
            result.is_err(),
            "old packet outside window should be rejected"
        );
    }

    #[test]
    fn test_base64_decode() {
        let decoded = base64_decode("AQIDBA==").unwrap();
        assert_eq!(decoded, vec![1, 2, 3, 4]);
    }

    #[test]
    fn test_base64_invalid_chars() {
        let result = base64_decode("AQID!A==");
        assert!(
            result.is_err(),
            "base64_decode should reject invalid characters"
        );
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("Invalid base64"),
            "error message should mention invalid base64: {}",
            err_msg
        );
    }

    // ---- SRTCP tests ----

    #[test]
    fn test_srtcp_roundtrip() {
        let key = make_test_key();
        let mut protect_ctx = SrtcpContext::from_sdes_key(&key).unwrap();
        let mut unprotect_ctx = SrtcpContext::from_sdes_key(&key).unwrap();

        // Build a simple RTCP SR (use the rtcp module)
        let mut stats = crate::media::rtcp::RtcpStats::new();
        stats.packets_sent = 50;
        stats.octets_sent = 8000;
        let rtcp = crate::media::rtcp::build_sr_rr(0x12345678, 0xAABBCCDD, &mut stats, 0, 8000);

        let srtcp = protect_ctx.protect_rtcp(&rtcp).unwrap();
        // Should be longer: +4 (E+index) +10 (auth tag)
        assert_eq!(srtcp.len(), rtcp.len() + 4 + SRTP_AUTH_TAG_LEN);

        // First 8 bytes (header+SSRC) should be in the clear
        assert_eq!(&srtcp[..4], &rtcp[..4], "RTCP header should be in clear");
        assert_eq!(&srtcp[4..8], &rtcp[4..8], "SSRC should be in clear");

        // Payload should be different (encrypted)
        if rtcp.len() > 8 {
            assert_ne!(
                &srtcp[8..rtcp.len()],
                &rtcp[8..],
                "payload should be encrypted"
            );
        }

        let decrypted = unprotect_ctx.unprotect_rtcp(&srtcp).unwrap();
        assert_eq!(decrypted, rtcp);
    }

    #[test]
    fn test_srtcp_auth_failure() {
        let key = make_test_key();
        let mut protect_ctx = SrtcpContext::from_sdes_key(&key).unwrap();
        let mut unprotect_ctx = SrtcpContext::from_sdes_key(&key).unwrap();

        let mut stats = crate::media::rtcp::RtcpStats::new();
        let rtcp = crate::media::rtcp::build_sr_rr(0x12345678, 0xAABBCCDD, &mut stats, 0, 8000);
        let mut srtcp = protect_ctx.protect_rtcp(&rtcp).unwrap();

        // Tamper with the encrypted payload
        srtcp[10] ^= 0xFF;

        assert!(unprotect_ctx.unprotect_rtcp(&srtcp).is_err());
    }

    #[test]
    fn test_srtp_sequence_wraparound() {
        let key = make_test_key();
        let mut protect_ctx = SrtpContext::from_sdes_key(&key).unwrap();
        let mut unprotect_ctx = SrtpContext::from_sdes_key(&key).unwrap();

        // Send a realistic stream of packets up through and past the 16-bit
        // sequence number wraparound boundary (65534, 65535, 0, 1, ...).
        // Start well before the boundary to establish steady state.
        let start: u16 = 65500;
        let count: u32 = 100; // crosses from 65500 through 0 up to ~64

        for i in 0..count {
            let seq = start.wrapping_add(i as u16);
            let ts = i.wrapping_mul(160);
            let rtp = crate::media::rtp::RtpHeader::build(
                0,
                seq,
                ts,
                0x12345678,
                false,
                &[seq as u8; 160],
            );
            let srtp = protect_ctx.protect(&rtp).unwrap();
            let decrypted = unprotect_ctx
                .unprotect(&srtp)
                .unwrap_or_else(|e| panic!("unprotect failed for seq {} (i={}): {}", seq, i, e));
            assert_eq!(decrypted, rtp, "roundtrip failed for seq {} (i={})", seq, i);
        }
    }

    #[test]
    fn test_srtp_out_of_order_within_window() {
        let key = make_test_key();
        let mut protect_ctx = SrtpContext::from_sdes_key(&key).unwrap();
        let mut unprotect_ctx = SrtpContext::from_sdes_key(&key).unwrap();

        // Protect packets 0..=4 in order
        let mut srtp_packets = Vec::new();
        for seq in 0u16..=4 {
            let rtp = crate::media::rtp::RtpHeader::build(
                0,
                seq,
                seq as u32 * 160,
                0xABCD,
                false,
                &[seq as u8; 80],
            );
            let srtp = protect_ctx.protect(&rtp).unwrap();
            srtp_packets.push((seq, rtp, srtp));
        }

        // Unprotect in out-of-order sequence: 0, 2, 4, 1, 3
        let unprotect_order = [0usize, 2, 4, 1, 3];
        for &idx in &unprotect_order {
            let (seq, ref original_rtp, ref srtp) = srtp_packets[idx];
            let decrypted = unprotect_ctx.unprotect(srtp).unwrap_or_else(|e| {
                panic!("unprotect failed for seq {} (out-of-order): {}", seq, e)
            });
            assert_eq!(
                &decrypted, original_rtp,
                "roundtrip failed for seq {} (out-of-order)",
                seq
            );
        }
    }

    #[test]
    fn test_srtcp_different_keys_from_srtp() {
        let key = make_test_key();
        let srtp_ctx = SrtpContext::from_sdes_key(&key).unwrap();
        let srtcp_ctx = SrtcpContext::from_sdes_key(&key).unwrap();

        // SRTCP uses labels 0x03/0x04/0x05 vs SRTP 0x00/0x01/0x02
        // so derived keys must differ
        assert_ne!(srtp_ctx.cipher_key, srtcp_ctx.cipher_key);
        assert_ne!(srtp_ctx.auth_key, srtcp_ctx.auth_key);
        assert_ne!(srtp_ctx.cipher_salt, srtcp_ctx.cipher_salt);
    }

    #[test]
    fn test_srtcp_multiple_packets() {
        let key = make_test_key();
        let mut protect_ctx = SrtcpContext::from_sdes_key(&key).unwrap();
        let mut unprotect_ctx = SrtcpContext::from_sdes_key(&key).unwrap();

        for i in 0..5 {
            let mut stats = crate::media::rtcp::RtcpStats::new();
            stats.packets_sent = i * 10;
            let rtcp = crate::media::rtcp::build_sr_rr(0x12345678, 0xAABBCCDD, &mut stats, 0, 8000);
            let srtcp = protect_ctx.protect_rtcp(&rtcp).unwrap();
            let decrypted = unprotect_ctx.unprotect_rtcp(&srtcp).unwrap();
            assert_eq!(decrypted, rtcp, "SRTCP roundtrip failed for packet {}", i);
        }
    }

    #[test]
    fn test_srtcp_replay_rejected() {
        let key = make_test_key();
        let mut protect_ctx = SrtcpContext::from_sdes_key(&key).unwrap();
        let mut unprotect_ctx = SrtcpContext::from_sdes_key(&key).unwrap();

        let mut stats = crate::media::rtcp::RtcpStats::new();
        let rtcp = crate::media::rtcp::build_sr_rr(0x12345678, 0xAABBCCDD, &mut stats, 0, 8000);
        let srtcp = protect_ctx.protect_rtcp(&rtcp).unwrap();

        // First unprotect should succeed
        unprotect_ctx.unprotect_rtcp(&srtcp).unwrap();

        // Replaying the same packet should fail
        let result = unprotect_ctx.unprotect_rtcp(&srtcp);
        assert!(result.is_err(), "SRTCP replay should be rejected");
        assert!(result.unwrap_err().to_string().contains("replay"));
    }

    #[test]
    fn test_srtcp_old_packet_outside_window_rejected() {
        let key = make_test_key();
        let mut protect_ctx = SrtcpContext::from_sdes_key(&key).unwrap();
        let mut unprotect_ctx = SrtcpContext::from_sdes_key(&key).unwrap();

        let mut stats = crate::media::rtcp::RtcpStats::new();

        // Protect 100 RTCP packets
        let mut srtcp_packets = Vec::new();
        for _ in 0..100 {
            stats.packets_sent += 10;
            let rtcp = crate::media::rtcp::build_sr_rr(0x12345678, 0xAABBCCDD, &mut stats, 0, 8000);
            srtcp_packets.push(protect_ctx.protect_rtcp(&rtcp).unwrap());
        }

        // Unprotect all in order
        for srtcp in &srtcp_packets {
            unprotect_ctx.unprotect_rtcp(srtcp).unwrap();
        }

        // Replaying packet 0 should fail (outside 64-packet window)
        let result = unprotect_ctx.unprotect_rtcp(&srtcp_packets[0]);
        assert!(
            result.is_err(),
            "old SRTCP packet outside window should be rejected"
        );
    }

    #[test]
    fn test_srtcp_index_increments() {
        let key = make_test_key();
        let mut protect_ctx = SrtcpContext::from_sdes_key(&key).unwrap();

        let mut stats = crate::media::rtcp::RtcpStats::new();
        let rtcp = crate::media::rtcp::build_sr_rr(0x12345678, 0xAABBCCDD, &mut stats, 0, 8000);

        // Protect 3 RTCP packets and extract the E+index field from each
        let mut indices = Vec::new();
        for _ in 0..3 {
            let srtcp = protect_ctx.protect_rtcp(&rtcp).unwrap();
            // E+index is 4 bytes just before the 10-byte auth tag
            let idx_offset = srtcp.len() - SRTP_AUTH_TAG_LEN - 4;
            let e_and_index = u32::from_be_bytes([
                srtcp[idx_offset],
                srtcp[idx_offset + 1],
                srtcp[idx_offset + 2],
                srtcp[idx_offset + 3],
            ]);
            indices.push(e_and_index);
        }

        // E-flag should be set (0x80000000) and indices should be 0, 1, 2
        assert_eq!(
            indices[0],
            0x80000000 | 0,
            "first packet should have index 0"
        );
        assert_eq!(
            indices[1],
            0x80000000 | 1,
            "second packet should have index 1"
        );
        assert_eq!(
            indices[2],
            0x80000000 | 2,
            "third packet should have index 2"
        );
    }

    #[test]
    fn test_srtp_different_keys_fail() {
        // Create two SrtpContexts with DIFFERENT keys
        let key_material_a = [
            0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B, 0x0C, 0x0D, 0x0E,
            0x0F, 0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1A, 0x1B, 0x1C,
            0x1D, 0x1E,
        ];
        let key_material_b = [
            0xA1, 0xA2, 0xA3, 0xA4, 0xA5, 0xA6, 0xA7, 0xA8, 0xA9, 0xAA, 0xAB, 0xAC, 0xAD, 0xAE,
            0xAF, 0xB0, 0xB1, 0xB2, 0xB3, 0xB4, 0xB5, 0xB6, 0xB7, 0xB8, 0xB9, 0xBA, 0xBB, 0xBC,
            0xBD, 0xBE,
        ];
        let key_a = crate::session::endpoint_rtp::base64_encode(&key_material_a);
        let key_b = crate::session::endpoint_rtp::base64_encode(&key_material_b);

        let mut ctx_a = SrtpContext::from_sdes_key(&key_a).unwrap();
        let mut ctx_b = SrtpContext::from_sdes_key(&key_b).unwrap();

        // Protect a packet with context A
        let rtp = crate::media::rtp::RtpHeader::build(0, 1, 160, 0x12345678, false, &[0xAA; 160]);
        let srtp = ctx_a.protect(&rtp).unwrap();

        // Try to unprotect with context B — should fail with auth tag mismatch
        let result = ctx_b.unprotect(&srtp);
        assert!(result.is_err(), "unprotect with different key should fail");
        let err_msg = result.err().unwrap().to_string();
        assert!(
            err_msg.contains("auth tag mismatch"),
            "error should mention auth tag mismatch: {err_msg}"
        );
    }

    #[test]
    fn test_srtp_short_key_material() {
        // Base64 of 20 bytes (less than the required 30)
        let short_material = [0x01u8; 20];
        let short_key = crate::session::endpoint_rtp::base64_encode(&short_material);

        let result = SrtpContext::from_sdes_key(&short_key);
        assert!(result.is_err(), "short key material should fail");
        let err_msg = result.err().unwrap().to_string();
        assert!(
            err_msg.contains("too short"),
            "error should mention key material too short: {err_msg}"
        );
    }

    #[test]
    fn test_srtp_exact_30_byte_key() {
        // Exactly 30 bytes of key material (16 master key + 14 master salt)
        let key_material: [u8; 30] = [
            0xDE, 0xAD, 0xBE, 0xEF, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A,
            0x0B, 0x0C, // 16-byte master key
            0x10, 0x20, 0x30, 0x40, 0x50, 0x60, 0x70, 0x80, 0x90, 0xA0, 0xB0, 0xC0, 0xD0,
            0xE0, // 14-byte master salt
        ];
        let key_b64 = crate::session::endpoint_rtp::base64_encode(&key_material);

        // Creating the context should succeed (no panic, no error)
        let ctx = SrtpContext::from_sdes_key(&key_b64);
        assert!(
            ctx.is_ok(),
            "exact 30-byte key material should succeed: {:?}",
            ctx.err()
        );

        let ctx = ctx.unwrap();
        // Derived keys should be non-zero (KDF actually ran)
        assert_ne!(
            ctx.cipher_key, [0u8; 16],
            "cipher key should be derived (non-zero)"
        );
        assert_ne!(
            ctx.cipher_salt, [0u8; 14],
            "cipher salt should be derived (non-zero)"
        );
        assert_ne!(
            ctx.auth_key, [0u8; 20],
            "auth key should be derived (non-zero)"
        );

        // Verify the context can actually protect and unprotect a packet
        let mut protect_ctx = SrtpContext::from_sdes_key(&key_b64).unwrap();
        let mut unprotect_ctx = SrtpContext::from_sdes_key(&key_b64).unwrap();

        let rtp = crate::media::rtp::RtpHeader::build(0, 1, 160, 0xCAFEBABE, false, &[0x55; 160]);
        let srtp = protect_ctx.protect(&rtp).unwrap();
        let decrypted = unprotect_ctx.unprotect(&srtp).unwrap();
        assert_eq!(
            decrypted, rtp,
            "roundtrip with exact 30-byte key should succeed"
        );
    }

    #[test]
    fn test_srtp_key_29_bytes_rejected() {
        // Exactly 1 byte short of the required 30
        let key_material = [0xAA; 29];
        let key_b64 = crate::session::endpoint_rtp::base64_encode(&key_material);
        let result = SrtpContext::from_sdes_key(&key_b64);
        assert!(result.is_err(), "29-byte key material should be rejected");
        let err = result.err().unwrap();
        assert!(err.to_string().contains("too short"));
    }

    #[test]
    fn test_srtp_key_31_bytes_accepted() {
        // 31 bytes is 1 byte MORE than required (30). The extra byte should be
        // silently ignored — the context should initialize and work correctly.
        let key_material = [0xBB; 31];
        let key_b64 = crate::session::endpoint_rtp::base64_encode(&key_material);
        let result = SrtpContext::from_sdes_key(&key_b64);
        assert!(
            result.is_ok(),
            "31-byte key material should be accepted (extra byte ignored)"
        );

        // Verify it can protect/unprotect roundtrip
        let mut protect_ctx = SrtpContext::from_sdes_key(&key_b64).unwrap();
        let mut unprotect_ctx = SrtpContext::from_sdes_key(&key_b64).unwrap();
        let rtp = crate::media::rtp::RtpHeader::build(0, 1, 160, 0x12345678, false, &[0xAA; 160]);
        let srtp = protect_ctx.protect(&rtp).unwrap();
        let decrypted = unprotect_ctx.unprotect(&srtp).unwrap();
        assert_eq!(decrypted, rtp, "roundtrip with 31-byte key should work");
    }

    #[test]
    fn test_srtp_replay_exact_duplicate() {
        let key = make_test_key();
        let mut protect_ctx = SrtpContext::from_sdes_key(&key).unwrap();
        let mut unprotect_ctx = SrtpContext::from_sdes_key(&key).unwrap();

        let rtp = crate::media::rtp::RtpHeader::build(0, 42, 6720, 0xABCD, false, &[0x55; 160]);
        let srtp = protect_ctx.protect(&rtp).unwrap();

        // First unprotect should succeed
        unprotect_ctx.unprotect(&srtp).unwrap();

        // Exact duplicate should be rejected (replay)
        let result = unprotect_ctx.unprotect(&srtp);
        assert!(result.is_err(), "exact duplicate packet should be rejected");
        assert!(
            result.unwrap_err().to_string().contains("replay"),
            "error should mention replay"
        );
    }

    #[test]
    fn test_srtp_replay_outside_window() {
        let key = make_test_key();
        let mut protect_ctx = SrtpContext::from_sdes_key(&key).unwrap();
        let mut unprotect_ctx = SrtpContext::from_sdes_key(&key).unwrap();

        // Protect packets 0..100
        let mut srtp_packets = Vec::new();
        for seq in 0u16..100 {
            let rtp = crate::media::rtp::RtpHeader::build(
                0,
                seq,
                seq as u32 * 160,
                0xABCD,
                false,
                &[seq as u8; 80],
            );
            srtp_packets.push(protect_ctx.protect(&rtp).unwrap());
        }

        // Unprotect packets 0..100 in order
        for srtp in &srtp_packets {
            unprotect_ctx.unprotect(srtp).unwrap();
        }

        // Now try to replay packet 0 — should be outside the 64-packet replay window
        let result = unprotect_ctx.unprotect(&srtp_packets[0]);
        assert!(
            result.is_err(),
            "packet far outside replay window should be rejected"
        );
    }

    #[test]
    fn test_srtp_key_malformed_base64_variants() {
        // Embedded null byte
        let result = SrtpContext::from_sdes_key("AQID\x00BA==");
        assert!(result.is_err(), "base64 with null byte should be rejected");

        // Whitespace in middle
        let result = SrtpContext::from_sdes_key("AQ ID BA==");
        assert!(result.is_err(), "base64 with space should be rejected");

        // Unicode
        let result = SrtpContext::from_sdes_key("AQIDé==");
        assert!(result.is_err(), "base64 with unicode should be rejected");

        // Empty string → too-short key material
        let result = SrtpContext::from_sdes_key("");
        assert!(result.is_err(), "empty string should be rejected");
    }

    // ── IV computation tests ─────────────────────────────────────────

    #[test]
    fn test_compute_iv_rfc3711_layout() {
        // Verify RFC 3711 §4.1.1 layout:
        // IV = (k_s * 2^16) XOR (SSRC * 2^64) XOR (i * 2^16)
        //
        // With all-zero salt, SSRC=0, index=0, IV should be all zeros.
        let salt = [0u8; 14];
        let iv = compute_iv(&salt, 0, 0);
        assert_eq!(iv, [0u8; 16], "all-zero inputs should produce all-zero IV");

        // With non-zero salt, zero SSRC and index:
        // IV should equal salt at bytes 0-13, bytes 14-15 = 0
        let salt = [
            0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B, 0x0C, 0x0D, 0x0E,
        ];
        let iv = compute_iv(&salt, 0, 0);
        assert_eq!(
            &iv[0..14],
            &salt,
            "salt-only IV should have salt at bytes 0-13"
        );
        assert_eq!(iv[14], 0, "byte 14 must be zero");
        assert_eq!(iv[15], 0, "byte 15 must be zero");
    }

    #[test]
    fn test_compute_iv_ssrc_position() {
        // SSRC * 2^64 places SSRC at bytes 4-7
        let salt = [0u8; 14];
        let iv = compute_iv(&salt, 0xDEADBEEF, 0);
        assert_eq!(iv[4], 0xDE);
        assert_eq!(iv[5], 0xAD);
        assert_eq!(iv[6], 0xBE);
        assert_eq!(iv[7], 0xEF);
        // Bytes 0-3 and 8-15 should be zero (no salt, no index)
        assert_eq!(&iv[0..4], &[0, 0, 0, 0]);
        assert_eq!(&iv[8..16], &[0, 0, 0, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn test_compute_iv_index_shifted_by_16() {
        // i * 2^16 places the 48-bit index at bytes 8-13 (shifted left by 16)
        let salt = [0u8; 14];
        // Index = ROC=1, SEQ=0 → raw 48-bit index = (1 << 16) | 0 = 0x10000
        let index: u64 = 0x10000;
        let iv = compute_iv(&salt, 0, index);
        // index << 16 = 0x1_0000_0000 → bytes: [0,0,0,1,0,0,0,0]
        // placed at iv[8..16]
        assert_eq!(
            &iv[8..16],
            &[0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00]
        );
        // Bytes 14-15 must be zero
        assert_eq!(iv[14], 0);
        assert_eq!(iv[15], 0);
    }

    #[test]
    fn test_compute_iv_full_combination() {
        // Combine salt, SSRC, and index and verify XOR
        let salt: [u8; 14] = [
            0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
        ];
        let ssrc: u32 = 0x12345678;
        // 48-bit index: ROC=0, SEQ=1 → index = 1
        let index: u64 = 1;

        let iv = compute_iv(&salt, ssrc, index);

        // Expected (manually computed):
        // index=1, shifted: 1u64 << 16 = 0x0000_0000_0001_0000
        //   to_be_bytes() = [0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00]
        //   placed at iv[8..16]
        //
        // Before salt XOR:
        //   iv = [0,0,0,0, 0x12,0x34,0x56,0x78, 0,0,0,0, 0,0x01,0,0]
        // After XOR with salt (all 0xFF at bytes 0-13):
        //   iv[0..4]   = 0xFF
        //   iv[4..8]   = SSRC XOR salt = [0xED, 0xCB, 0xA9, 0x87]
        //   iv[8..14]  = [0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFE]
        //   iv[14..16] = [0, 0] (no salt contribution)
        assert_eq!(iv[0], 0xFF);
        assert_eq!(iv[1], 0xFF);
        assert_eq!(iv[2], 0xFF);
        assert_eq!(iv[3], 0xFF);
        assert_eq!(iv[4], 0xFF ^ 0x12);
        assert_eq!(iv[5], 0xFF ^ 0x34);
        assert_eq!(iv[6], 0xFF ^ 0x56);
        assert_eq!(iv[7], 0xFF ^ 0x78);
        // shifted index [0,0,0,0,0,0x01,0,0] at iv[8..16], XOR with salt[8..14]
        assert_eq!(iv[8], 0xFF); // 0x00 ^ 0xFF
        assert_eq!(iv[9], 0xFF); // 0x00 ^ 0xFF
        assert_eq!(iv[10], 0xFF); // 0x00 ^ 0xFF
        assert_eq!(iv[11], 0xFF); // 0x00 ^ 0xFF
        assert_eq!(iv[12], 0xFF); // 0x00 ^ 0xFF
        assert_eq!(iv[13], 0xFE); // 0x01 ^ 0xFF
        assert_eq!(iv[14], 0x00); // no salt at byte 14
        assert_eq!(iv[15], 0x00);
    }

    #[test]
    fn test_compute_srtcp_iv_layout() {
        // Same layout rules as SRTP IV but with 32-bit SRTCP index
        let salt = [0u8; 14];

        // All zeros
        let iv = compute_srtcp_iv(&salt, 0, 0);
        assert_eq!(iv, [0u8; 16]);

        // Salt only
        let salt = [
            0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B, 0x0C, 0x0D, 0x0E,
        ];
        let iv = compute_srtcp_iv(&salt, 0, 0);
        assert_eq!(&iv[0..14], &salt);
        assert_eq!(iv[14], 0);
        assert_eq!(iv[15], 0);

        // SSRC only
        let salt = [0u8; 14];
        let iv = compute_srtcp_iv(&salt, 0xAABBCCDD, 0);
        assert_eq!(&iv[4..8], &[0xAA, 0xBB, 0xCC, 0xDD]);

        // Index shifted by 16: SRTCP index 1 → (1u64 << 16) = 0x10000
        let iv = compute_srtcp_iv(&salt, 0, 1);
        // [0,0,0,0,0,0x01,0x00,0x00] at iv[8..16]
        assert_eq!(&iv[8..14], &[0x00, 0x00, 0x00, 0x00, 0x00, 0x01]);
        assert_eq!(iv[14], 0);
        assert_eq!(iv[15], 0);
    }

    #[test]
    fn test_srtcp_protect_unprotect_roundtrip() {
        let key = make_test_key();
        let mut protect_ctx = SrtcpContext::from_sdes_key(&key).unwrap();
        let mut unprotect_ctx = SrtcpContext::from_sdes_key(&key).unwrap();

        // Build a minimal RTCP Sender Report by hand:
        // 4-byte header + 4-byte SSRC + 20-byte SR body = 28 bytes
        // V=2, P=0, RC=0 → first byte = 0x80
        // PT=200 (SR)
        // Length=6 (28 bytes / 4 - 1 = 6)
        let mut rtcp = [0u8; 28];
        rtcp[0] = 0x80; // V=2, P=0, RC=0
        rtcp[1] = 200; // PT = Sender Report
        rtcp[2] = 0x00; // length high byte
        rtcp[3] = 0x06; // length low byte (6 32-bit words after header)
        // SSRC
        rtcp[4] = 0x12;
        rtcp[5] = 0x34;
        rtcp[6] = 0x56;
        rtcp[7] = 0x78;
        // Fill SR body (NTP timestamp, RTP timestamp, packet/octet counts)
        // with recognizable non-zero data so we can verify roundtrip
        for i in 8..28 {
            rtcp[i] = i as u8;
        }
        let original = rtcp.to_vec();

        // Protect (encrypt + authenticate)
        let srtcp = protect_ctx.protect_rtcp(&original).unwrap();
        // SRTCP adds 4-byte E+index field and 10-byte auth tag
        assert_eq!(srtcp.len(), original.len() + 4 + SRTP_AUTH_TAG_LEN);

        // Unprotect (verify + decrypt) with a separate context from the same key
        let decrypted = unprotect_ctx.unprotect_rtcp(&srtcp).unwrap();
        assert_eq!(
            decrypted, original,
            "SRTCP roundtrip with hand-built SR should recover original packet"
        );
    }
}
