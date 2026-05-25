//! AV-audio channel handler.
//!
//! Three audio channels live on this AV-audio dispatch path:
//!
//! | Channel | Name         | Direction | Spec                       |
//! |---------|--------------|-----------|----------------------------|
//! | 4       | Media Audio  | us -> car | 48 kHz stereo 16-bit PCM   |
//! | 5       | Speech Audio | car -> us | 16 kHz stereo 16-bit PCM   |
//! | 6       | System Audio | car -> us | 48 kHz stereo 16-bit PCM   |
//!
//! All three share `AVChannelMessage::Enum`:
//!   - 0x8000 SETUP_REQUEST       (car -> us): `AVChannelSetupRequest{config_index}`
//!   - 0x8003 SETUP_RESPONSE      (us -> car): `AVChannelSetupResponse{OK, max_unacked, configs}`
//!   - 0x8001 START_INDICATION    (car -> us): start streaming on `config`
//!   - 0x8002 STOP_INDICATION     (car -> us): stop streaming
//!   - 0x8004 AV_MEDIA_ACK        (car -> us, on output): per-buffer ack
//!   - 0x8005 AV_INPUT_OPEN_REQ   (car -> us, on input): car opens the mic
//!   - 0x8006 AV_INPUT_OPEN_RESP  (us -> car): ack mic open
//!   - 0x0000 AV_MEDIA_WITH_TS    : data plane (8-byte BE microsecond timestamp + audio payload)
//!   - 0x0001 AV_MEDIA            : data plane (untimed; rarely used in practice)
//!
//! ## Phase 1 deliverable — protocol-portable infrastructure
//!
//! This module is the protocol-portable layer that sits between AAP wire
//! frames and a concrete audio backend. It defines:
//!
//!   - [`AudioFormat`] / [`AudioFrame`] — neutral PCM types.
//!   - [`AudioSink`] / [`AudioSource`] traits — replaceable backends.
//!   - [`LoggingAudioSink`] — writes inbound PCM frames to disk for offline
//!     analysis. Default sink for the speech-in / system-in directions.
//!   - [`TestToneSource`] — emits a 440 Hz sine wave at the channel's sample
//!     rate. Default source for media-out so the car gets non-silent audio.
//!   - [`MediaInboundDecoder`] / [`MediaOutboundEncoder`] — strip / prepend
//!     the 8-byte microsecond timestamp on data-plane frames.
//!   - [`AudioChannelManager`] — owns per-channel session state; consumes
//!     setup/start/stop/open messages and routes data-plane frames to the
//!     right sink. Spawns the outbound encoder task for media-out on START.
//!
//! ### AAC: deliberately deferred
//!
//! Real Android Auto wraps the per-channel PCM in AAC-LC LATM frames. We
//! advertise PCM-compatible configs in `services.rs` (sample_rate / bit_depth /
//! channel_count) and the spec lets a head unit accept that; in practice cars
//! still expect AAC-LATM and our `LoggingAudioSink` would just be saving
//! bytes the OS audio framework can't replay.
//!
//! We deliberately ship **raw PCM in the data-plane payload** for Phase 1 and
//! mark this as a known wire-level deviation. Cross-compiling `fdk-aac` (the
//! best-in-class encoder) to `aarch64-linux-android` requires a vendored
//! static lib in the NDK sysroot, which isn't worth the build-pipeline churn
//! before the Android-side audio integration in Phase 2 forces our hand.
//! When Phase 2 lands and we have a real `AudioRecord` / `AudioTrack` source,
//! that's the right moment to also bolt in fdk-aac (or libfdk's
//! `cAACEncoder` invoked through JNI on the Kotlin side).
//!
//! Until then, [`LoggingAudioSink`] dumps `.bin` files of the form:
//!   `[u64 BE timestamp_us][u32 BE payload_len][payload bytes]…`
//! which is trivially parsed by any wire-analysis tool (Wireshark, Python).
//!
//! ## Phase 2 — Android audio integration (not implemented here)
//!
//! The next phase replaces [`LoggingAudioSink`] / [`TestToneSource`] with
//! Android backends. Sketch — DO NOT implement in this turn:
//!
//!   - **Media-out (channel 4)**: tap the Android mix. Options:
//!       a) `AudioPlaybackCaptureConfiguration` on API 29+ — captures
//!          opted-in apps' playback. Most music apps don't opt in.
//!       b) Virtual audio device (`AUDIO_DEVICE_OUT_REMOTE_SUBMIX`) that
//!          Android routes all media output to — clean but requires AOSP
//!          audio_policy_configuration.xml edits in our CM5 build.
//!       Either way the bridge is JNI: Kotlin reads PCM out of a buffer,
//!       feeds it into a Rust `AudioSource` impl via a JNI ringbuffer.
//!
//!   - **Speech-in (channel 5)**: the car-mic PCM we receive must become an
//!       Android *input* device. The clean path is a virtual mic via
//!       `AUDIO_DEVICE_IN_REMOTE_SUBMIX` so Android voice-assistant pipelines
//!       (e.g. Google Assistant, OEM) can `AudioRecord` from it. Our Rust
//!       `AudioSink` impl would push frames into the audio HAL's submix
//!       endpoint via JNI.
//!
//!   - **System-in (channel 6)**: car-side dings/prompts; route into the
//!       Android *music* stream (`AudioTrack` with usage =
//!       `USAGE_ASSISTANCE_SONIFICATION`). Less critical — for Phase 2
//!       backlog, can keep using `LoggingAudioSink` until we have full
//!       integration.
//!
//!   - **AudioFocus**: the AAP-level `AudioFocusRequest` we already answer
//!       with `GAIN` is a protocol-level signal between source and sink. The
//!       Android-side `AudioManager.requestAudioFocus` is independent — the
//!       Phase 2 JNI bridge needs to acquire/release Android focus in
//!       lockstep with the AAP focus answers, so apps on AABox pause when
//!       the car takes focus and resume when it gives it back.

use super::OutboundMessage;
use aabox_common::ChannelId;
use aabox_proto::enums::av_channel_setup_status;
use aabox_proto::messages::{
    AvChannelSetupRequest, AvChannelSetupResponse, AvChannelStartIndication,
    AvChannelStopIndication, AvInputOpenRequest, AvInputOpenResponse, AvMediaAckIndication,
};
use anyhow::{anyhow, Context, Result};
use bytes::{BufMut, Bytes, BytesMut};
use prost::Message;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{mpsc, Mutex};

pub const MSG_AV_MEDIA_WITH_TIMESTAMP_INDICATION: u16 = 0x0000;
pub const MSG_AV_MEDIA_INDICATION: u16 = 0x0001;
pub const MSG_SETUP_REQUEST: u16 = 0x8000;
pub const MSG_START_INDICATION: u16 = 0x8001;
pub const MSG_STOP_INDICATION: u16 = 0x8002;
pub const MSG_SETUP_RESPONSE: u16 = 0x8003;
pub const MSG_AV_MEDIA_ACK_INDICATION: u16 = 0x8004;
pub const MSG_AV_INPUT_OPEN_REQUEST: u16 = 0x8005;
pub const MSG_AV_INPUT_OPEN_RESPONSE: u16 = 0x8006;

/// Logical direction of an audio channel from our perspective (the source
/// daemon). `Output` = we generate audio for the car. `Input` = car generates
/// audio for us.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioDirection {
    /// Media audio: we (source daemon) produce PCM and ship it to the car.
    Output,
    /// Speech / system audio: the car produces PCM and ships it to us.
    Input,
}

/// PCM format descriptor — what the channel was negotiated to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioFormat {
    pub sample_rate: u32,
    pub channels: u16,
    pub bit_depth: u16,
}

impl AudioFormat {
    pub const fn new(sample_rate: u32, channels: u16, bit_depth: u16) -> Self {
        Self { sample_rate, channels, bit_depth }
    }

    pub const fn bytes_per_sample(&self) -> usize {
        (self.bit_depth as usize / 8) * self.channels as usize
    }
}

/// One decoded PCM frame: a timestamp plus the raw interleaved samples.
#[derive(Debug, Clone)]
pub struct AudioFrame {
    /// Microseconds since the AAP stream started (matches AAP's u64 BE wire
    /// timestamp on AV_MEDIA_WITH_TIMESTAMP_INDICATION).
    pub timestamp_us: u64,
    /// Raw interleaved PCM samples (little-endian, host order — Android +
    /// every common AA car runs little-endian ARM/x86).
    pub pcm: Bytes,
}

/// Default per-channel descriptor. The numbers come from `services.rs`'s
/// advertised configs — keep them in sync.
pub fn default_format(channel: ChannelId) -> Option<AudioFormat> {
    Some(match channel {
        ChannelId::MediaAudio => AudioFormat::new(48_000, 2, 16),
        ChannelId::SpeechAudio => AudioFormat::new(16_000, 2, 16),
        ChannelId::SystemAudio => AudioFormat::new(48_000, 2, 16),
        _ => return None,
    })
}

/// Convenience: classify a channel's audio direction (or `None` if not an
/// audio channel).
pub fn direction_of(channel: ChannelId) -> Option<AudioDirection> {
    Some(match channel {
        ChannelId::MediaAudio => AudioDirection::Output,
        ChannelId::SpeechAudio | ChannelId::SystemAudio => AudioDirection::Input,
        _ => return None,
    })
}

// -------------------------------------------------------------------------
// Sink / source traits
// -------------------------------------------------------------------------

/// Receives decoded PCM frames coming *from* the car (speech / system audio).
#[async_trait::async_trait]
pub trait AudioSink: Send + Sync {
    /// Push one PCM frame. Implementations should not block the caller for
    /// long; if they need to do real I/O, spawn it on a background task.
    async fn push(&self, frame: AudioFrame) -> Result<()>;

    /// Optional flush hook called on STOP_INDICATION. Default is a no-op.
    async fn flush(&self) -> Result<()> {
        Ok(())
    }
}

/// Emits PCM frames *to* the car (media audio).
#[async_trait::async_trait]
pub trait AudioSource: Send + Sync {
    /// Pull the next frame. Returning `Ok(None)` signals end-of-stream and
    /// the encoder task will exit cleanly.
    async fn next(&mut self) -> Result<Option<AudioFrame>>;
}

// -------------------------------------------------------------------------
// Concrete sink: log-to-file
// -------------------------------------------------------------------------

/// Writes every received frame to a binary file as
///   `[u64 BE timestamp_us][u32 BE payload_len][payload bytes]…`
///
/// Default path is `/data/local/tmp/aabox-audio-{channel}.bin` (matches the
/// task prompt). Override via [`LoggingAudioSink::with_path`] for tests.
pub struct LoggingAudioSink {
    /// `tokio::sync::Mutex` (not `std::sync::Mutex`) because the write
    /// happens inside an `async fn` and we may await across the lock.
    inner: Arc<Mutex<LoggingAudioSinkInner>>,
}

struct LoggingAudioSinkInner {
    path: PathBuf,
    file: Option<tokio::fs::File>,
    /// Total decoded payload bytes written. Handy for tests + smoke logs.
    bytes_written: u64,
    /// Number of frames written. Tests assert on this.
    frames_written: u64,
}

impl LoggingAudioSink {
    /// Build a sink that writes to `/data/local/tmp/aabox-audio-{channel}.bin`.
    pub fn for_channel(channel: ChannelId) -> Self {
        let path = PathBuf::from(format!(
            "/data/local/tmp/aabox-audio-{}.bin",
            channel_log_tag(channel)
        ));
        Self::with_path(path)
    }

    pub fn with_path(path: impl Into<PathBuf>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(LoggingAudioSinkInner {
                path: path.into(),
                file: None,
                bytes_written: 0,
                frames_written: 0,
            })),
        }
    }

    pub async fn frames_written(&self) -> u64 {
        self.inner.lock().await.frames_written
    }

    pub async fn bytes_written(&self) -> u64 {
        self.inner.lock().await.bytes_written
    }

    pub async fn path(&self) -> PathBuf {
        self.inner.lock().await.path.clone()
    }
}

fn channel_log_tag(channel: ChannelId) -> &'static str {
    match channel {
        ChannelId::MediaAudio => "media",
        ChannelId::SpeechAudio => "speech",
        ChannelId::SystemAudio => "system",
        _ => "other",
    }
}

#[async_trait::async_trait]
impl AudioSink for LoggingAudioSink {
    async fn push(&self, frame: AudioFrame) -> Result<()> {
        use tokio::io::AsyncWriteExt;

        let mut guard = self.inner.lock().await;
        if guard.file.is_none() {
            // Lazy open. Make sure the parent dir exists; failures here are
            // surfaced rather than silently swallowed.
            if let Some(parent) = guard.path.parent() {
                if !parent.as_os_str().is_empty() {
                    tokio::fs::create_dir_all(parent)
                        .await
                        .with_context(|| {
                            format!("create_dir_all({})", parent.display())
                        })?;
                }
            }
            let f = tokio::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&guard.path)
                .await
                .with_context(|| format!("open {}", guard.path.display()))?;
            guard.file = Some(f);
        }

        let mut hdr = [0u8; 12];
        hdr[..8].copy_from_slice(&frame.timestamp_us.to_be_bytes());
        hdr[8..].copy_from_slice(&(frame.pcm.len() as u32).to_be_bytes());

        let file = guard.file.as_mut().expect("file open");
        file.write_all(&hdr).await.context("write log hdr")?;
        file.write_all(&frame.pcm).await.context("write log pcm")?;

        guard.frames_written += 1;
        guard.bytes_written += frame.pcm.len() as u64;
        Ok(())
    }

    async fn flush(&self) -> Result<()> {
        use tokio::io::AsyncWriteExt;
        let mut guard = self.inner.lock().await;
        if let Some(f) = guard.file.as_mut() {
            f.flush().await.context("flush log file")?;
        }
        Ok(())
    }
}

// -------------------------------------------------------------------------
// Concrete source: 440 Hz test tone
// -------------------------------------------------------------------------

/// Generates a continuous 440 Hz sine wave at the channel's sample rate.
/// Pacing: emits ~20 ms chunks (a common AA frame interval) without sleeping.
/// The actual rate-limiting is done by the channel's outbound mpsc backpressure
/// — for the integration test we just let the source run as fast as the
/// encoder will consume.
pub struct TestToneSource {
    format: AudioFormat,
    /// Sample index — drives the sine phase.
    sample_idx: u64,
    /// Wall-clock origin so we can compute timestamps consistently.
    started: Instant,
    /// How many samples to emit per frame. ~20 ms by default.
    samples_per_frame: u32,
    /// Stop after this many frames have been emitted; set to `u64::MAX` for
    /// an unbounded source. The integration test uses a finite count so the
    /// pump task exits cleanly.
    max_frames: u64,
    frames_emitted: u64,
}

impl TestToneSource {
    pub fn new(format: AudioFormat) -> Self {
        let samples_per_frame = (format.sample_rate / 50).max(1); // ~20 ms
        Self {
            format,
            sample_idx: 0,
            started: Instant::now(),
            samples_per_frame,
            max_frames: u64::MAX,
            frames_emitted: 0,
        }
    }

    /// Cap how many frames will be produced before `next` returns `None`.
    pub fn with_limit(mut self, frames: u64) -> Self {
        self.max_frames = frames;
        self
    }

    /// Override frame size (samples per channel per frame).
    pub fn with_frame_size(mut self, samples_per_frame: u32) -> Self {
        self.samples_per_frame = samples_per_frame.max(1);
        self
    }

    fn render_frame(&mut self) -> AudioFrame {
        const FREQ: f64 = 440.0;
        const AMP: f64 = 0.25; // -12 dB to avoid clipping
        let sr = self.format.sample_rate as f64;
        let n = self.samples_per_frame as usize;
        let ch = self.format.channels as usize;
        let mut buf = BytesMut::with_capacity(n * self.format.bytes_per_sample());
        for i in 0..n {
            let t = (self.sample_idx + i as u64) as f64 / sr;
            let sample = (AMP * (2.0 * std::f64::consts::PI * FREQ * t).sin()
                * i16::MAX as f64) as i16;
            for _ in 0..ch {
                buf.put_i16_le(sample);
            }
        }
        let ts_us = self.started.elapsed().as_micros() as u64;
        self.sample_idx += n as u64;
        AudioFrame {
            timestamp_us: ts_us,
            pcm: buf.freeze(),
        }
    }
}

#[async_trait::async_trait]
impl AudioSource for TestToneSource {
    async fn next(&mut self) -> Result<Option<AudioFrame>> {
        if self.frames_emitted >= self.max_frames {
            return Ok(None);
        }
        self.frames_emitted += 1;
        Ok(Some(self.render_frame()))
    }
}

// -------------------------------------------------------------------------
// Inbound decoder (data plane: car -> us)
// -------------------------------------------------------------------------

/// Parse one data-plane audio payload into an [`AudioFrame`].
///
/// For Phase 1 the "audio bitstream" is raw PCM — we just strip the 8-byte
/// BE microsecond timestamp prefix and the rest is the PCM. When AAC lands
/// in Phase 2, this is where the LATM parser + decoder would live.
pub struct MediaInboundDecoder {
    _format: AudioFormat,
}

impl MediaInboundDecoder {
    pub fn new(format: AudioFormat) -> Self {
        Self { _format: format }
    }

    /// `with_timestamp` = true for `AV_MEDIA_WITH_TIMESTAMP_INDICATION`
    /// (0x0000), false for the untimed `AV_MEDIA_INDICATION` (0x0001).
    pub fn decode(&self, payload: &[u8], with_timestamp: bool) -> Result<AudioFrame> {
        if with_timestamp {
            if payload.len() < 8 {
                return Err(anyhow!(
                    "audio inbound: payload too short for timestamp ({} bytes)",
                    payload.len()
                ));
            }
            let ts_us = u64::from_be_bytes(payload[..8].try_into().unwrap());
            let pcm = Bytes::copy_from_slice(&payload[8..]);
            Ok(AudioFrame { timestamp_us: ts_us, pcm })
        } else {
            Ok(AudioFrame {
                timestamp_us: 0,
                pcm: Bytes::copy_from_slice(payload),
            })
        }
    }
}

// -------------------------------------------------------------------------
// Outbound encoder (data plane: us -> car)
// -------------------------------------------------------------------------

/// Frame one [`AudioFrame`] into an `OutboundMessage` ready for the control
/// loop to encrypt and ship out. Wire shape:
///   `msg_id = 0x0000` (AV_MEDIA_WITH_TIMESTAMP_INDICATION)
///   payload = `[u64 BE timestamp_us][PCM bytes]`
pub fn encode_outbound_frame(channel: ChannelId, frame: &AudioFrame) -> OutboundMessage {
    let mut buf = BytesMut::with_capacity(8 + frame.pcm.len());
    buf.put_u64(frame.timestamp_us);
    buf.put_slice(&frame.pcm);
    OutboundMessage::new(
        channel as u8,
        MSG_AV_MEDIA_WITH_TIMESTAMP_INDICATION,
        buf.freeze(),
    )
}

/// Pull frames from an [`AudioSource`] and emit them on the supplied mpsc.
/// Spawn as a tokio task — the task exits when the source returns `None`,
/// the channel receives a stop signal, or the receiver is dropped.
pub async fn run_outbound_encoder<S: AudioSource + Send + 'static>(
    channel: ChannelId,
    mut source: S,
    out_tx: mpsc::Sender<OutboundMessage>,
    mut stop_rx: tokio::sync::watch::Receiver<bool>,
) {
    loop {
        tokio::select! {
            biased;
            changed = stop_rx.changed() => {
                if changed.is_err() || *stop_rx.borrow() {
                    tracing::debug!(channel = ?channel, "audio: outbound encoder stop signal");
                    break;
                }
            }
            next = source.next() => {
                match next {
                    Ok(Some(frame)) => {
                        let msg = encode_outbound_frame(channel, &frame);
                        if out_tx.send(msg).await.is_err() {
                            tracing::debug!(channel = ?channel, "audio: outbound rx dropped, encoder stopping");
                            break;
                        }
                    }
                    Ok(None) => {
                        tracing::debug!(channel = ?channel, "audio: source exhausted");
                        break;
                    }
                    Err(e) => {
                        tracing::warn!(channel = ?channel, "audio: source error: {e:#}");
                        break;
                    }
                }
            }
        }
    }
}

// -------------------------------------------------------------------------
// Channel manager
// -------------------------------------------------------------------------

/// Per-channel runtime state held by the manager.
struct AudioChannelState {
    direction: AudioDirection,
    format: AudioFormat,
    sink: Option<Arc<dyn AudioSink>>,
    /// Stop-signal handle for the outbound encoder task (Output direction
    /// only). `None` until START fires.
    stop_tx: Option<tokio::sync::watch::Sender<bool>>,
    /// Most recent session id seen on START / INPUT_OPEN (echoed back on
    /// AV_INPUT_OPEN_RESPONSE).
    session: i32,
    /// Decoder (Input direction) — instantiated lazily on first data frame.
    decoder: Option<MediaInboundDecoder>,
    /// True once START_INDICATION (Output) or AV_INPUT_OPEN_REQUEST (Input)
    /// has been answered — gates data-plane dispatch.
    started: bool,
}

/// Holds audio channel state across the lifetime of one control-channel
/// session. Driven by [`AudioChannelManager::handle`] from the control loop's
/// dispatch path; emits any reply messages directly + uses `out_tx` for
/// asynchronous data-plane traffic.
pub struct AudioChannelManager {
    channels: Mutex<HashMap<u8, AudioChannelState>>,
    /// mpsc to the control loop's writer half; outbound encoders push frames
    /// here and the loop serialises them onto the wire.
    out_tx: mpsc::Sender<OutboundMessage>,
    /// Factory for the source side (media-out). Default produces a
    /// `TestToneSource`. Tests can override to inject a finite source so
    /// the encoder task exits.
    source_factory: Mutex<Option<Box<dyn FnMut(AudioFormat) -> Box<dyn AudioSource + Send> + Send>>>,
    /// Factory for the sink side (speech-in / system-in). Default produces a
    /// `LoggingAudioSink` writing under `/data/local/tmp`. Tests inject a
    /// custom sink (shared `Arc`) so they can observe what landed.
    sink_factory: Mutex<Option<Box<dyn FnMut(ChannelId, AudioFormat) -> Arc<dyn AudioSink> + Send>>>,
}

impl AudioChannelManager {
    pub fn new(out_tx: mpsc::Sender<OutboundMessage>) -> Self {
        Self {
            channels: Mutex::new(HashMap::new()),
            out_tx,
            source_factory: Mutex::new(None),
            sink_factory: Mutex::new(None),
        }
    }

    /// Replace the source factory. Used by tests to inject a finite tone.
    pub async fn set_source_factory<F>(&self, f: F)
    where
        F: FnMut(AudioFormat) -> Box<dyn AudioSource + Send> + Send + 'static,
    {
        *self.source_factory.lock().await = Some(Box::new(f));
    }

    /// Replace the sink factory. Used by tests to grab a handle to the sink
    /// they expect the manager to push frames into.
    pub async fn set_sink_factory<F>(&self, f: F)
    where
        F: FnMut(ChannelId, AudioFormat) -> Arc<dyn AudioSink> + Send + 'static,
    {
        *self.sink_factory.lock().await = Some(Box::new(f));
    }

    fn make_default_source(format: AudioFormat) -> Box<dyn AudioSource + Send> {
        Box::new(TestToneSource::new(format))
    }

    fn make_default_sink(channel: ChannelId, _format: AudioFormat) -> Arc<dyn AudioSink> {
        Arc::new(LoggingAudioSink::for_channel(channel))
    }

    /// Ensure per-channel state exists. Returns `None` if the channel id
    /// isn't one of the three audio channels.
    async fn ensure_state(&self, channel: u8) -> Option<()> {
        let cid = ChannelId::try_from(channel).ok()?;
        let direction = direction_of(cid)?;
        let format = default_format(cid)?;
        let mut chans = self.channels.lock().await;
        chans.entry(channel).or_insert(AudioChannelState {
            direction,
            format,
            sink: None,
            stop_tx: None,
            session: 0,
            decoder: None,
            started: false,
        });
        Some(())
    }

    /// Handle one inbound audio-channel message. May produce zero or more
    /// reply `OutboundMessage`s (returned directly) and may also kick off
    /// background work that pushes additional messages through `out_tx`.
    pub async fn handle(
        &self,
        channel: u8,
        msg_id: u16,
        payload: &[u8],
    ) -> Result<Vec<OutboundMessage>> {
        if self.ensure_state(channel).await.is_none() {
            tracing::warn!(
                channel,
                msg_id = format!("0x{:04x}", msg_id),
                "audio: ignoring message on non-audio channel"
            );
            return Ok(vec![]);
        }
        let cid = ChannelId::try_from(channel).expect("checked above");

        match msg_id {
            MSG_SETUP_REQUEST => self.on_setup_request(cid, payload).await,
            MSG_START_INDICATION => self.on_start_indication(cid, payload).await,
            MSG_STOP_INDICATION => self.on_stop_indication(cid, payload).await,
            MSG_AV_MEDIA_ACK_INDICATION => self.on_media_ack(cid, payload).await,
            MSG_AV_INPUT_OPEN_REQUEST => self.on_input_open(cid, payload).await,
            MSG_AV_MEDIA_WITH_TIMESTAMP_INDICATION => {
                self.on_data_frame(cid, payload, true).await
            }
            MSG_AV_MEDIA_INDICATION => self.on_data_frame(cid, payload, false).await,
            other => {
                tracing::warn!(
                    channel,
                    msg_id = format!("0x{:04x}", other),
                    "audio: unhandled msg id"
                );
                Ok(vec![])
            }
        }
    }

    async fn on_setup_request(
        &self,
        cid: ChannelId,
        payload: &[u8],
    ) -> Result<Vec<OutboundMessage>> {
        let req = AvChannelSetupRequest::decode(payload)
            .map_err(|e| anyhow!("decode AVChannelSetupRequest: {e}"))?;
        tracing::info!(
            channel = cid as u8,
            config_index = req.config_index,
            "audio: SETUP_REQUEST -> OK"
        );
        let resp = AvChannelSetupResponse {
            media_status: av_channel_setup_status::Enum::Ok as i32,
            max_unacked: 10,
            configs: vec![0],
        };
        Ok(vec![encode_proto_msg(cid as u8, MSG_SETUP_RESPONSE, &resp)])
    }

    async fn on_start_indication(
        &self,
        cid: ChannelId,
        payload: &[u8],
    ) -> Result<Vec<OutboundMessage>> {
        let req = AvChannelStartIndication::decode(payload)
            .map_err(|e| anyhow!("decode AVChannelStartIndication: {e}"))?;
        tracing::info!(
            channel = cid as u8,
            session = req.session,
            config = req.config,
            "audio: START_INDICATION"
        );

        let mut chans = self.channels.lock().await;
        let st = chans.get_mut(&(cid as u8)).expect("state created earlier");
        st.session = req.session;

        match st.direction {
            AudioDirection::Output => {
                // Kick off the outbound encoder pump. Replace any previous
                // task with a fresh one (defensive in case of duplicate
                // START).
                if let Some(prev) = st.stop_tx.take() {
                    let _ = prev.send(true);
                }
                let format = st.format;
                drop(chans);

                let source: Box<dyn AudioSource + Send> = {
                    let mut factory = self.source_factory.lock().await;
                    match factory.as_mut() {
                        Some(f) => (f)(format),
                        None => Self::make_default_source(format),
                    }
                };

                let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
                {
                    let mut chans = self.channels.lock().await;
                    if let Some(st) = chans.get_mut(&(cid as u8)) {
                        st.stop_tx = Some(stop_tx);
                        st.started = true;
                    }
                }
                let out_tx = self.out_tx.clone();
                tokio::spawn(run_outbound_encoder_dyn(cid, source, out_tx, stop_rx));
            }
            AudioDirection::Input => {
                // Cars seldom send START on an input channel — the open
                // pathway is AV_INPUT_OPEN_REQUEST — but if one does, treat
                // it as "ready to receive data."
                st.started = true;
                if st.decoder.is_none() {
                    st.decoder = Some(MediaInboundDecoder::new(st.format));
                }
            }
        }
        Ok(vec![])
    }

    async fn on_stop_indication(
        &self,
        cid: ChannelId,
        payload: &[u8],
    ) -> Result<Vec<OutboundMessage>> {
        let _ = AvChannelStopIndication::decode(payload);
        tracing::info!(channel = cid as u8, "audio: STOP_INDICATION");

        let mut chans = self.channels.lock().await;
        if let Some(st) = chans.get_mut(&(cid as u8)) {
            st.started = false;
            if let Some(tx) = st.stop_tx.take() {
                let _ = tx.send(true);
            }
            if let Some(sink) = &st.sink {
                let sink = sink.clone();
                tokio::spawn(async move {
                    if let Err(e) = sink.flush().await {
                        tracing::warn!("audio: sink flush failed: {e:#}");
                    }
                });
            }
        }
        Ok(vec![])
    }

    async fn on_media_ack(
        &self,
        cid: ChannelId,
        payload: &[u8],
    ) -> Result<Vec<OutboundMessage>> {
        match AvMediaAckIndication::decode(payload) {
            Ok(a) => tracing::debug!(
                channel = cid as u8,
                session = a.session,
                value = a.value,
                "audio: media ack"
            ),
            Err(e) => tracing::warn!(
                channel = cid as u8,
                "audio: bad media ack: {e}"
            ),
        }
        Ok(vec![])
    }

    async fn on_input_open(
        &self,
        cid: ChannelId,
        payload: &[u8],
    ) -> Result<Vec<OutboundMessage>> {
        let req = AvInputOpenRequest::decode(payload)
            .map_err(|e| anyhow!("decode AVInputOpenRequest: {e}"))?;
        tracing::info!(
            channel = cid as u8,
            open = req.open,
            anc = req.anc,
            ec = req.ec,
            max_unacked = req.max_unacked,
            "audio: AV_INPUT_OPEN_REQUEST"
        );

        // On open, attach a sink and arm the decoder. Per the AVInput shape
        // the response uses session/value as a bidirectional handshake;
        // aasdk's "ack any open" semantics work for us (session=0, value=0).
        let mut chans = self.channels.lock().await;
        let st = chans
            .get_mut(&(cid as u8))
            .expect("state created in ensure_state");
        if st.sink.is_none() {
            let format = st.format;
            drop(chans);
            let sink: Arc<dyn AudioSink> = {
                let mut factory = self.sink_factory.lock().await;
                match factory.as_mut() {
                    Some(f) => (f)(cid, format),
                    None => Self::make_default_sink(cid, format),
                }
            };
            let mut chans = self.channels.lock().await;
            if let Some(st) = chans.get_mut(&(cid as u8)) {
                st.sink = Some(sink);
                st.decoder = Some(MediaInboundDecoder::new(st.format));
                st.started = req.open;
            }
        } else if let Some(st) = chans.get_mut(&(cid as u8)) {
            st.started = req.open;
            if st.decoder.is_none() {
                st.decoder = Some(MediaInboundDecoder::new(st.format));
            }
        }

        let resp = AvInputOpenResponse { session: 0, value: 0 };
        Ok(vec![encode_proto_msg(
            cid as u8,
            MSG_AV_INPUT_OPEN_RESPONSE,
            &resp,
        )])
    }

    async fn on_data_frame(
        &self,
        cid: ChannelId,
        payload: &[u8],
        with_timestamp: bool,
    ) -> Result<Vec<OutboundMessage>> {
        // Snapshot the bits we need without holding the lock across awaits.
        let (sink, decoder_fmt, session) = {
            let chans = self.channels.lock().await;
            let st = match chans.get(&(cid as u8)) {
                Some(s) => s,
                None => {
                    tracing::debug!(
                        channel = cid as u8,
                        "audio: data frame for unknown channel state"
                    );
                    return Ok(vec![]);
                }
            };
            if !st.started {
                tracing::debug!(
                    channel = cid as u8,
                    "audio: data frame before start (dropping)"
                );
                return Ok(vec![]);
            }
            let sink = match st.sink.as_ref() {
                Some(s) => s.clone(),
                None => {
                    tracing::debug!(
                        channel = cid as u8,
                        "audio: data frame with no sink attached (dropping)"
                    );
                    return Ok(vec![]);
                }
            };
            (sink, st.format, st.session)
        };

        let decoder = MediaInboundDecoder::new(decoder_fmt);
        let frame = decoder.decode(payload, with_timestamp)?;
        let frame_len = frame.pcm.len();
        if let Err(e) = sink.push(frame).await {
            tracing::warn!(channel = cid as u8, "audio: sink push failed: {e:#}");
        } else {
            tracing::trace!(
                channel = cid as u8,
                bytes = frame_len,
                "audio: dispatched inbound frame to sink"
            );
        }

        // Ack inbound buffer so the car will keep sending data. value is the
        // "buffers acked" counter — incrementing by 1 per data frame is the
        // simplest correct policy; aasdk's MediaAudioServiceChannel does
        // exactly this.
        let ack = AvMediaAckIndication {
            session,
            value: 1,
        };
        Ok(vec![encode_proto_msg(
            cid as u8,
            MSG_AV_MEDIA_ACK_INDICATION,
            &ack,
        )])
    }
}

/// Adapter so we can spawn `run_outbound_encoder` with a boxed source.
async fn run_outbound_encoder_dyn(
    channel: ChannelId,
    source: Box<dyn AudioSource + Send>,
    out_tx: mpsc::Sender<OutboundMessage>,
    stop_rx: tokio::sync::watch::Receiver<bool>,
) {
    struct BoxedSource(Box<dyn AudioSource + Send>);
    #[async_trait::async_trait]
    impl AudioSource for BoxedSource {
        async fn next(&mut self) -> Result<Option<AudioFrame>> {
            self.0.next().await
        }
    }
    run_outbound_encoder(channel, BoxedSource(source), out_tx, stop_rx).await
}

fn encode_proto_msg<M: Message>(channel: u8, msg_id: u16, m: &M) -> OutboundMessage {
    let mut buf = Vec::with_capacity(m.encoded_len());
    m.encode(&mut buf).expect("encode");
    OutboundMessage::new(channel, msg_id, buf)
}

// -------------------------------------------------------------------------
// Back-compat free-function entry point.
// -------------------------------------------------------------------------

/// Stateless ack-only fallback. Kept so callers that haven't been threaded
/// through the [`AudioChannelManager`] (and unit tests that don't care about
/// the data plane) still compile. Real dispatch goes through the manager.
pub fn handle(channel: u8, msg_id: u16, payload: &[u8]) -> Result<Vec<OutboundMessage>> {
    match msg_id {
        MSG_SETUP_REQUEST => {
            let req = AvChannelSetupRequest::decode(payload)
                .map_err(|e| anyhow!("decode AVChannelSetupRequest: {e}"))?;
            tracing::info!(
                channel,
                config_index = req.config_index,
                "av: SETUP_REQUEST -> OK (stateless fallback)"
            );
            let resp = AvChannelSetupResponse {
                media_status: av_channel_setup_status::Enum::Ok as i32,
                max_unacked: 10,
                configs: vec![0],
            };
            Ok(vec![encode_proto_msg(channel, MSG_SETUP_RESPONSE, &resp)])
        }
        MSG_START_INDICATION | MSG_STOP_INDICATION => Ok(vec![]),
        MSG_AV_MEDIA_ACK_INDICATION => Ok(vec![]),
        MSG_AV_INPUT_OPEN_REQUEST => {
            let _ = AvInputOpenRequest::decode(payload);
            let resp = AvInputOpenResponse { session: 0, value: 0 };
            Ok(vec![encode_proto_msg(
                channel,
                MSG_AV_INPUT_OPEN_RESPONSE,
                &resp,
            )])
        }
        MSG_AV_MEDIA_WITH_TIMESTAMP_INDICATION | MSG_AV_MEDIA_INDICATION => Ok(vec![]),
        _ => Ok(vec![]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aabox_common::ChannelId;

    #[test]
    fn setup_request_acks() {
        let req = AvChannelSetupRequest { config_index: 0 };
        let mut buf = Vec::new();
        req.encode(&mut buf).unwrap();
        let out = handle(ChannelId::MediaAudio as u8, MSG_SETUP_REQUEST, &buf).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].message_id, MSG_SETUP_RESPONSE);
        let resp = AvChannelSetupResponse::decode(&out[0].payload[..]).unwrap();
        assert_eq!(resp.media_status, av_channel_setup_status::Enum::Ok as i32);
    }

    #[test]
    fn input_open_acks_via_fallback() {
        let req = AvInputOpenRequest {
            open: true,
            anc: false,
            ec: false,
            max_unacked: 4,
        };
        let mut buf = Vec::new();
        req.encode(&mut buf).unwrap();
        let out = handle(
            ChannelId::SpeechAudio as u8,
            MSG_AV_INPUT_OPEN_REQUEST,
            &buf,
        )
        .unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].message_id, MSG_AV_INPUT_OPEN_RESPONSE);
    }

    #[test]
    fn inbound_decoder_strips_timestamp() {
        let fmt = AudioFormat::new(16_000, 2, 16);
        let dec = MediaInboundDecoder::new(fmt);
        let mut payload = Vec::new();
        payload.extend_from_slice(&0x1122_3344_5566_7788_u64.to_be_bytes());
        payload.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);
        let frame = dec.decode(&payload, true).unwrap();
        assert_eq!(frame.timestamp_us, 0x1122_3344_5566_7788);
        assert_eq!(&frame.pcm[..], &[0xDE, 0xAD, 0xBE, 0xEF]);
    }

    #[test]
    fn outbound_encoder_prefixes_timestamp() {
        let frame = AudioFrame {
            timestamp_us: 42,
            pcm: Bytes::from_static(&[1, 2, 3, 4]),
        };
        let msg = encode_outbound_frame(ChannelId::MediaAudio, &frame);
        assert_eq!(msg.channel, ChannelId::MediaAudio as u8);
        assert_eq!(msg.message_id, MSG_AV_MEDIA_WITH_TIMESTAMP_INDICATION);
        assert_eq!(msg.payload.len(), 12);
        assert_eq!(u64::from_be_bytes(msg.payload[..8].try_into().unwrap()), 42);
        assert_eq!(&msg.payload[8..], &[1, 2, 3, 4]);
    }

    #[tokio::test]
    async fn logging_sink_writes_frames_to_disk() {
        let dir = std::env::temp_dir().join(format!(
            "aabox-audio-sink-test-{}",
            std::process::id()
        ));
        let _ = tokio::fs::remove_dir_all(&dir).await;
        let path = dir.join("speech.bin");
        let sink = LoggingAudioSink::with_path(&path);

        for i in 0..3u64 {
            let frame = AudioFrame {
                timestamp_us: i * 1000,
                pcm: Bytes::from(vec![i as u8; 8]),
            };
            sink.push(frame).await.unwrap();
        }
        sink.flush().await.unwrap();
        assert_eq!(sink.frames_written().await, 3);
        assert_eq!(sink.bytes_written().await, 24);

        let bytes = tokio::fs::read(&path).await.unwrap();
        // 3 frames * (12 hdr + 8 payload) = 60.
        assert_eq!(bytes.len(), 60);
        // First frame: timestamp 0, len 8.
        assert_eq!(u64::from_be_bytes(bytes[0..8].try_into().unwrap()), 0);
        assert_eq!(u32::from_be_bytes(bytes[8..12].try_into().unwrap()), 8);
    }

    #[tokio::test]
    async fn test_tone_source_emits_pcm_at_format() {
        let fmt = AudioFormat::new(48_000, 2, 16);
        let mut src = TestToneSource::new(fmt)
            .with_frame_size(480) // 10 ms @ 48 kHz
            .with_limit(2);
        let f1 = src.next().await.unwrap().unwrap();
        // 480 samples * 2 channels * 2 bytes = 1920 bytes.
        assert_eq!(f1.pcm.len(), 1920);
        let f2 = src.next().await.unwrap().unwrap();
        assert!(f2.timestamp_us >= f1.timestamp_us);
        // Third call hits the limit and yields None.
        assert!(src.next().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn manager_routes_inbound_data_to_sink() {
        // Captures every pushed frame in memory so we can assert content.
        struct CaptureSink {
            frames: Arc<Mutex<Vec<AudioFrame>>>,
        }
        #[async_trait::async_trait]
        impl AudioSink for CaptureSink {
            async fn push(&self, frame: AudioFrame) -> Result<()> {
                self.frames.lock().await.push(frame);
                Ok(())
            }
        }

        let captured: Arc<Mutex<Vec<AudioFrame>>> = Arc::new(Mutex::new(Vec::new()));
        let (out_tx, _out_rx) = mpsc::channel::<OutboundMessage>(16);
        let mgr = AudioChannelManager::new(out_tx);
        let cap_clone = Arc::clone(&captured);
        mgr.set_sink_factory(move |_cid, _fmt| {
            let inner = Arc::clone(&cap_clone);
            let sink: Arc<dyn AudioSink> = Arc::new(CaptureSink { frames: inner });
            sink
        })
        .await;

        // 1) SETUP_REQUEST.
        let req = AvChannelSetupRequest { config_index: 0 };
        let mut buf = Vec::new();
        req.encode(&mut buf).unwrap();
        let out = mgr
            .handle(ChannelId::SpeechAudio as u8, MSG_SETUP_REQUEST, &buf)
            .await
            .unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].message_id, MSG_SETUP_RESPONSE);

        // 2) AV_INPUT_OPEN_REQUEST.
        let open = AvInputOpenRequest {
            open: true,
            anc: false,
            ec: false,
            max_unacked: 4,
        };
        let mut buf = Vec::new();
        open.encode(&mut buf).unwrap();
        let out = mgr
            .handle(
                ChannelId::SpeechAudio as u8,
                MSG_AV_INPUT_OPEN_REQUEST,
                &buf,
            )
            .await
            .unwrap();
        assert_eq!(out[0].message_id, MSG_AV_INPUT_OPEN_RESPONSE);

        // 3) A few timestamped data frames.
        for ts in [1_000u64, 2_000, 3_000] {
            let mut payload = Vec::new();
            payload.extend_from_slice(&ts.to_be_bytes());
            payload.extend_from_slice(&[0xAA, 0xBB, 0xCC, 0xDD]);
            let out = mgr
                .handle(
                    ChannelId::SpeechAudio as u8,
                    MSG_AV_MEDIA_WITH_TIMESTAMP_INDICATION,
                    &payload,
                )
                .await
                .unwrap();
            assert_eq!(out.len(), 1, "data frame should produce a media ack");
            assert_eq!(out[0].message_id, MSG_AV_MEDIA_ACK_INDICATION);
        }

        let frames = captured.lock().await.clone();
        assert_eq!(frames.len(), 3);
        assert_eq!(frames[0].timestamp_us, 1_000);
        assert_eq!(&frames[0].pcm[..], &[0xAA, 0xBB, 0xCC, 0xDD]);
        assert_eq!(frames[2].timestamp_us, 3_000);
    }

    #[tokio::test]
    async fn manager_runs_outbound_encoder_on_start() {
        let (out_tx, mut out_rx) = mpsc::channel::<OutboundMessage>(32);
        let mgr = AudioChannelManager::new(out_tx);

        // Inject a finite tone source so the encoder task exits cleanly.
        mgr.set_source_factory(|fmt| {
            Box::new(
                TestToneSource::new(fmt)
                    .with_frame_size(480)
                    .with_limit(5),
            )
        })
        .await;

        // SETUP first (so state exists).
        let req = AvChannelSetupRequest { config_index: 0 };
        let mut buf = Vec::new();
        req.encode(&mut buf).unwrap();
        let _ = mgr
            .handle(ChannelId::MediaAudio as u8, MSG_SETUP_REQUEST, &buf)
            .await
            .unwrap();

        // START_INDICATION kicks off the pump.
        let start = AvChannelStartIndication {
            session: 7,
            config: 0,
        };
        let mut buf = Vec::new();
        start.encode(&mut buf).unwrap();
        let _ = mgr
            .handle(ChannelId::MediaAudio as u8, MSG_START_INDICATION, &buf)
            .await
            .unwrap();

        // Collect everything the pump produced. 5 frames × 1920 byte PCM
        // payload + 8 byte timestamp = 1928 byte wire payload each.
        let mut got = Vec::new();
        for _ in 0..5 {
            let msg = tokio::time::timeout(
                std::time::Duration::from_secs(2),
                out_rx.recv(),
            )
            .await
            .expect("encoder produced no message in time")
            .expect("encoder channel closed prematurely");
            assert_eq!(msg.channel, ChannelId::MediaAudio as u8);
            assert_eq!(msg.message_id, MSG_AV_MEDIA_WITH_TIMESTAMP_INDICATION);
            assert!(msg.payload.len() >= 8 + 1920);
            got.push(msg);
        }
        assert_eq!(got.len(), 5);
    }
}
