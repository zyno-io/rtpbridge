---
layout: home
hero:
  name: rtpbridge
  text: RTP Media Routing Server
  tagline: High-performance media bridge in Rust — WebRTC, RTP/SRTP, transcoding, recording, and more.
  actions:
    - theme: brand
      text: Get Started
      link: /guide/getting-started
    - theme: alt
      text: Protocol Reference
      link: /protocol/overview
    - theme: alt
      text: GitHub
      link: https://github.com/zyno-io/rtpbridge

features:
  - title: WebRTC & SIP
    details: Bridge WebRTC (DTLS/ICE/SRTP) and plain RTP/SRTP endpoints in the same session. Auto-detects endpoint type from SDP.
  - title: Codec Transcoding
    details: Transcode between PCMU (G.711), G.722, and Opus on the fly. DTMF always passes through untouched.
  - title: Recording & VAD
    details: Record to PCAP with accurate timestamps. Independent voice activity detection for IVR, voicemail, and LLM streaming.
  - title: File Playback
    details: Play WAV, MP3, OGG, FLAC from local files or URLs. Seek, pause, resume, loop. Shared decode across sessions.
  - title: WebSocket Control
    details: JSON control protocol over WebSocket. 1:1 session binding. Orphan timeout with reconnection support.
  - title: Production Ready
    details: Graceful shutdown for k8s. Split control/media interfaces. Symmetric RTP for NAT. Resource limits.
---
