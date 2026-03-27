# Recording

Recording captures decrypted RTP and RTCP packets to a PCAP file with accurate timestamps. Recordings operate at two independent layers that can run simultaneously:

- **Full session**: records all legs (all endpoints)
- **Single leg**: records one specific endpoint (e.g., for voicemail)

## recording.start

```json
{
  "id": "1",
  "method": "recording.start",
  "params": {
    "endpoint_id": null,
    "file_path": "/recordings/call-123.pcap"
  }
}
```

| Param | Type | Default | Description |
|-------|------|---------|-------------|
| `endpoint_id` | string or null | optional (default: `null`) | `null` for full-session recording, endpoint ID for single leg |
| `file_path` | string | required | Absolute path within `recording_dir`. Symlinks are resolved; the real path must remain inside the configured directory |

**Response:**
```json
{"id":"1","result":{"recording_id":"..."}}
```

## recording.stop

```json
{"id":"2","method":"recording.stop","params":{"recording_id":"..."}}
```

**Response:**
```json
{
  "id": "2",
  "result": {
    "file_path": "/recordings/call-123.pcap",
    "duration_ms": 30000,
    "packets": 1500,
    "dropped_packets": 0
  }
}
```

## PCAP Format

- Link type: Ethernet
- Each packet has synthetic Ethernet/IPv4/UDP headers
- Endpoint addresses are deterministic: `10.{(N/254)+1}.0.{(N%254)+1}:10000` where N is the endpoint index
- Timestamps are wall-clock time from when the packet was received
- Both RTP media packets and RTCP reports are captured
- Packets are post-decryption (SRTP is unwrapped before recording)
- The PCAP file can be opened directly in Wireshark

| Field | Description |
|-------|-------------|
| `file_path` | Absolute path of the written PCAP file |
| `duration_ms` | Wall-clock recording duration |
| `packets` | Total packets written to the PCAP file |
| `dropped_packets` | Packets dropped due to disk I/O backpressure (see Bounded Channel below) |

## Bounded Channel

Recording uses a bounded channel (capacity 1000 packets) between the media path and the disk write task. If disk I/O stalls, packets are dropped from the recording rather than blocking the media path. Drops are logged at warn level and reported in the `dropped_packets` field of the `recording.stop` response.
