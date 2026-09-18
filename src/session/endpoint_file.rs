use std::fs::File;
use std::path::Path;

use symphonia::core::codecs::CodecParameters;
use symphonia::core::codecs::audio::AudioDecoderOptions;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, SeekMode, SeekTo, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::units::Time;
use tokio::sync::broadcast;
use tracing::warn;

use super::endpoint::EndpointConfig;
use super::stats::EndpointStats;
use crate::control::protocol::{EndpointDirection, EndpointId, EndpointState};

/// Convert a dB gain value to a linear amplitude factor.
fn db_to_linear(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

/// Apply a linear gain factor to PCM samples with saturation clamping.
fn apply_gain(samples: &mut [i16], factor: f32) {
    for s in samples.iter_mut() {
        *s = (*s as f32 * factor).clamp(i16::MIN as f32, i16::MAX as f32) as i16;
    }
}

/// A file playback endpoint that decodes audio files to PCM.
/// Always send-only. Produces PCM samples on a timer (ptime interval).
pub struct FileEndpoint {
    pub id: EndpointId,
    pub config: EndpointConfig,
    pub state: EndpointState,
    pub stats: EndpointStats,
    pub(crate) playback_error: Option<String>,
    pub(crate) storage_admission: Option<tokio::sync::OwnedSemaphorePermit>,
    pub(crate) cache_lease: Option<crate::playback::file_cache::CacheLease>,
    pub(crate) download_cancel: tokio_util::sync::CancellationToken,

    worker: Option<PlaybackWorker>,
    reader: Option<Box<dyn symphonia::core::formats::FormatReader>>,
    decoder: Option<Box<dyn symphonia::core::codecs::audio::AudioDecoder>>,
    track_id: Option<u32>,
    sample_rate: Option<u32>,

    /// PCM sample buffer (interleaved i16, mono)
    pcm_buffer: Vec<i16>,
    /// Current read position in pcm_buffer
    pcm_pos: usize,

    /// Playback state
    pub loop_count: Option<u32>, // None = infinite, Some(0) = done
    pub start_ms: u64,
    pub paused: bool,

    /// Source path (for re-opening on loop)
    source_path: String,

    /// Linear gain factor (precomputed from dB value at creation time)
    gain_factor: f32,

    /// Whether this endpoint uses shared playback
    pub shared: bool,

    /// Shared playback subscriber (receives PCM frames from SharedPlaybackManager).
    /// Stored as the full subscriber so Drop correctly decrements the ref count.
    /// pub(crate) so media_session can take it for explicit async cleanup.
    pub(crate) shared_sub: Option<crate::playback::shared_playback::SharedPlaybackSubscriber>,
}

impl std::fmt::Debug for FileEndpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileEndpoint")
            .field("id", &self.id)
            .field("state", &self.state)
            .finish_non_exhaustive()
    }
}

type PlaybackFrame = (u64, Result<Vec<i16>, Option<String>>);
struct PlaybackWorker {
    frames: tokio::sync::mpsc::Receiver<PlaybackFrame>,
    commands: tokio::sync::mpsc::Sender<SeekRequest>,
    epoch: u64,
}
struct SeekRequest {
    position: u64,
    epoch: u64,
    reply: tokio::sync::oneshot::Sender<anyhow::Result<()>>,
}

impl FileEndpoint {
    #[allow(clippy::too_many_arguments)]
    pub async fn prepare(
        id: EndpointId,
        source: String,
        media_dir: Option<std::path::PathBuf>,
        lease: Option<crate::playback::file_cache::CacheLease>,
        shared_manager: std::sync::Arc<crate::playback::shared_playback::SharedPlaybackManager>,
        shared: bool,
        start_ms: u64,
        loop_count: Option<u32>,
        gain_db: f32,
    ) -> anyhow::Result<Self> {
        let resolved = crate::storage::run(move || {
            if lease.is_none() {
                let base = media_dir.ok_or_else(|| {
                    anyhow::anyhow!("Local file playback is disabled (no media_dir configured)")
                })?;
                let base = base.canonicalize()?;
                let path = std::path::Path::new(&source).canonicalize()?;
                anyhow::ensure!(
                    path.starts_with(base),
                    "File path is outside the allowed media directory"
                );
                anyhow::ensure!(
                    path.metadata()?.is_file(),
                    "playback input must be a regular file"
                );
                Ok((path.to_string_lossy().into_owned(), lease))
            } else {
                Ok((source, lease))
            }
        })
        .await;
        let (source, lease) = resolved?;
        if shared {
            let subscribed = shared_manager
                .subscribe_with_lease(&source, 8000, start_ms, loop_count, lease)
                .await;
            let sub = subscribed?;
            Ok(Self::new_shared(id, &source, sub, gain_db))
        } else {
            let admission = crate::storage::admit_stream()?;
            let opened = crate::storage::run(move || {
                let mut endpoint = Self::open(id, &source, start_ms, loop_count, gain_db)?;
                endpoint.cache_lease = lease;
                endpoint.storage_admission = Some(admission);
                Ok(endpoint)
            })
            .await;
            let endpoint = opened?;
            Ok(endpoint.into_worker())
        }
    }

    /// The session owns only a bounded PCM receiver. Decoder, file handle, cache
    /// lease and admission remain on the storage side through real completion.
    pub fn into_worker(mut self) -> Self {
        let (frames, receiver) = tokio::sync::mpsc::channel(4);
        let (commands, mut command_rx) = tokio::sync::mpsc::channel::<SeekRequest>(4);
        let mut outer = Self::new_buffering(self.id, 0.0);
        outer.state = EndpointState::Playing;
        outer.sample_rate = self.sample_rate;
        outer.source_path = self.source_path.clone();
        outer.gain_factor = self.gain_factor;
        outer.loop_count = self.loop_count;
        outer.start_ms = self.start_ms;
        outer.worker = Some(PlaybackWorker {
            frames: receiver,
            commands,
            epoch: 0,
        });
        let cancel = outer.download_cancel.clone();
        tokio::spawn(async move {
            let mut epoch = 0;
            let mut at_eof = false;
            loop {
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => break,
                    Some(request) = command_rx.recv() => {
                        let work = crate::storage::run(move || {
                            // EOF may have been prefetched while the session is
                            // still playing or paused. It can still request a seek.
                            let finished = self.state == EndpointState::Finished;
                            if finished { self.state = EndpointState::Playing; }
                            let result = self.seek(request.position);
                            if finished && result.is_err() { self.state = EndpointState::Finished; }
                            Ok((self, result, request))
                        });
                        let completed = tokio::time::timeout(std::time::Duration::from_secs(10), work).await;
                        match completed {
                            Ok(Ok((core, result, request))) => {
                                self = core;
                                epoch = request.epoch;
                                at_eof = false;
                                let _ = request.reply.send(result);
                            }
                            _ => { let _ = frames.send((epoch, Err(Some("playback seek failed".into())))).await; break; }
                        }
                    }
                    permit = frames.reserve(), if !at_eof => {
                        let Ok(permit) = permit else { break };
                        let work = crate::storage::run(move || {
                            let samples = self.next_pcm((self.sample_rate() / 50) as usize);
                            let error = self.playback_error.clone();
                            Ok((self, samples, error))
                        });
                        let completed = tokio::time::timeout(std::time::Duration::from_secs(10), work).await;
                        match completed {
                            Ok(Ok((core, samples, error))) => {
                                self = core;
                                match samples {
                                    Some(samples) => { permit.send((epoch, Ok(samples))); }
                                    None => {
                                        let failed = error.is_some();
                                        permit.send((epoch, Err(error)));
                                        if failed { break; }
                                        // Keep seek service until the session observes EOF.
                                        at_eof = true;
                                    }
                                }
                            }
                            _ => { permit.send((epoch, Err(Some("playback read failed".into())))); break; }
                        }
                    }
                }
            }
        });
        outer
    }

    pub fn request_seek(
        &mut self,
        position: u64,
        reply: tokio::sync::oneshot::Sender<anyhow::Result<()>>,
    ) {
        if !matches!(self.state, EndpointState::Playing | EndpointState::Paused) {
            let _ = reply.send(Err(anyhow::anyhow!("Cannot seek in current state")));
            return;
        }
        let Some(worker) = self.worker.as_mut() else {
            let result = self.seek(position);
            let _ = reply.send(result);
            return;
        };
        let epoch = worker.epoch.wrapping_add(1);
        match worker.commands.try_send(SeekRequest {
            position,
            epoch,
            reply,
        }) {
            Ok(()) => {
                worker.epoch = epoch;
            }
            Err(error) => {
                let _ = error
                    .into_inner()
                    .reply
                    .send(Err(anyhow::anyhow!("PLAYBACK_BUSY")));
            }
        }
    }

    /// Open a local audio file for playback
    pub fn open(
        id: EndpointId,
        source_path: &str,
        start_ms: u64,
        loop_count: Option<u32>,
        gain_db: f32,
    ) -> anyhow::Result<Self> {
        let (reader, decoder, track_id, sample_rate) = open_audio_file(source_path)?;

        let mut endpoint = Self {
            id,
            config: EndpointConfig {
                direction: EndpointDirection::SendOnly,
            },
            state: EndpointState::Playing,
            stats: EndpointStats::new(),
            playback_error: None,
            storage_admission: None,
            cache_lease: None,
            download_cancel: tokio_util::sync::CancellationToken::new(),
            worker: None,
            reader: Some(reader),
            decoder: Some(decoder),
            track_id: Some(track_id),
            sample_rate: Some(sample_rate),
            pcm_buffer: Vec::with_capacity(4096),
            pcm_pos: 0,
            loop_count,
            start_ms,
            paused: false,
            source_path: source_path.to_string(),
            gain_factor: db_to_linear(gain_db),
            shared: false,
            shared_sub: None,
        };

        // Seek to start position if specified
        if start_ms > 0 {
            endpoint.seek(start_ms)?;
        }

        Ok(endpoint)
    }

    /// Create a FileEndpoint in Buffering state (for async URL downloads)
    pub fn new_buffering(id: EndpointId, gain_db: f32) -> Self {
        Self {
            id,
            config: EndpointConfig {
                direction: EndpointDirection::SendOnly,
            },
            state: EndpointState::Buffering,
            stats: EndpointStats::new(),
            playback_error: None,
            storage_admission: None,
            cache_lease: None,
            download_cancel: tokio_util::sync::CancellationToken::new(),
            worker: None,
            reader: None,
            decoder: None,
            track_id: None,
            sample_rate: None,
            pcm_buffer: Vec::with_capacity(4096),
            pcm_pos: 0,
            loop_count: None,
            start_ms: 0,
            paused: false,
            source_path: String::new(),
            gain_factor: db_to_linear(gain_db),
            shared: false,
            shared_sub: None,
        }
    }

    /// Create a FileEndpoint that receives PCM from a shared playback broadcast channel.
    pub fn new_shared(
        id: EndpointId,
        source_path: &str,
        sub: crate::playback::shared_playback::SharedPlaybackSubscriber,
        gain_db: f32,
    ) -> Self {
        Self {
            id,
            config: EndpointConfig {
                direction: EndpointDirection::SendOnly,
            },
            state: EndpointState::Playing,
            stats: EndpointStats::new(),
            playback_error: None,
            storage_admission: None,
            cache_lease: None,
            download_cancel: tokio_util::sync::CancellationToken::new(),
            worker: None,
            reader: None,
            decoder: None,
            track_id: None,
            sample_rate: Some(8000),
            pcm_buffer: Vec::with_capacity(4096),
            pcm_pos: 0,
            loop_count: None,
            start_ms: 0,
            paused: false,
            source_path: source_path.to_string(),
            gain_factor: db_to_linear(gain_db),
            shared: true,
            shared_sub: Some(sub),
        }
    }

    /// Source path for this file endpoint.
    pub fn source_path(&self) -> &str {
        &self.source_path
    }

    /// Return the gain in dB (reconstructed from the linear factor).
    pub fn gain_db(&self) -> f32 {
        20.0 * self.gain_factor.log10()
    }

    /// Initialize a buffering endpoint with a downloaded file, transitioning to Playing state.
    #[allow(dead_code)] // synchronous core API; production prepares on storage workers
    pub fn initialize(
        &mut self,
        path: &str,
        start_ms: u64,
        loop_count: Option<u32>,
    ) -> anyhow::Result<()> {
        let (reader, decoder, track_id, sample_rate) = open_audio_file(path)?;
        self.reader = Some(reader);
        self.decoder = Some(decoder);
        self.track_id = Some(track_id);
        self.sample_rate = Some(sample_rate);
        self.source_path = path.to_string();
        self.start_ms = start_ms;
        self.loop_count = loop_count;
        self.state = EndpointState::Playing;

        if start_ms > 0 {
            self.seek(start_ms)?;
        }

        Ok(())
    }

    /// Read the next ptime_ms worth of PCM samples at the given target sample rate.
    /// Returns None if playback is finished, paused, or still buffering.
    pub fn next_pcm(&mut self, target_samples: usize) -> Option<Vec<i16>> {
        if self.paused
            || self.state == EndpointState::Finished
            || self.state == EndpointState::Buffering
        {
            return None;
        }

        if let Some(worker) = self.worker.as_mut() {
            while let Ok((epoch, frame)) = worker.frames.try_recv() {
                if epoch != worker.epoch {
                    continue;
                }
                match frame {
                    Ok(samples) => {
                        self.stats.record_outbound(samples.len() * 2);
                        return Some(samples);
                    }
                    Err(error) => {
                        self.playback_error = error;
                        self.state = EndpointState::Finished;
                        self.download_cancel.cancel();
                        return None;
                    }
                }
            }
            if worker.frames.is_closed() && worker.frames.is_empty() {
                self.playback_error = Some("playback worker stopped".into());
                self.state = EndpointState::Finished;
                self.download_cancel.cancel();
            }
            return None;
        }

        // Shared playback: receive PCM from broadcast channel instead of local decode
        if let Some(ref mut sub) = self.shared_sub {
            match sub.rx.try_recv() {
                Ok(pcm) => {
                    self.stats.record_outbound(pcm.len() * 2);
                    let mut samples = pcm.as_ref().clone();
                    if self.gain_factor != 1.0 {
                        apply_gain(&mut samples, self.gain_factor);
                    }
                    return Some(samples);
                }
                Err(broadcast::error::TryRecvError::Lagged(_)) => {
                    // Slow consumer — skip ahead and try again
                    match sub.rx.try_recv() {
                        Ok(pcm) => {
                            self.stats.record_outbound(pcm.len() * 2);
                            let mut samples = pcm.as_ref().clone();
                            if self.gain_factor != 1.0 {
                                apply_gain(&mut samples, self.gain_factor);
                            }
                            return Some(samples);
                        }
                        _ => return None,
                    }
                }
                Err(broadcast::error::TryRecvError::Empty) => return None,
                Err(broadcast::error::TryRecvError::Closed) => {
                    self.playback_error = sub.error();
                    self.state = EndpointState::Finished;
                    return None;
                }
            }
        }

        // Fill buffer if needed (limit iterations to prevent busy-loop on pathological files)
        const MAX_DECODE_ITERATIONS: usize = 64;
        let mut iterations = 0;
        while self.pcm_buffer.len() - self.pcm_pos < target_samples {
            iterations += 1;
            if iterations > MAX_DECODE_ITERATIONS {
                warn!(endpoint_id = %self.id, "decode loop exceeded max iterations, marking finished");
                self.state = EndpointState::Finished;
                return None;
            }
            if !self.decode_next_packet() {
                if self.playback_error.is_some() {
                    self.state = EndpointState::Finished;
                    return None;
                }
                // End of file — handle looping
                if let Some(ref mut count) = self.loop_count {
                    if *count == 0 {
                        self.state = EndpointState::Finished;
                        let mut tail = self.pcm_buffer[self.pcm_pos..].to_vec();
                        self.pcm_pos = self.pcm_buffer.len();
                        if tail.is_empty() {
                            return None;
                        }
                        // Only EOF pads a final partial output frame. Preserve
                        // every decoded sample and finish on the next poll.
                        tail.resize(target_samples, 0);
                        if self.gain_factor != 1.0 {
                            apply_gain(&mut tail, self.gain_factor);
                        }
                        self.stats.record_outbound(tail.len() * 2);
                        return Some(tail);
                    }
                    *count = count.saturating_sub(1);
                }
                let tail = self.pcm_buffer[self.pcm_pos..].to_vec();
                // Production calls this core from a bounded storage worker.
                if let Err(e) = self.rewind() {
                    warn!(endpoint_id = %self.id, error = %e, "failed to rewind for loop");
                    self.state = EndpointState::Finished;
                    return None;
                }
                self.pcm_buffer.extend_from_slice(&tail);
            }
        }

        // Extract requested samples
        let available = self.pcm_buffer.len() - self.pcm_pos;
        let take = target_samples.min(available);
        let mut samples = self.pcm_buffer[self.pcm_pos..self.pcm_pos + take].to_vec();
        self.pcm_pos += take;

        // Compact buffer periodically
        if self.pcm_pos > 8192 {
            self.pcm_buffer.drain(..self.pcm_pos);
            self.pcm_pos = 0;
        }

        if self.gain_factor != 1.0 {
            apply_gain(&mut samples, self.gain_factor);
        }

        self.stats.record_outbound(take * 2);
        Some(samples)
    }

    /// The sample rate of the decoded audio
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate.unwrap_or(8000)
    }

    /// Seek to a position in milliseconds. Only valid from Playing or Paused state.
    pub fn seek(&mut self, position_ms: u64) -> anyhow::Result<()> {
        match self.state {
            EndpointState::Playing | EndpointState::Paused => {}
            other => return Err(anyhow::anyhow!("Cannot seek in state {other:?}")),
        }
        let reader = self
            .reader
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("Reader not initialized"))?;
        let decoder = self
            .decoder
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("Decoder not initialized"))?;
        let track_id = self
            .track_id
            .ok_or_else(|| anyhow::anyhow!("Track ID not initialized"))?;
        let time = Time::try_from_secs_f64(position_ms as f64 / 1000.0)
            .ok_or_else(|| anyhow::anyhow!("Invalid seek position"))?;
        // Always reset decoder and clear buffers even if seek fails,
        // to avoid mixing stale samples from the old position.
        let result = reader.seek(
            SeekMode::Coarse,
            SeekTo::Time {
                time,
                track_id: Some(track_id),
            },
        );
        decoder.reset();
        self.pcm_buffer.clear();
        self.pcm_pos = 0;
        result?;
        Ok(())
    }

    /// Pause playback. Only valid from Playing state; idempotent on Paused.
    pub fn pause(&mut self) -> anyhow::Result<()> {
        match self.state {
            EndpointState::Paused => Ok(()),
            EndpointState::Playing => {
                self.paused = true;
                self.state = EndpointState::Paused;
                Ok(())
            }
            other => Err(anyhow::anyhow!("Cannot pause from state {other:?}")),
        }
    }

    /// Resume playback. Only valid from Paused state; idempotent on Playing.
    pub fn resume(&mut self) -> anyhow::Result<()> {
        match self.state {
            EndpointState::Playing => Ok(()),
            EndpointState::Paused => {
                self.paused = false;
                self.state = EndpointState::Playing;
                Ok(())
            }
            other => Err(anyhow::anyhow!("Cannot resume from state {other:?}")),
        }
    }

    fn rewind(&mut self) -> anyhow::Result<()> {
        // Re-open the file (symphonia seek to 0 may not work for all formats)
        let (reader, decoder, track_id, sample_rate) = open_audio_file(&self.source_path)?;
        self.reader = Some(reader);
        self.decoder = Some(decoder);
        self.track_id = Some(track_id);
        self.sample_rate = Some(sample_rate);
        self.pcm_buffer.clear();
        self.pcm_pos = 0;

        if self.start_ms > 0 {
            self.seek(self.start_ms)?;
        }
        Ok(())
    }

    /// Decode the next packet from the file. Returns false on EOF.
    fn decode_next_packet(&mut self) -> bool {
        let reader = match self.reader.as_mut() {
            Some(r) => r,
            None => return false,
        };
        let decoder = match self.decoder.as_mut() {
            Some(d) => d,
            None => return false,
        };
        let track_id = match self.track_id {
            Some(t) => t,
            None => return false,
        };
        for _ in 0..64 {
            let packet = match reader.next_packet() {
                Ok(Some(p)) => p,
                Ok(None) => return false, // EOF
                Err(symphonia::core::errors::Error::IoError(ref e))
                    if e.kind() == std::io::ErrorKind::UnexpectedEof =>
                {
                    return false;
                }
                Err(_) => return false,
            };

            if packet.track_id != track_id {
                continue;
            }

            match decoder.decode(&packet) {
                Ok(audio_buf) => {
                    let n_frames = audio_buf.frames();
                    let ch = audio_buf.spec().channels().count();

                    if n_frames == 0 {
                        continue;
                    }

                    if ch == 0 || ch > 8 || n_frames > 384_000 {
                        self.playback_error =
                            Some("decoded audio block exceeds resource limit".into());
                        return false;
                    }
                    let mut samples: Vec<i16> = Vec::with_capacity(n_frames * ch);
                    audio_buf.copy_to_vec_interleaved(&mut samples);

                    // Convert to mono if stereo
                    if ch > 1 {
                        for frame in 0..n_frames {
                            let mut sum: i32 = 0;
                            for c in 0..ch {
                                sum += samples[frame * ch + c] as i32;
                            }
                            let mono =
                                (sum / ch as i32).clamp(i16::MIN as i32, i16::MAX as i32) as i16;
                            self.pcm_buffer.push(mono);
                        }
                    } else {
                        self.pcm_buffer.extend_from_slice(&samples);
                    }

                    return true;
                }
                Err(_) => continue,
            }
        }
        self.playback_error = Some("audio decode work limit exceeded".into());
        false
    }
}

/// Return type for `open_audio_file`: (format_reader, decoder, track_id, sample_rate)
type AudioFileComponents = (
    Box<dyn symphonia::core::formats::FormatReader>,
    Box<dyn symphonia::core::codecs::audio::AudioDecoder>,
    u32,
    u32,
);

/// Codec registry with all default symphonia codecs plus Opus (via libopus).
fn get_codecs() -> &'static symphonia::core::codecs::registry::CodecRegistry {
    use std::sync::LazyLock;
    use symphonia::core::codecs::registry::CodecRegistry;
    static REGISTRY: LazyLock<CodecRegistry> = LazyLock::new(|| {
        let mut registry = CodecRegistry::new();
        symphonia::default::register_enabled_codecs(&mut registry);
        registry.register_audio_decoder::<symphonia_adapter_libopus::OpusDecoder>();
        registry
    });
    &REGISTRY
}

/// Open a local audio file for symphonia decode
fn open_audio_file(path: &str) -> anyhow::Result<AudioFileComponents> {
    // NONBLOCK prevents named pipes from hanging even if a local path is swapped.
    let file: File = rustix::fs::open(
        path,
        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::CLOEXEC | rustix::fs::OFlags::NONBLOCK,
        rustix::fs::Mode::empty(),
    )?
    .into();
    if !file.metadata()?.is_file() {
        anyhow::bail!("playback input must be a regular file");
    }
    let mss = MediaSourceStream::new(Box::new(file), Default::default());

    let mut hint = Hint::new();
    if let Some(ext) = Path::new(path).extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }

    let reader = symphonia::default::get_probe().probe(
        &hint,
        mss,
        FormatOptions::default(),
        MetadataOptions::default(),
    )?;

    let track = reader
        .first_track_known_codec(TrackType::Audio)
        .ok_or_else(|| anyhow::anyhow!("No audio track found"))?;

    let track_id = track.id;
    let audio_params = match track.codec_params.as_ref() {
        Some(CodecParameters::Audio(params)) => params,
        _ => return Err(anyhow::anyhow!("No audio codec parameters")),
    };
    let sample_rate = audio_params
        .sample_rate
        .ok_or_else(|| anyhow::anyhow!("Unknown sample rate"))?;

    anyhow::ensure!(
        (8000..=192000).contains(&sample_rate),
        "unsupported file sample rate"
    );
    anyhow::ensure!(
        audio_params
            .channels
            .as_ref()
            .is_none_or(|channels| (1..=8).contains(&channels.count())),
        "unsupported file channel count"
    );
    anyhow::ensure!(
        audio_params
            .max_frames_per_packet
            .is_none_or(|frames| frames <= 384000),
        "audio packet exceeds decode limit"
    );
    let decoder = get_codecs().make_audio_decoder(audio_params, &AudioDecoderOptions::default())?;

    Ok((reader, decoder, track_id, sample_rate))
}

impl Drop for FileEndpoint {
    fn drop(&mut self) {
        self.download_cancel.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn short_file_tail_is_preserved_across_loops_and_padded_only_at_eof() {
        for (duration, loops) in [(0.005, 3), (0.025, 1)] {
            let file = tempfile::NamedTempFile::new().unwrap();
            let path = file.path().to_str().unwrap();
            test_wav(path, duration);
            let mut endpoint = FileEndpoint::new_buffering(uuid::Uuid::new_v4(), 0.0);
            endpoint.initialize(path, 0, Some(loops)).unwrap();
            let samples_per_pass = (duration * 8000.0) as usize;
            let mut expected = Vec::new();
            for _ in 0..=loops {
                expected.extend((0..samples_per_pass).map(|i| {
                    (f64::sin(2.0 * std::f64::consts::PI * 440.0 * i as f64 / 8000.0) * 16000.0)
                        as i16
                }));
            }
            expected.resize(expected.len().div_ceil(160) * 160, 0);
            let mut actual = Vec::new();
            while let Some(frame) = endpoint.next_pcm(160) {
                actual.extend(frame);
                assert!(actual.len() <= expected.len());
            }
            assert_eq!(actual, expected, "duration={duration}, loops={loops}");
        }
    }

    #[tokio::test]
    async fn double_check_seek_before_prefetched_eof_is_consumed() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let path = file.path().to_str().unwrap();
        test_wav(path, 0.04);
        let budget = std::sync::Arc::new(tokio::sync::Semaphore::new(1));
        let mut core = FileEndpoint::open(uuid::Uuid::new_v4(), path, 0, Some(0), 0.0).unwrap();
        core.storage_admission = Some(budget.clone().try_acquire_owned().unwrap());
        let mut endpoint = core.into_worker();
        let prefetched = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while endpoint.worker.as_ref().unwrap().frames.len() < 3 {
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }
        })
        .await;
        prefetched.unwrap();
        endpoint.pause().unwrap();
        let (reply, response) = tokio::sync::oneshot::channel();
        endpoint.request_seek(0, reply);
        let result = response.await;
        result
            .unwrap()
            .expect("prefetching EOF cannot end a still-live endpoint's seek service");
        endpoint.resume().unwrap();
        let mut samples = Vec::new();
        let finished = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while endpoint.state != EndpointState::Finished {
                if let Some(frame) = endpoint.next_pcm(160) {
                    samples.extend(frame);
                }
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }
            while budget.available_permits() != 1 {
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }
        })
        .await;
        finished.unwrap();
        assert_eq!(
            samples.len(),
            320,
            "only the two frames after the seek are emitted"
        );
        assert!(endpoint.playback_error.is_none());
    }

    /// Generate a minimal WAV file with a sine tone
    fn test_wav(path: &str, duration_secs: f64) {
        let sample_rate: u32 = 8000;
        let num_samples = (sample_rate as f64 * duration_secs) as usize;
        let data_size = (num_samples * 2) as u32;

        let mut f = std::fs::File::create(path).unwrap();
        f.write_all(b"RIFF").unwrap();
        f.write_all(&(36 + data_size).to_le_bytes()).unwrap();
        f.write_all(b"WAVE").unwrap();
        f.write_all(b"fmt ").unwrap();
        f.write_all(&16u32.to_le_bytes()).unwrap();
        f.write_all(&1u16.to_le_bytes()).unwrap(); // PCM
        f.write_all(&1u16.to_le_bytes()).unwrap(); // mono
        f.write_all(&sample_rate.to_le_bytes()).unwrap();
        f.write_all(&(sample_rate * 2).to_le_bytes()).unwrap();
        f.write_all(&2u16.to_le_bytes()).unwrap();
        f.write_all(&16u16.to_le_bytes()).unwrap();
        f.write_all(b"data").unwrap();
        f.write_all(&data_size.to_le_bytes()).unwrap();
        for i in 0..num_samples {
            let t = i as f64 / sample_rate as f64;
            let sample = (f64::sin(2.0 * std::f64::consts::PI * 440.0 * t) * 16000.0) as i16;
            f.write_all(&sample.to_le_bytes()).unwrap();
        }
    }

    #[test]
    fn test_file_endpoint_buffering_to_playing() {
        let id = uuid::Uuid::new_v4();
        let mut ep = FileEndpoint::new_buffering(id, 0.0);
        assert_eq!(ep.state, EndpointState::Buffering);

        // While buffering, next_pcm should return None
        assert!(ep.next_pcm(160).is_none());

        // Initialize with an actual file
        let path = "/tmp/rtpbridge-test-buf2play.wav";
        test_wav(path, 0.5);
        ep.initialize(path, 0, None).unwrap();

        assert_eq!(ep.state, EndpointState::Playing);
        let pcm = ep.next_pcm(160);
        assert!(pcm.is_some(), "should produce PCM after initialization");
        assert_eq!(pcm.unwrap().len(), 160);

        std::fs::remove_file(path).ok();
    }

    #[test]
    fn test_file_endpoint_pause_resume() {
        let path = "/tmp/rtpbridge-test-pause.wav";
        test_wav(path, 1.0);
        let mut ep = FileEndpoint::open(uuid::Uuid::new_v4(), path, 0, None, 0.0).unwrap();

        // Should produce PCM initially
        assert!(ep.next_pcm(160).is_some());

        // Pause — should return None
        ep.pause().unwrap();
        assert_eq!(ep.state, EndpointState::Paused);
        assert!(ep.next_pcm(160).is_none());

        // Resume — should produce PCM again
        ep.resume().unwrap();
        assert_eq!(ep.state, EndpointState::Playing);
        assert!(ep.next_pcm(160).is_some());

        std::fs::remove_file(path).ok();
    }

    #[test]
    fn test_file_endpoint_loop_count() {
        let path = "/tmp/rtpbridge-test-loop.wav";
        test_wav(path, 0.1); // very short file

        // loop_count = Some(0) means play once then finish
        let mut ep = FileEndpoint::open(uuid::Uuid::new_v4(), path, 0, Some(0), 0.0).unwrap();

        // Drain all PCM until finished
        let mut total_samples = 0usize;
        loop {
            match ep.next_pcm(160) {
                Some(pcm) => total_samples += pcm.len(),
                None => break,
            }
            if total_samples > 100_000 {
                panic!("loop didn't terminate — too many samples");
            }
        }

        assert_eq!(ep.state, EndpointState::Finished);
        assert!(total_samples > 0, "should have produced some samples");

        std::fs::remove_file(path).ok();
    }

    #[test]
    fn test_file_endpoint_seek() {
        let path = "/tmp/rtpbridge-test-seek.wav";
        test_wav(path, 1.0); // 1 second
        let mut ep = FileEndpoint::open(uuid::Uuid::new_v4(), path, 0, None, 0.0).unwrap();

        // Read some samples from the beginning
        let first = ep.next_pcm(160).unwrap();
        assert_eq!(first.len(), 160);

        // Seek to 500ms
        ep.seek(500).unwrap();

        // Buffer should be cleared (pcm_pos reset)
        let after_seek = ep.next_pcm(160).unwrap();
        assert_eq!(after_seek.len(), 160);
        // The samples should be different (500ms of 440Hz ≈ 220 full cycles)
        // We can't easily compare exact values, but the seek should not panic
        // and should produce valid samples

        std::fs::remove_file(path).ok();
    }

    #[test]
    fn test_open_nonexistent_file() {
        let result = FileEndpoint::open(
            uuid::Uuid::new_v4(),
            "/tmp/rtpbridge-nonexistent-file.wav",
            0,
            None,
            0.0,
        );
        assert!(
            result.is_err(),
            "opening a nonexistent file should return Err"
        );
    }

    #[test]
    fn test_open_invalid_format() {
        let path = "/tmp/rtpbridge-test-invalid.wav";
        std::fs::write(path, b"this is not audio").unwrap();
        let result = FileEndpoint::open(uuid::Uuid::new_v4(), path, 0, None, 0.0);
        assert!(
            result.is_err(),
            "opening a file with garbage content should return Err"
        );
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn test_open_empty_file() {
        let path = "/tmp/rtpbridge-test-empty.wav";
        std::fs::File::create(path).unwrap();
        let result = FileEndpoint::open(uuid::Uuid::new_v4(), path, 0, None, 0.0);
        assert!(result.is_err(), "opening an empty file should return Err");
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn test_loop_count_one() {
        let path = "/tmp/rtpbridge-test-loop1.wav";
        test_wav(path, 0.1); // 0.1s = 800 samples at 8kHz

        // loop_count=Some(1) means play once, then loop once = 2 total passes
        let mut ep = FileEndpoint::open(uuid::Uuid::new_v4(), path, 0, Some(1), 0.0).unwrap();

        let mut total_samples = 0usize;
        loop {
            match ep.next_pcm(160) {
                Some(pcm) => total_samples += pcm.len(),
                None => break,
            }
            if total_samples > 100_000 {
                panic!("loop didn't terminate — too many samples");
            }
        }

        assert_eq!(ep.state, EndpointState::Finished);
        // 0.1s at 8kHz = 800 samples per pass, 2 passes = ~1600 total
        // Allow some tolerance for decoder framing
        assert!(
            total_samples >= 1400 && total_samples <= 2000,
            "expected ~1600 total samples (2 passes of ~800), got {total_samples}"
        );

        std::fs::remove_file(path).ok();
    }

    #[test]
    fn test_open_corrupted_truncated_wav() {
        // A WAV file with valid RIFF header but truncated audio data
        let path = "/tmp/rtpbridge-test-truncated.wav";
        {
            let mut f = std::fs::File::create(path).unwrap();
            // Write a valid WAV header claiming 8000 bytes of data, but only write 10
            f.write_all(b"RIFF").unwrap();
            let data_size: u32 = 8000;
            f.write_all(&(36 + data_size).to_le_bytes()).unwrap();
            f.write_all(b"WAVE").unwrap();
            f.write_all(b"fmt ").unwrap();
            f.write_all(&16u32.to_le_bytes()).unwrap();
            f.write_all(&1u16.to_le_bytes()).unwrap(); // PCM
            f.write_all(&1u16.to_le_bytes()).unwrap(); // mono
            f.write_all(&8000u32.to_le_bytes()).unwrap();
            f.write_all(&16000u32.to_le_bytes()).unwrap();
            f.write_all(&2u16.to_le_bytes()).unwrap();
            f.write_all(&16u16.to_le_bytes()).unwrap();
            f.write_all(b"data").unwrap();
            f.write_all(&data_size.to_le_bytes()).unwrap();
            // Only write 10 bytes instead of 8000
            f.write_all(&[0x00; 10]).unwrap();
        }

        // Symphonia should open the file (header is valid), but decoding should
        // hit EOF early. The endpoint should transition to Finished gracefully.
        let result = FileEndpoint::open(uuid::Uuid::new_v4(), path, 0, Some(0), 0.0);
        match result {
            Ok(mut ep) => {
                // Drain all samples — should finish without panic
                let mut total = 0;
                loop {
                    match ep.next_pcm(160) {
                        Some(pcm) => total += pcm.len(),
                        None => break,
                    }
                    if total > 100_000 {
                        panic!("truncated file should finish, not loop forever");
                    }
                }
                assert_eq!(ep.state, EndpointState::Finished);
            }
            Err(_) => {
                // Also acceptable: symphonia rejects the truncated file upfront
            }
        }

        std::fs::remove_file(path).ok();
    }

    #[test]
    fn test_open_non_audio_file() {
        // A file that is clearly not audio (e.g., a text file with .ogg extension)
        // should fail gracefully, not panic.
        let path = "/tmp/rtpbridge-test-noaudio.ogg";
        std::fs::write(
            path,
            b"This is definitely not an OGG audio file.\nJust plain text.",
        )
        .unwrap();
        let result = FileEndpoint::open(uuid::Uuid::new_v4(), path, 0, None, 0.0);
        assert!(
            result.is_err(),
            "opening a non-audio file should return Err"
        );
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn test_open_binary_garbage_file() {
        // Random binary data with a .wav extension — symphonia should reject it
        let path = "/tmp/rtpbridge-test-garbage.wav";
        let garbage: Vec<u8> = (0..256).map(|i| (i * 37 % 256) as u8).collect();
        std::fs::write(path, &garbage).unwrap();
        let result = FileEndpoint::open(uuid::Uuid::new_v4(), path, 0, None, 0.0);
        assert!(
            result.is_err(),
            "opening garbage binary data should return Err"
        );
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn test_infinite_loop_produces_continuous_output() {
        let path = "/tmp/rtpbridge-test-infloop.wav";
        test_wav(path, 0.1); // 0.1s file

        // loop_count=None means infinite looping
        let mut ep = FileEndpoint::open(uuid::Uuid::new_v4(), path, 0, None, 0.0).unwrap();

        // Read 50 chunks of 160 samples = 8000 samples = 1 second of audio
        // This is 10x the file length, so loops must be working
        for i in 0..50 {
            let pcm = ep.next_pcm(160);
            assert!(
                pcm.is_some(),
                "chunk {i} should produce samples with infinite loop"
            );
            assert_eq!(pcm.unwrap().len(), 160);
        }

        // Endpoint should still be playing, not finished
        assert_eq!(ep.state, EndpointState::Playing);

        std::fs::remove_file(path).ok();
    }
}
