# Performance and capacity

Capacity depends on the negotiated codecs, fanout, native codec libraries, packet duration, storage, and host scheduler. Measure the intended workload on the deployment host; this project does not establish a universal sessions-per-core or bytes-per-session figure.

## Media work

A destination with one source and the same codec forwards packets without decoding unless VAD or fax analysis is active. A destination with multiple sources mixes PCM even when all participants use the same codec. WebRTC currently negotiates Opus; selecting G.722 for its WebRTC side is not supported.

| Codec | PCM rate | RTP clock | Payload for 20 ms |
| --- | --- | --- | --- |
| PCMU | 8 kHz | 8 kHz | 160 bytes |
| G.722 | 16 kHz | 8 kHz | 160 bytes |
| Opus | 48 kHz | 48 kHz | Variable; encoder targets 24 kbps |
| Internal L16 | 8, 16 or 48 kHz | Same as PCM | 320, 640 or 1920 bytes |

Sources that require PCM are decoded once per input packet. VAD, fax analysis, transcoders and mixers share that decode; destinations at the same PCM rate share the source's resampling result. Each mixed destination still needs its own sum and encoder. The mixer uses a wide sum and clamps the final sample once.

For 20 participants each sending 50 packets/s, shared decoding requires 1,000 input decodes/s, compared with 19,000 in the previous per-destination implementation. This is a work-count reduction, not a claim of a 19× CPU speedup. Summation, encoding, encryption, socket IO and allocation still contribute to total cost.

Decoded short packets accumulate and long packets produce multiple 20 ms frames; they are not truncated or silently padded. Supported native codec packets are bounded to 120 ms. RTP is reordered before stateful decoding. SSRC, sequence or timestamp discontinuities reset a partial source frame. Same-codec forwarding retains the original packet duration.

The linear resampler preserves block duration and continuity with one input sample of delay. It is intended for voice and has no anti-alias filter. Test audio quality before using it for music.

## Bounded storage and sockets

These process limits complement the configured session, endpoint, connection and packet-channel limits:

| Resource | Bound | Saturation behavior |
| --- | --- | --- |
| Storage/decode workers | 4 threads, 64 waiting jobs | `STORAGE_BUSY` |
| Playback system DNS lookups | 4 jobs, including cancelled requests whose resolver call is still running | `DNS_BUSY` |
| File decoder streams | 64, including shared playback | `PLAYBACK_BUSY` |
| Nonshared playback prefetch | 4 frames per stream | Decoder waits; session keeps running |
| Recording writers | 32 threads | `RECORDING_BUSY` |
| Queued recording payload | 1 MiB per recording, 16 MiB process total, plus packet-count limit | Recording packets drop; counters increase |
| Recording HTTP downloads | 4, one 64 KiB buffer each | HTTP 503 |
| WebSocket writer backlog | 16 messages and 1 MiB per socket | Connection closes |
| WebSocket control input | 16 requests and 1 MiB total | Connection closes |
| WebSocket audio input | 256 KiB per endpoint | Connection closes on overflow |
| Concurrent transfers | 64 coordinators, 10-second deadline | `TRANSFER_BUSY` or `TRANSFER_TIMEOUT` |

A timeout cannot interrupt a filesystem syscall. Work already running retains its file lease and admission permit until it actually finishes. Recording flush timeout stops observation; the bounded writer continues draining. Stalled storage therefore consumes finite worker capacity and can cause subsequent storage requests to fail, while sessions continue processing media.

URL downloads default to 16 active transfers, 64 pending distinct keys, 256 request owners, 1,000 cache entries and a 1 GiB cache budget. Each active transfer reserves `max_file_download_bytes` before opening its temporary file and reduces the reservation to actual size after completion. With the default 100 MiB per-file maximum, the disk budget can reject work before all 16 network slots are occupied. Leased files are never evicted. See [configuration](./configuration.md) for migration and tuning.

These bounds are not a RAM sizing estimate. Also account for codec state, endpoint maps, routing edges, WebRTC/TLS state, OS socket buffers, queued events and allocator overhead. Increasing session or endpoint limits increases worst-case CPU and memory; an endpoint limit is not a CPU reservation.

## Ports and recording storage

WebRTC allocates one UDP socket per configured address family. Plain RTP/SRTP allocates an even/odd RTP/RTCP pair from `rtp_port_range`. The default range, 30000–39999, contains 5,000 pairs per configured family. File, tone, bridge and WebSocket endpoints consume no RTP ports.

An IPv4 PCMU source at 50 packets/s records approximately 11.5 KB/s before codec descriptors and RTCP: 160 payload bytes + 12 RTP + 42 Ethernet/IP/UDP + 16 PCAP record header. Multiply by recorded sources and retention time. Packet sizes, IPv6, Opus bitrate, overlapping recordings and filesystem overhead change this figure.

Monitor `dropped_packets` when stopping recordings. A recording stop response marks the session's stop boundary; it does not prove the writer has flushed. A download of an active recording captures its opened-handle length and can end with a partial PCAP record. Stop and allow flushing before exporting a finalized recording.

## Measurement

Run the benchmark with the same native libraries and release profile used in deployment:

```bash
cargo bench --bench mixing_bench
```

The harness compares shared decoding with the retained per-destination decode path in the same executable for 2, 3, 10 and 20 participants using PCMU, G.722, Opus and mixed codecs. Its two-participant case exercises the PCM path for comparison; an ordinary same-codec two-party call uses passthrough. This microbenchmark excludes network, encryption, session scheduling and storage and is not a full server capacity test.

Use `rtpbridge_audio_packets_decoded_total` to check decoder work under live fanout. Measure CPU, RSS, allocations, media-loop delay, packet loss, playout drops, recording drops, and output continuity. Run the [soak harness](https://github.com/zyno-io/rtpbridge/tree/main/e2e/soak50#readme) for lifecycle and browser/TURN coverage, including transfers and renegotiation.

A measured comparison against `7614703`, including its workload limits and uncertainty, is available in the [remediation results](../security-performance-remediation-results.md#conference-processing-measurements). It measures codec/mixer processing, not a production scheduling or capacity limit.
