# Codecs

rtpbridge supports three audio codecs with automatic transcoding between them.

## Supported Codecs

| Codec | PT | Sample Rate | RTP Clock Rate | Crate |
|-------|-----|-------------|----------------|-------|
| PCMU (G.711 mu-law) | 0 | 8 kHz | 8000 | `xlaw` (pure Rust) |
| G.722 | 9 | 16 kHz | 8000* | `ezk-g722` (pure Rust) |
| Opus | 111 (dynamic) | 48 kHz | 48000 | `opus` (libopus FFI) |

\* G.722 uses 8000 in SDP per RFC 3551 even though the actual audio sample rate is 16 kHz.

## Transcoding

When endpoints in a session use different codecs, rtpbridge automatically transcodes:

```
Source codec → Decode to PCM i16 → Resample → Encode to destination codec
```

The resample step handles rate conversion between 8 kHz, 16 kHz, and 48 kHz using linear interpolation. Opus requires exact frame sizes (960 samples at 48 kHz for 20ms ptime), so the pipeline pads or truncates after resampling.

### Passthrough

Matching codecs forward directly when their directional Opus receive profiles permit it.
A stricter bitrate, playback bandwidth, stereo, or packetization requirement can require
decode/encode even when both legs use Opus. These routes contribute to peer conversion
metrics with the reason `opus_receive_profile`; ordinary codec differences use `codec_mismatch`.

### DTMF Bypass

DTMF (telephone-event) packets are **never transcoded**. They bypass the transcode pipeline entirely and are forwarded as-is, with payload type remapping if the two endpoints negotiated different dynamic PTs.

## Ptime

All codecs use 20ms ptime:

| Codec | Samples per 20ms |
|-------|-------------------|
| PCMU | 160 |
| G.722 | 320 |
| Opus | 960 |

## Telephone-Event

For WebRTC endpoints, RFC 4733 telephone-event uses PT 101. For plain RTP/SRTP endpoints, generated SDP includes telephone-event by default and answers retain the offered telephone-event payload type and clock rate. The `a=fmtp:101 0-16` line supports digits 0-9, *, #, A-D, and flash. File, tone, bridge, and WebSocket audio endpoints do not negotiate telephone-event.

## Source-aware negotiation

An established conversational source takes priority over the default Opus/G.722/PCMU
quality ranking. For an unanswered source, its supported codecs take priority in quality
order. SIP proposals use one audio codec; WebRTC enables one compatible Opus/PCMU codec
through str0m. G.722 requires conversion on a WebRTC leg. Operations without source
context retain their existing codec defaults.

Opus receive limits are directional. Source receive constraints shape an outgoing RTP
offer; an accepted destination's receive constraints shape an unanswered RTP caller's
answer. Payload types and SRTP keys remain local to each endpoint. Plain RTP advertises
20ms maximum packetization; unsupported Opus packetization is excluded before selection,
and WebRTC rejects it before mutating negotiation state. Conversion uses mono 20ms Opus
at up to 24kbps and compatible bandwidth. It retains the existing variable-bitrate encoder
when the receive ceiling permits 24kbps; a lower ceiling uses constant bitrate to bound
each encoded packet without imposing that encoder cost on unconstrained destinations.
Direct forwarding conservatively compares negotiated receive envelopes rather than
assuming every packet is mono, fullband, or 20ms.

The additive control fields, capability detection, descriptors, and session-scoped source
lookup are documented in [the endpoint protocol](../protocol/endpoints.md#source-aware-negotiation).
SIP rejection retries and endpoint sharing remain owned by the call controller.
