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

If source and destination use the same codec, packets are forwarded directly without transcoding.

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

RFC 4733 telephone-event is always negotiated (PT 101 by default). The `a=fmtp:101 0-16` line supports digits 0-9, *, #, A-D, and flash.
