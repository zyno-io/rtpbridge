# SRTP

rtpbridge supports SRTP encryption for plain RTP endpoints using SDES key exchange.

## SDES Key Exchange

For plain RTP endpoints with `srtp: true`, rtpbridge generates an `a=crypto` line in the SDP:

```
a=crypto:1 AES_CM_128_HMAC_SHA1_80 inline:<base64-key>
```

When accepting an offer with `a=crypto`, rtpbridge generates an independent TX key for the SDP answer. The offerer's key is used only for the RX (decrypt) direction to prevent keystream reuse between directions.

## Cipher Suite

Only `AES_CM_128_HMAC_SHA1_80` is supported:

- **Encryption**: AES-128 in Counter Mode (AES-CM, RFC 3711 §4.1.1)
- **Authentication**: HMAC-SHA1 truncated to 80 bits
- **Key material**: 30 bytes (16-byte master key + 14-byte master salt)
- **Key derivation**: SRTP KDF (RFC 3711 §4.3.1)

## Key Material

The base64-encoded key in `a=crypto` decodes to 30 bytes:

```
Bytes 0-15:  Master key (128 bits)
Bytes 16-29: Master salt (112 bits)
```

Session keys are derived using the SRTP KDF:
- Label 0x00 → cipher key (16 bytes)
- Label 0x01 → auth key (20 bytes)
- Label 0x02 → salt key (14 bytes)

## Packet Processing

### Outbound (protect)
1. Encrypt RTP payload using AES-CM with IV derived from SSRC + packet index
2. Compute HMAC-SHA1 over (RTP header + encrypted payload + ROC)
3. Append truncated 10-byte auth tag

### Inbound (unprotect)
1. Verify HMAC-SHA1 auth tag
2. Decrypt RTP payload using AES-CM
3. Return decrypted RTP packet (auth tag stripped)

## WebRTC

WebRTC endpoints handle SRTP internally via str0m's DTLS-SRTP. The SRTP module is only used for plain RTP endpoints with SDES.

## Rekey

Plain RTP/SRTP endpoints support mid-session key rotation via `endpoint.srtp_rekey`:

1. **`endpoint.srtp_rekey`** generates a new TX master key and returns an SDP offer with the new `a=crypto` line. The TX context is replaced immediately — outbound packets use the new key.

2. **`endpoint.accept_answer`** with the remote's new SDP installs the remote's new RX key as a pending context. A 5-second dual-context transition window begins.

3. **Dual-context decryption**: During the transition, inbound packets are tried against the new key first. If the new key succeeds, the old key is discarded (early promotion). If the new key fails, the old key is used as fallback.

4. **Switchover deadline**: After 5 seconds, any remaining old context is force-replaced by the new context, regardless of whether early promotion occurred.

5. **Address learning**: The symmetric RTP address learning window is reopened on rekey to handle NAT rebinding that may accompany key rotation.

SRTCP contexts follow the same dual-context pattern independently of SRTP.

## Implementation

The SRTP implementation uses pure Rust crates (`aes`, `ctr`, `hmac`, `sha1`) with no C dependencies beyond what str0m already requires.
