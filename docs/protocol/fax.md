# Fax Tone Detection

Fax detection monitors an endpoint's incoming audio for the two in-band tones that flank the start of a T.30 fax call and emits an event when one is heard. It runs alongside [VAD](vad.md) — both share a single PCM decode of the inbound stream — and is independent of recording.

Detection is **notification only**: rtpbridge is a media router, so reacting to a detected fax (e.g. re-INVITE to T.38, or switching the leg to G.711 pass-through) is the controller's responsibility.

## Tones Detected

| Tone | Frequency | Source | Notes |
|------|-----------|--------|-------|
| **CNG** | 1100 Hz | Calling fax | 0.5s-on / 3s-off cadence. Detected per on-burst. |
| **CED** | 2100 Hz | Answering fax | Continuous ~2.6–4s. Also the V.25 modem answer tone, so a CED detection means "fax-or-modem answer tone heard". |

Detection uses the Goertzel algorithm over 20ms frames, probing a small band of frequencies around each nominal tone so that compliant but off-nominal transmitters are still caught (T.30 permits CNG at 1100 ±38 Hz). A tone must dominate the frame energy for ~160ms before its onset event fires (this debounces transient speech/music). Detection stays armed for the life of the detector — after a tone ends, a later occurrence (the next CNG burst, or a fresh call) fires again.

CNG/CED are narrowband pure tones that survive the voice codecs carriers use, so detection runs on the leg's **native codec** — G.711, G.722, or Opus. The endpoint's negotiated codec is decoded to PCM (shared with VAD) and analysed at its source rate. This is what makes fax-on-voice-line detection possible.

This is **tone** detection only. The subsequent T.30 fax *image* data is a high-rate modem carrier that does **not** survive wideband/lossy transcoding (G.722/Opus) or mixing — so once a tone is detected, the controller must move the call to a fax path (T.38 or G.711 pass-through) before the image phase begins.

## fax_detect.start

```json
{
  "id": "1",
  "method": "fax_detect.start",
  "params": {
    "endpoint_id": "..."
  }
}
```

| Param | Type | Description |
|-------|------|-------------|
| `endpoint_id` | string | required — endpoint to monitor |

## fax_detect.stop

```json
{"id":"2","method":"fax_detect.stop","params":{"endpoint_id":"..."}}
```

## Events

### fax.cng_detected

Emitted when a 1100 Hz CNG calling tone is detected.

```json
{"event":"fax.cng_detected","data":{"endpoint_id":"..."}}
```

### fax.ced_detected

Emitted when a 2100 Hz CED answer tone is detected.

```json
{"event":"fax.ced_detected","data":{"endpoint_id":"..."}}
```

### fax.error

Emitted when the decoder for the endpoint's codec cannot be created, so fax detection cannot run.

```json
{"event":"fax.error","data":{"endpoint_id":"...","error":"Fax detection decoder creation failed: ..."}}
```

## Typical Pattern

1. Bridge a call between two endpoints as usual.
2. `fax_detect.start` on the leg you want to watch (often the answering leg, for CED).
3. On `fax.ced_detected` (or `fax.cng_detected`), the controller decides how to proceed — e.g. renegotiate the leg to T.38 or reconfigure for G.711 fax pass-through.
4. `fax_detect.stop` once the decision is made (or leave it running; detection re-arms continuously).
