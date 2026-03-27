# Production Readiness Fixes — Implementation Plan

## Key Finding

All originally-reported P0 `panic!()` calls are inside `#[cfg(test)]` modules — **not production code**. The production paths already handle these cases with proper error returns and logging. The `session.idle_timeout` event is also already documented. This significantly reduces urgency.

---

## Step 1: Fix documentation default mismatch (P1)

**File**: `docs/protocol/file-playback.md`

Change `cache_ttl_secs` default from `0` to `300`. The code default is `fn default_cache_ttl() -> u32 { 300 }` in `src/control/protocol.rs:202`.

---

## Step 2: Make hardcoded limits configurable (P1)

### 2a. Add fields to Config

**File**: `src/config.rs`

Add to `Config` struct with serde defaults:

| Field | Type | Default | Current location |
|-------|------|---------|-----------------|
| `max_connections` | `usize` | `1000` | `control/server.rs:30` |
| `ws_ping_interval_secs` | `u64` | `30` | `control/connection.rs:50` |
| `event_channel_size` | `usize` | `256` | `control/connection.rs:44` |
| `critical_event_channel_size` | `usize` | `64` | `control/connection.rs:45` |
| `transcode_cache_size` | `usize` | `64` | `session/media_session.rs:24` |
| `max_file_download_bytes` | `u64` | `104857600` (100MB) | `playback/file_cache.rs:141` |
| `max_recording_download_bytes` | `u64` | `536870912` (512MB) | `control/server.rs:368` |

Add validation: all must be > 0 (except `max_connections` where 0 = unlimited).

### 2b. Thread config to consumption sites

- `control/server.rs` — use `config.max_connections` and `config.max_recording_download_bytes`
- `control/connection.rs` — use `config.ws_ping_interval_secs`, `config.event_channel_size`, `config.critical_event_channel_size`
- `session/media_session.rs` — use `config.transcode_cache_size`
- `playback/file_cache.rs` — store `max_download_bytes` on `FileCache`, use from config

### 2c. Update docs and example config

- `rtpbridge.toml.example` — add commented examples for new fields
- `docs/guide/configuration.md` — add to config reference table

---

## Step 3: Log silent errors (P1)

**File**: `src/playback/file_cache.rs`

- Line 216: Replace `.ok()` with `if let Err(e) = ... { warn!(...) }`
- Line 235: Same pattern for eviction removal

---

## Step 4: Documentation polish (P2)

| Fix | File |
|-----|------|
| Add `endpoint.accept_answer` response example | `docs/protocol/endpoints.md` |
| Clarify `endpoint.srtp_rekey` is RTP-only (not WebRTC) | `docs/protocol/endpoints.md` |
| Add endpoint state transition table | `docs/protocol/endpoints.md` |
| Clarify `timeout_ms` applies to URL sources only | `docs/protocol/file-playback.md` |
| Document `recording_flush_timeout_secs` impact | `docs/guide/performance.md` |

---

## Step 5: Unit tests for untested modules (P2)

| Module | Test focus |
|--------|-----------|
| `session/stats.rs` | Counter increment accuracy, `ms_since_last_received` |
| `session/endpoint_enum.rs` | `id()`, `direction()`, `state()`, `accept_answer()` dispatch |
| `session/file_poll.rs` | PCM output from playing file, finished/paused behavior |
| `session/vad_tap.rs` | `vad_start`/`vad_stop` lifecycle, `process_vad` filtering |
| `recording/recorder.rs` | Start/stop lifecycle, max limit enforcement, dead task detection |
| `playback/shared_playback.rs` | Subscribe/unsubscribe, ref counting |

---

## Step 6: Integration tests (P2)

| Test file | Scope |
|-----------|-------|
| `tests/rtcp_test.rs` | RTCP SR/RR handling in a two-party RTP session |
| `tests/reconnect_race_test.rs` | Rapid disconnect+reconnect, session.attach races, orphan events |

---

## Step 7: CI improvements (P2)

| Change | File |
|--------|------|
| Add tarpaulin coverage reporting + codecov | `.github/workflows/ci.yml` |
| Add `cargo-deny` for license/advisory/ban checks | `deny.toml` + `.github/workflows/ci.yml` |
| Pin Rust image to `rust:1.85-bookworm` | `Dockerfile` |

---

## Step 8: Optimize tokio features (P3)

**File**: `Cargo.toml`

Replace `features = ["full"]` with selective:

```toml
tokio = { version = "1", features = [
    "rt-multi-thread", "macros", "net", "time",
    "sync", "io-util", "signal", "fs",
] }
```

Verify with `cargo build && cargo test` after change.

---

## Commit Sequence

| # | Priority | Description | Status |
|---|----------|-------------|--------|
| 1 | P1 | Fix `cache_ttl_secs` default in docs (300 not 0) | DONE |
| 2 | P1 | Add configurable limit fields to Config struct | DONE |
| 3 | P1 | Thread config values to 4 consumption sites | DONE |
| 4 | P1 | Log file cache removal errors instead of `.ok()` | DONE |
| 5 | P2 | 5 documentation improvements | DONE |
| 6 | P2 | Unit tests for 4 untested modules (64 new tests) | DONE |
| 7 | P2 | Reconnect race integration tests (6 tests) | DONE |
| 8 | P2 | CI: cargo-deny, pin Rust version | DONE |
| 9 | P3 | Selective tokio features | DONE |

All steps complete. 393 unit tests + 188 integration tests = 581 total tests passing.
