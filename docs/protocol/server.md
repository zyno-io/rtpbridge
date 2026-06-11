# Server

## server.info

Get server-level metadata.

```json
{"id":"1","method":"server.info","params":{}}
```

**Response:**
```json
{
  "id": "1",
  "result": {
    "hostname": "rtpbridge-0",
    "media_ip": ["203.0.113.5", "2001:db8::5"]
  }
}
```

> **Wire-format note:** `media_ip` is an **array** of the configured media-plane
> bind IPs (at most one IPv4 and one IPv6). It was a scalar string before
> dual-stack support; single-stack instances return a one-element array.
