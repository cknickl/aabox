//! Post-handshake control-channel message loop.
//!
//! Drives every conversation the car expects from us after the TLS tunnel is
//! up. The flow is (per AACS/AAServer, openauto reference, and an empirical
//! capture of Carlinkit↔openauto on 2026-05-24):
//!
//!   1. WAIT for HU to send `AuthComplete{status:OK}` (plaintext, control
//!      channel). The HU sends it to us; we do NOT initiate it. Sending
//!      AuthComplete from the source side confuses real HUs (KIA returns
//!      AuthComplete{status=-3} and disconnects USB).
//!   2. SEND our `ServiceDiscoveryRequest` (encrypted, control channel) with
//!      `device_name="Auto", device_brand="Android"` — these are the exact
//!      strings Carlinkit sends, observed via openauto's log. The HU then
//!      replies with its `ServiceDiscoveryResponse` listing supported
//!      channels.
//!   3. From here on, react to every inbound message:
//!        - PingRequest -> PingResponse (echo timestamp)
//!        - AudioFocusRequest -> AudioFocusResponse{GAIN}
//!        - NavigationFocusRequest -> NavigationFocusResponse{NATIVE=1}
//!        - VoiceSessionRequest -> log only
//!        - ChannelOpenRequest -> ChannelOpenResponse{OK}
//!        - any frame on a non-control channel -> dispatch to channels::*
//!      Plus, we may *emit* navigation messages from a mpsc receiver in
//!      parallel.
//!
//! Concurrency model: a single tokio task owns the I/O stream and the rustls
//! ServerConnection (TLS state is not Sync). It does a `select!` between
//! reading the next frame and pulling any outbound message off an mpsc queue
//! that the nav demo / channel handlers can write to. This avoids locks.

use crate::channels::audio::AudioChannelManager;
use crate::channels::sensor::SensorHandler;
use crate::channels::video::VideoPipelineHandle;
use crate::channels::{audio, input, nav, sensor, video, OutboundMessage};
use crate::control::{read_frame, write_frame};
use std::sync::Arc;
use crate::encrypted::{decrypt_payload, encrypt_payload, Side};
use crate::framing::{Frame, FrameType};
use crate::nav::source::{Destination, NavInstructionSource, PositionFix};
use crate::nav::{spawn_nav_runner, NavRunnerConfig};
use crate::services;
use aabox_common::{ChannelId, ControlMessageId};
use aabox_proto::enums::{audio_focus_state, status};
use aabox_proto::messages::{
    AudioFocusRequest, AudioFocusResponse, AuthCompleteIndication, ChannelOpenRequest,
    ChannelOpenResponse, NavigationFocusRequest, NavigationFocusResponse, PhoneInfo,
    PingRequest, PingResponse, ServiceDiscoveryRequest, ServiceDiscoveryResponse,
};
use anyhow::{anyhow, Context, Result};
use bytes::{BufMut, Bytes, BytesMut};
use prost::Message;
use tokio::io::{AsyncRead, AsyncWrite};

/// Tunable runtime knobs for the control-channel loop. All defaults are sane.
pub struct ControlLoopConfig {
    /// Where to write the GPS NDJSON log. Override in tests.
    pub gps_log_path: std::path::PathBuf,
    /// If true, spawn a periodic synthetic NavInstruction so the car sees
    /// non-empty traffic on the nav channel before we have a real router.
    /// Mutually exclusive with `nav_source`: when a `nav_source` is provided
    /// the demo loop is suppressed regardless of this flag.
    pub demo_nav: bool,
    /// Demo nav interval (used when `demo_nav=true`).
    pub demo_nav_interval: std::time::Duration,
    /// Optional response builder override (lets tests inject a minimal SDR).
    /// `None` -> use `services::full_response()`.
    pub service_discovery_response: Option<ServiceDiscoveryResponse>,
    /// Optional real navigation source (OSRM / Valhalla / offline). When set,
    /// the control loop wires the sensor channel's GPS output to the source
    /// and pumps the source's emitted instructions onto the nav channel.
    /// Mutually exclusive with `demo_nav`.
    pub nav_source: Option<Box<dyn NavInstructionSource + Send>>,
    /// Optional initial destination handed to the nav source at startup. The
    /// CLI fills this in when `--nav-destination "lat,lon[,label]"` is used;
    /// the destination-file watcher pushes additional destinations later.
    pub initial_destination: Option<Destination>,
    /// Optional destination-file watcher channel. When `Some`, the control
    /// loop forwards destinations from this receiver into the nav source.
    pub destination_rx: Option<tokio::sync::mpsc::Receiver<Destination>>,
}

impl Default for ControlLoopConfig {
    fn default() -> Self {
        Self {
            gps_log_path: std::path::PathBuf::from(sensor::DEFAULT_GPS_LOG_PATH),
            demo_nav: true,
            demo_nav_interval: std::time::Duration::from_secs(15),
            service_discovery_response: None,
            nav_source: None,
            initial_destination: None,
            destination_rx: None,
        }
    }
}

/// Build an outbound frame from a plaintext (msg_id || payload) blob.
/// Encrypts the inner bytes via the supplied TLS connection and wraps the
/// resulting ciphertext into an AAP frame with the ENCRYPTED flag set.
///
/// The CONTROL flag bit (0x04) gets set automatically for ChannelOpenRequest
/// (0x0007) and ChannelOpenResponse (0x0008) on non-zero channels — real
/// AA head units (per aa-proxy-rs captures, mitm.rs:2466) expect 0x0f flags
/// (ENCRYPTED|CONTROL|FIRST|LAST) on service-channel control frames; without
/// it, the bytes get treated as malformed service data and the HU disconnects.
/// Channel 0 (Control) never sets the CONTROL bit — DHU 2.0 rejects 0x07/0x0f
/// there, so it stays 0x0b.
fn build_encrypted_frame<C: Side>(
    conn: &mut C,
    channel: u8,
    msg_id: u16,
    payload: &[u8],
) -> Result<Frame> {
    let mut plain = BytesMut::with_capacity(2 + payload.len());
    plain.put_u16(msg_id);
    plain.put_slice(payload);
    let ciphertext = encrypt_payload(conn, &plain).context("encrypt_payload")?;
    let is_channel_control = channel != 0
        && (msg_id == ControlMessageId::ChannelOpenRequest as u16
            || msg_id == ControlMessageId::ChannelOpenResponse as u16);
    Ok(Frame {
        channel_id: channel,
        frame_type: FrameType::Bulk,
        control: is_channel_control,
        encrypted: true,
        total_length: None,
        payload: Bytes::from(ciphertext),
    })
}

/// Per-fragment plaintext cap. aasdk uses 0x4000 (16 KiB); we use 0x1F00
/// (~7.9 KiB) because Rockchip's f_accessory function on the Rock 5B+
/// kernel returns ESPIPE on `flush()` for the LAST fragment when the FIRST
/// is at the 16 KiB ceiling. Empirical: 2026-05-24 KIA test, single
/// 16 421-byte TX frame succeeded but the next write failed with
/// `Illegal seek (os error 29)`, the read endpoint started returning our
/// own previous TX bytes (kernel loopback), rustls then failed to decrypt
/// the garbage and emitted a TLS Alert, KIA tore down USB. Halving the
/// fragment size keeps every AAP frame under one bulk-transfer worth
/// (kernel `BULK_BUFFER_SIZE`) so the gadget stack handles them in one
/// kernel-level USB transfer per write — no flush-time ESPIPE.
const MAX_FRAGMENT_PLAINTEXT: usize = 0x1F00;

/// Send a single `OutboundMessage` encrypted on the wire, fragmenting into
/// FIRST/MIDDLE/LAST frames if the plaintext exceeds [`MAX_FRAGMENT_PLAINTEXT`].
///
/// Layout matches aasdk's `MessageOutStream`:
///   - SHORT form (one BULK frame) when plaintext fits in one fragment.
///   - EXTENDED form when fragmented:
///       FIRST  : channel | flags=FIRST|ENC | u16 frag_size | u32 total_plain
///                | ciphertext-of-first-chunk
///       MIDDLE : channel | flags=0|ENC     | u16 frag_size | ciphertext
///       LAST   : channel | flags=LAST|ENC  | u16 frag_size | ciphertext
///
///   `total_plain` = full plaintext length (msg_id u16 + payload).
///   `frag_size`   = ciphertext byte count of this fragment.
async fn send_encrypted<S, C>(
    stream: &mut S,
    conn: &mut C,
    out: OutboundMessage,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
    C: Side,
{
    // Build full plaintext: [u16 BE msg_id][payload].
    let mut plain = BytesMut::with_capacity(2 + out.payload.len());
    plain.put_u16(out.message_id);
    plain.put_slice(&out.payload);

    if plain.len() <= MAX_FRAGMENT_PLAINTEXT {
        // Fast path: one BULK frame.
        let f = build_encrypted_frame(conn, out.channel, out.message_id, &out.payload)?;
        return write_frame(stream, &f).await;
    }

    let total_plain = plain.len();
    let mut offset = 0usize;
    while offset < total_plain {
        let chunk_end = (offset + MAX_FRAGMENT_PLAINTEXT).min(total_plain);
        let chunk = &plain[offset..chunk_end];
        let ciphertext =
            encrypt_payload(conn, chunk).context("encrypt_payload (fragment)")?;
        let frame_type = if offset == 0 {
            FrameType::First
        } else if chunk_end == total_plain {
            FrameType::Last
        } else {
            FrameType::Middle
        };
        let frame = Frame {
            channel_id: out.channel,
            frame_type,
            control: false,
            encrypted: true,
            total_length: if frame_type == FrameType::First {
                Some(total_plain as u32)
            } else {
                None
            },
            payload: Bytes::from(ciphertext),
        };
        write_frame(stream, &frame).await?;
        offset = chunk_end;
    }
    Ok(())
}

/// Send a control-channel message as PLAINTEXT (used only for AuthComplete,
/// which the car expects unencrypted to confirm the TLS tunnel is live).
async fn send_plain_control<S, M>(stream: &mut S, msg_id: u16, msg: &M) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
    M: Message,
{
    let mut body = Vec::with_capacity(msg.encoded_len());
    msg.encode(&mut body).expect("encode");
    let frame = Frame::bulk_control(ChannelId::Control as u8, msg_id, &body);
    write_frame(stream, &frame).await
}

/// Run the control-channel loop. Returns when the stream closes or hits an
/// unrecoverable error.
pub async fn run<S, C>(stream: &mut S, conn: &mut C, cfg: ControlLoopConfig) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
    C: Side,
{
    // Step A: WAIT for the HU's AuthComplete (plaintext, control channel).
    // Per AACS/AAServer (`AaCommunicator.cpp:handleAuthComplete`), openauto
    // (`AndroidAutoEntity.cpp:135-142`), and aasdk's `ControlServiceChannel`,
    // it is the HU — not the source — that sends AuthComplete after the TLS
    // tunnel comes up. Sending it from our side confuses the HU (empirically
    // KIA returns `AuthComplete{status=-3}` and tears down USB).
    //
    // We read one plaintext frame on channel 0 and expect msg_id 0x0004
    // (AuthComplete). Any other msg_id is a protocol error.
    {
        let frame = read_frame(stream).await.context("read HU AuthComplete")?;
        let payload = &frame.payload;
        if payload.len() < 2 {
            return Err(anyhow!(
                "HU AuthComplete frame too short ({} bytes)",
                payload.len()
            ));
        }
        let hu_msg_id = u16::from_be_bytes([payload[0], payload[1]]);
        if hu_msg_id != ControlMessageId::AuthComplete as u16 {
            return Err(anyhow!(
                "expected AuthComplete (0x0004) from HU, got msg_id 0x{:04x}",
                hu_msg_id
            ));
        }
        let body = &payload[2..];
        let ind = AuthCompleteIndication::decode(body).context("decode AuthComplete")?;
        if ind.status != status::Enum::Ok as i32 {
            tracing::error!(status = ind.status, "HU AuthComplete returned non-OK");
            return Err(anyhow!(
                "HU rejected our cert: AuthComplete{{status={}}}",
                ind.status
            ));
        }
        tracing::info!(status = ind.status, "control: HU AuthComplete received (OK)");
    }

    // Step A2: SEND our ServiceDiscoveryRequest (encrypted) with our identity.
    // Per AAP, the source identifies itself to the HU right after AuthComplete.
    //
    // The proto fields were RENUMBERED between aasdk-era (2018) and modern AAP
    // (KIA Carnival 2025+ firmware). aasdk had `device_name=4, device_brand=5`;
    // modern AAP has `label_text=4, device_name=5, phone_info=6`. KIA
    // empirically disconnects USB ~500ms after seeing an old-format SDR with
    // those mis-numbered fields. The proto file under
    // references/aasdk/aasdk_proto/ServiceDiscoveryRequestMessage.proto has
    // been updated to the modern layout; we populate the modern fields here.
    //
    // openauto's old-aasdk-parser interprets Carlinkit's modern SDR as
    // `device_name="Auto", brand="Android"` — but those are actually
    // label_text and device_name on the wire. Carlinkit really sends:
    //   label_text  = "Auto"      (the display label / app name)
    //   device_name = "Android"   (or a phone model — we use "Pixel 6"
    //                              to match our USB descriptor)
    //   phone_info  = { instance_id, connectivity_lifetime_id }
    {
        let sdr = ServiceDiscoveryRequest {
            small_icon: Vec::new(),
            medium_icon: Vec::new(),
            large_icon: Vec::new(),
            label_text: "Auto".to_string(),
            device_name: "Pixel 6".to_string(),
            phone_info: Some(PhoneInfo {
                instance_id: "AABOX0123456789".to_string(),
                connectivity_lifetime_id: "aabox-rock5bp-1".to_string(),
            }),
        };
        let mut buf = Vec::with_capacity(sdr.encoded_len());
        sdr.encode(&mut buf).expect("encode SDR");
        let out = OutboundMessage::new(
            ChannelId::Control as u8,
            ControlMessageId::ServiceDiscoveryRequest as u16,
            buf,
        );
        send_encrypted(stream, conn, out)
            .await
            .context("send ServiceDiscoveryRequest")?;
        tracing::info!(
            label_text = "Auto",
            device_name = "Pixel 6",
            "control: ServiceDiscoveryRequest sent (modern proto, encrypted)"
        );
    }

    // Step B: wire up the navigation pipeline.
    //
    //   nav_tx   <- runner / demo task : carries encoded-ready NavInstruction
    //                                    to the wire below
    //   gps_tx   <- sensor handler     : forwards every GPS fix
    //   dest_tx  <- file watcher / CLI : forwards destinations
    //
    // Three configurations:
    //   - cfg.nav_source = Some(_)  : real source, runner spawned, demo off
    //   - cfg.nav_source = None && cfg.demo_nav = true : demo loop spawned
    //   - cfg.nav_source = None && cfg.demo_nav = false : nav channel silent
    let (nav_tx, mut nav_rx) =
        tokio::sync::mpsc::channel::<nav::NavInstruction>(8);

    // GPS sink: bounded mpsc the sensor handler forwards into. The runner
    // owns the receiver. Capacity is small (the runner consumes each fix in
    // a couple of ms unless an OSRM call is in flight; if we ever back up
    // that much, dropping fixes is fine — the car ships ~1 Hz).
    let (gps_tx, gps_rx) = tokio::sync::mpsc::channel::<PositionFix>(8);

    let sensor_handler = if cfg.nav_source.is_some() {
        SensorHandler::new(&cfg.gps_log_path).with_gps_sink(gps_tx)
    } else {
        // No nav source — don't bother forwarding GPS. Drop gps_tx so its
        // matching gps_rx the runner never sees a writer. We still construct
        // the rx so the variable type is uniform.
        drop(gps_tx);
        SensorHandler::new(&cfg.gps_log_path)
    };

    // Destination feed: always allocate a local sender so the CLI's
    // `initial_destination` (if any) can be pushed through it before the
    // runner starts reading. If the caller also supplied `destination_rx`
    // (typically the file watcher) we honour that one for the runner side;
    // the local sender then just keeps the channel alive against accidental
    // close events from any future CLI pokes we add.
    let (local_dest_tx, local_dest_rx) =
        tokio::sync::mpsc::channel::<Destination>(4);
    let dest_rx_for_runner = cfg.destination_rx.unwrap_or(local_dest_rx);

    let had_nav_source = cfg.nav_source.is_some();
    let _runner_handle = if let Some(source) = cfg.nav_source {
        if let Some(d) = cfg.initial_destination.clone() {
            // Capacity is 4 and the channel is fresh; this never fails.
            let _ = local_dest_tx.try_send(d);
        }
        Some(spawn_nav_runner(
            source,
            dest_rx_for_runner,
            gps_rx,
            nav_tx.clone(),
            NavRunnerConfig { emit_initial: true },
        ))
    } else {
        drop(gps_rx);
        drop(dest_rx_for_runner);
        None
    };
    // Keep the local sender alive for the lifetime of run() so the runner
    // doesn't see the destination channel as "closed".
    let _local_dest_tx_keepalive = local_dest_tx;

    let _demo_handle = if !had_nav_source && cfg.demo_nav {
        Some(nav::spawn_demo(nav_tx, cfg.demo_nav_interval))
    } else {
        // Drop the cloneable sender so the channel closes when the only
        // sender (the runner, if any) goes away.
        drop(nav_tx);
        None
    };

    let sdr_template = cfg
        .service_discovery_response
        .unwrap_or_else(services::full_response);

    // Step B'': audio channel manager + its outbound mpsc. The manager pushes
    // data-plane media-audio frames here; the message loop drains and ships
    // them encrypted on the wire. 64 slots is enough for ~1.3 sec of 48 kHz /
    // 20 ms frames before backpressure kicks in.
    let (audio_tx, mut audio_rx) =
        tokio::sync::mpsc::channel::<OutboundMessage>(64);
    let audio_mgr = Arc::new(AudioChannelManager::new(audio_tx));

    // Step B''': video pipeline. Symmetric to audio: handler calls .start()
    // / .stop() on the handle during inbound dispatch, and the encoder task
    // pushes encoded H.264 access units onto the outbound mpsc.
    let video_pipeline = video::VideoPipeline::new(video::VIDEO_MAX_UNACKED);
    let video_handle = video_pipeline.handle();
    let mut video_rx = video_pipeline
        .take_outbound_rx()
        .expect("VideoPipeline outbound_rx not yet taken");

    // Step C: the message loop.
    loop {
        tokio::select! {
            biased;
            // Inbound frame from the car. We use a small bias toward reading
            // so outbound nav pushes never starve the wire.
            frame_res = read_frame(stream) => {
                let frame = match frame_res {
                    Ok(f) => f,
                    Err(e) => {
                        tracing::info!("control: stream closed / error: {e:#}");
                        return Ok(());
                    }
                };
                if let Err(e) = handle_frame(
                    stream,
                    conn,
                    &frame,
                    &sdr_template,
                    &sensor_handler,
                    &audio_mgr,
                    &video_handle,
                )
                .await
                {
                    tracing::warn!("control: handle_frame error: {e:#}");
                }
            }
            // Nav demo / future router output -> wire.
            Some(instr) = nav_rx.recv() => {
                tracing::info!(
                    text = %instr.status_text,
                    turn = instr.turn_id,
                    dist_m = instr.distance_m,
                    "control: emitting nav instruction"
                );
                for out in nav::encode_instruction(&instr) {
                    if let Err(e) = send_encrypted(stream, conn, out).await {
                        tracing::warn!("control: nav send failed: {e:#}");
                        break;
                    }
                }
            }
            // Outbound audio frames produced by the AudioChannelManager's
            // encoder tasks (media-out test tone today, real Android tap
            // tomorrow). Each tick = one AV_MEDIA_WITH_TIMESTAMP packet.
            Some(out) = audio_rx.recv() => {
                if let Err(e) = send_encrypted(stream, conn, out).await {
                    tracing::warn!("control: audio send failed: {e:#}");
                }
            }
            // Outbound video frames produced by the VideoPipeline's
            // streaming task. Same shape as audio: one wire frame per recv.
            Some(out) = video_rx.recv() => {
                if let Err(e) = send_encrypted(stream, conn, out).await {
                    tracing::warn!("control: video send failed: {e:#}");
                }
            }
        }
    }
}

/// Dispatch one inbound frame. The frame has already been read off the wire.
async fn handle_frame<S, C>(
    stream: &mut S,
    conn: &mut C,
    frame: &Frame,
    sdr_template: &ServiceDiscoveryResponse,
    sensor_handler: &SensorHandler,
    audio_mgr: &AudioChannelManager,
    video_handle: &VideoPipelineHandle,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
    C: Side,
{
    // Decrypt if needed.
    let plain = if frame.encrypted {
        decrypt_payload(conn, &frame.payload).context("decrypt frame")?
    } else {
        frame.payload.to_vec()
    };
    if plain.len() < 2 {
        return Err(anyhow!(
            "channel {} frame: plaintext too short ({})",
            frame.channel_id,
            plain.len()
        ));
    }
    let msg_id = u16::from_be_bytes([plain[0], plain[1]]);
    let body = &plain[2..];

    // ChannelOpenResponse + UNEXPECTED_MESSAGE arrive on the SERVICE channel
    // for opens we sent from the control loop. Route them through `dispatch`
    // (which has the cross-channel intercept) instead of the audio/video
    // handlers — those don't know about Control-message IDs and would log
    // them as "unknown".
    let is_cross_channel_control =
        msg_id == ControlMessageId::ChannelOpenResponse as u16 || msg_id == 0x00ff;

    // VideoFocusNotification (msg 0x8008) from HU: this is the signal to
    // start streaming H.264 frames. Two things must happen:
    //   1. tell the live VideoPipeline what wire channel to write on
    //      (KIA assigned this — channel 1 in the Carnival 2024 case,
    //       NOT our internal canonical ChannelId::Video=3)
    //   2. kick `video_handle.start(session=1, config=0)` so the streamer
    //      task starts pushing test-pattern frames onto outbound_tx
    // The `dispatch` cross-channel intercept also returns the wire-format
    // Start{session=1, config=0} message we need to send to the HU.
    if msg_id == 0x8008 {
        video_handle.set_wire_channel(frame.channel_id);
        video_handle.start(1, 0);
        tracing::info!(
            channel = frame.channel_id,
            "video: pipeline kicked on wire channel"
        );
    }
    // AV_MEDIA_ACK_INDICATION (msg 0x8004) — HU acks one of our streamed
    // video frames. Release an in-flight slot so the streamer can send
    // another. This is the per-frame backpressure mechanism advertised
    // via `max_unacked` in our SETUP_RESPONSE (default 10). Without it the
    // pipeline stalls after the first burst of frames.
    if msg_id == 0x8004 {
        video_handle.on_media_ack();
        tracing::debug!(channel = frame.channel_id, "video: media ack received");
    }
    // STOP_INDICATION (msg 0x8002) on the video wire channel: KIA asks us
    // to halt streaming. CAREFUL: 0x8002 is also:
    //   - SensorStartResponse on the sensor channel
    //   - BindingResponse on the input channel
    //   - MicrophoneResponse alias
    // and these arrive ALL THE TIME during setup. Only treat as a STOP if
    // the frame is on the video pipeline's current wire channel. KIA
    // Carnival assigns video to channel byte 1; we recorded that in
    // VideoPipelineHandle::wire_channel when VideoFocusNotification arrived.
    // (Earlier code stopped on any msg 0x8002 on a non-Control non-input
    // channel — that fired on the sensor channel's ack and killed the
    // video pipeline before it ever started.)
    if msg_id == 0x8002 && frame.channel_id == video_handle.wire_channel() {
        video_handle.stop();
        tracing::info!(channel = frame.channel_id, "video: STOP_INDICATION received");
    }

    // Audio channels (4/5/6) need an async dispatch path so the sink push
    // can do real I/O. Video has stateful pipeline kicking so it needs the
    // live handle. Everything else goes through the pure `dispatch` helper,
    // which the tests reuse to exercise routing without sockets.
    let chan_kind = ChannelId::try_from(frame.channel_id).ok();
    let outs = if is_cross_channel_control {
        dispatch(frame.channel_id, msg_id, body, sdr_template, sensor_handler)?
    } else if matches!(
        chan_kind,
        Some(ChannelId::MediaAudio)
            | Some(ChannelId::SpeechAudio)
            | Some(ChannelId::SystemAudio)
            | Some(ChannelId::AvInput)
    ) {
        audio_mgr.handle(frame.channel_id, msg_id, body).await?
    } else if matches!(chan_kind, Some(ChannelId::Video)) {
        video::handle(frame.channel_id, msg_id, body, video_handle)?
    } else {
        dispatch(frame.channel_id, msg_id, body, sdr_template, sensor_handler)?
    };
    for out in outs {
        send_encrypted(stream, conn, out).await?;
    }
    Ok(())
}

/// Pure dispatch — given the (channel, msg_id, body) of an inbound message,
/// return the OutboundMessages we want to send back. Factored out so tests
/// can exercise the routing table without sockets.
pub fn dispatch(
    channel_id: u8,
    msg_id: u16,
    body: &[u8],
    sdr_template: &ServiceDiscoveryResponse,
    sensor_handler: &SensorHandler,
) -> Result<Vec<OutboundMessage>> {
    // ChannelOpenResponse (msg_id=0x0008) arrives on the SERVICE channel, not
    // Control — intercept it here before falling through to the per-channel
    // handler (which would log it as unknown). Same for the UNEXPECTED message
    // (0x00FF) that KIA sends back when an open is rejected.
    if msg_id == ControlMessageId::ChannelOpenResponse as u16 {
        if let Ok(resp) = ChannelOpenResponse::decode(body) {
            tracing::info!(
                channel = channel_id,
                status = resp.status,
                "ChannelOpenResponse received from HU"
            );
        } else {
            tracing::warn!(channel = channel_id, "ChannelOpenResponse undecodable");
        }
        return Ok(vec![]);
    }
    if msg_id == 0x00ff {
        tracing::warn!(
            channel = channel_id,
            "UNEXPECTED_MESSAGE from HU — HU rejected something we sent on this channel"
        );
        return Ok(vec![]);
    }
    // AVChannelSetupResponse (msg_id=0x8003) is what KIA sends back on the AV
    // channel after we send AVChannelSetupRequest (0x8000). Intercept and log
    // it here so it doesn't get buried in the per-channel "unhandled msg id"
    // log — this is the signal we're past the proto2-required-`type` bug and
    // KIA accepts our Setup.
    if msg_id == 0x8003 {
        tracing::info!(
            channel = channel_id,
            body_len = body.len(),
            "AVChannelSetupResponse received from HU"
        );
        return Ok(vec![]);
    }
    // VIDEO_FOCUS_NOTIFICATION (0x8008): HU pushes this UNSOLICITED on the
    // video channel after our SetupResponse. Per
    // /tmp/gearhead-research/openautolink/.../jni_session.cpp:860-872, the
    // phone's ProjectionWindowManager waits for this before sending Start
    // and beginning to stream H.264 frames. Log + send Start back.
    //
    // Start body: required int32 session_id = 1, required uint32
    // configuration_index = 2. Wire: `08 01 10 00` (session_id=1, config=0).
    if msg_id == 0x8008 {
        tracing::info!(
            channel = channel_id,
            body_len = body.len(),
            "VideoFocusNotification received from HU — sending Start session=1 config=0"
        );
        return Ok(vec![OutboundMessage::new(
            channel_id,
            0x8001,  // START_INDICATION
            vec![0x08, 0x01, 0x10, 0x00],
        )]);
    }
    // AudioFocusNotification (msg_id=0x0013) on Control channel: HU's ack
    // to our AudioFocusRequest, or an unsolicited push after we send Setup
    // on an audio channel. Log it.
    if msg_id == ControlMessageId::AudioFocusResponse as u16
        && channel_id == ChannelId::Control as u8
    {
        tracing::info!(
            body_len = body.len(),
            "AudioFocusNotification received from HU"
        );
        return Ok(vec![]);
    }
    // SensorStartResponse (msg_id=0x8002 on Sensor channel) — KIA's ack to
    // our SensorStartRequest(0x8001).
    if msg_id == 0x8002 && channel_id == ChannelId::Sensor as u8 {
        tracing::info!(
            channel = channel_id,
            body_len = body.len(),
            "SensorStartResponse received from HU"
        );
        return Ok(vec![]);
    }
    // MicrophoneResponse (msg_id=0x8006 on AvInput channel) — KIA's ack to
    // our MicrophoneRequest(0x8005) sent in `setup_message_for("av_input")`.
    // Modern proto (aa-proxy-rs/src/protos/protos.proto:503):
    //   message MicrophoneResponse {
    //     required int32  status     = 1;
    //     optional int32  session_id = 2;
    //   }
    // We just log it — there's nothing to do until we actually want to
    // pump mic audio (voice session), at which point we'd send another
    // MicrophoneRequest{open=true}.
    if msg_id == 0x8006 && channel_id == ChannelId::AvInput as u8 {
        tracing::info!(
            channel = channel_id,
            body_len = body.len(),
            "MicrophoneResponse received from HU"
        );
        return Ok(vec![]);
    }
    // BindingResponse (msg_id=0x8003 on Input channel) — clashes with
    // AVChannelSetupResponse numerically; the channel byte disambiguates.
    // The check above (msg_id==0x8003) catches both, but for Input that's
    // BindingResponse. Log distinctly when on input channel.
    // (No action needed — handled by the 0x8003 case above which logs body.)
    let chan = ChannelId::try_from(channel_id).unwrap_or(ChannelId::None);
    match chan {
        ChannelId::Control => dispatch_control(msg_id, body, sdr_template),
        ChannelId::Input => input::handle(msg_id, body),
        ChannelId::Sensor => sensor_handler.handle(msg_id, body),
        ChannelId::Video => {
            // The Phase-5 video pipeline takes a handle to drive START/STOP
            // state transitions on the streaming task. From this pure
            // dispatcher (which the unit tests reuse without sockets) we
            // hand it a fresh, disconnected handle: SETUP responses still
            // flow back correctly, and START/STOP poke a pipeline whose
            // streaming task is never started anyway. Live runs go through
            // `handle_frame` which uses the real pipeline.
            let pipeline = video::VideoPipeline::new(video::VIDEO_MAX_UNACKED);
            let handle = pipeline.handle();
            video::handle(channel_id, msg_id, body, &handle)
        }
        ChannelId::MediaAudio | ChannelId::SpeechAudio | ChannelId::SystemAudio
        | ChannelId::AvInput => audio::handle(channel_id, msg_id, body),
        ChannelId::Navigation => nav::handle(msg_id, body),
        ChannelId::Bluetooth | ChannelId::None => {
            tracing::info!(
                channel = channel_id,
                msg_id = format!("0x{:04x}", msg_id),
                "unhandled channel"
            );
            Ok(vec![])
        }
    }
}

fn dispatch_control(
    msg_id: u16,
    body: &[u8],
    sdr_template: &ServiceDiscoveryResponse,
) -> Result<Vec<OutboundMessage>> {
    let id = ControlMessageId::try_from(msg_id);
    match id {
        Ok(ControlMessageId::ServiceDiscoveryResponse) => {
            // We're the source; this is the HU telling us what channels it
            // supports. Classify each descriptor by its embedded sub-message
            // (sensor_channel / av_channel / input_channel / etc.) and emit
            // a ChannelOpenRequest for every projection channel. KIA stays
            // pinging us until it sees opens; without them it stops at
            // "Follow instructions on your phone".
            let mut outs: Vec<OutboundMessage> = Vec::new();
            if let Ok(resp) = ServiceDiscoveryResponse::decode(body) {
                tracing::info!(
                    channel_count = resp.channels.len(),
                    head_unit_name = %resp.head_unit_name,
                    car_model = %resp.car_model,
                    car_year = %resp.car_year,
                    sw_build = %resp.sw_build,
                    "control: ServiceDiscoveryResponse received from HU"
                );

                // === Proactive phone-status notifications on Control (0) ===
                // gromaudio/android_external_aoaptest/hu_aap.c and KIA-specific
                // anecdotal reports suggest the HU gates projection on seeing
                // these "real phone" signals. We send them eagerly before the
                // channel-open dance so KIA sees a fully-engaged phone.

                // BatteryStatusNotification (msg_id=0x0017): required uint32
                // battery_level = 1. Wire: `08 64` (level=100).
                tracing::info!("control: sending BatteryStatusNotification(level=100)");
                outs.push(OutboundMessage::new(
                    ChannelId::Control as u8,
                    0x0017,
                    vec![0x08, 0x64],
                ));

                // CallAvailabilityStatus (msg_id=0x0018): optional bool
                // call_available = 1. Wire: `08 00` (false).
                tracing::info!("control: sending CallAvailabilityStatus(false)");
                outs.push(OutboundMessage::new(
                    ChannelId::Control as u8,
                    0x0018,
                    vec![0x08, 0x00],
                ));
                for ch in &resp.channels {
                    let kind = classify_channel(ch);
                    // Dump per-channel descriptor body so we can spot which
                    // sub-fields KIA actually populated (audio_configs count,
                    // video_configs count, stream_type/audio_type enums). This
                    // is the missing data we need to tell whether channel 1's
                    // "audio_unknown" is actually a video sink in disguise.
                    let (st, at, ac, vc) = match &ch.av_channel {
                        Some(av) => (
                            av.stream_type,
                            av.audio_type,
                            av.audio_configs.len(),
                            av.video_configs.len(),
                        ),
                        None => (0, 0, 0, 0),
                    };
                    tracing::info!(
                        channel_id = ch.channel_id,
                        kind = kind,
                        has_sensor = ch.sensor_channel.is_some(),
                        has_input = ch.input_channel.is_some(),
                        has_av_input = ch.av_input_channel.is_some(),
                        has_bluetooth = ch.bluetooth_channel.is_some(),
                        has_nav = ch.navigation_channel.is_some(),
                        has_vendor = ch.vendor_extension_channel.is_some(),
                        av_stream_type = st,
                        av_audio_type = at,
                        av_audio_configs = ac,
                        av_video_configs = vc,
                        "  - HU channel"
                    );
                    // Dump each video_config so we know what resolution / fps
                    // KIA actually offered. VideoResolution: 1=480p, 2=720p,
                    // 3=1080p. VideoFPS: 1=30, 2=60. Our test pattern is
                    // 1080p30 — we need at least one of KIA's configs to
                    // match, OR send the H264 with KIA's preferred config.
                    if let Some(av) = &ch.av_channel {
                        for (i, vcfg) in av.video_configs.iter().enumerate() {
                            tracing::info!(
                                channel_id = ch.channel_id,
                                idx = i,
                                resolution = vcfg.video_resolution,
                                fps = vcfg.video_fps,
                                margin_w = vcfg.margin_width,
                                margin_h = vcfg.margin_height,
                                dpi = vcfg.dpi,
                                "    video_config"
                            );
                        }
                        for (i, acfg) in av.audio_configs.iter().enumerate() {
                            tracing::info!(
                                channel_id = ch.channel_id,
                                idx = i,
                                sample_rate = acfg.sample_rate,
                                bit_depth = acfg.bit_depth,
                                channel_count = acfg.channel_count,
                                "    audio_config"
                            );
                        }
                    }
                    if should_open(kind) {
                        tracing::info!(
                            channel_id = ch.channel_id,
                            kind = kind,
                            "control: sending ChannelOpenRequest"
                        );
                        // ChannelOpenRequest is addressed to the TARGET service's
                        // channel byte (not the Control channel 0). aasdk's
                        // SensorServiceChannel::sendChannelOpenResponse and the
                        // matching receive switch both address the open
                        // exchange to `channelId_` (the service's own channel).
                        //
                        // Hand-encode the body so `priority=0` is on the wire.
                        // The modern aa-proxy-rs proto declares
                        // `required sint32 priority = 1; required int32 service_id = 2`
                        // (proto2 syntax). Our generated prost proto3 code
                        // omits priority=0 from the wire — KIA reads the
                        // truncated message as missing a required field and
                        // silently drops the request (2026-05-24 capture:
                        // 9 opens sent, 0 ChannelOpenResponses received over
                        // 4 full reconnect cycles).
                        //
                        // Wire layout: `08 00 10 <varint service_id>`
                        //   tag 0x08 = field 1 (priority), wire type 0 (varint)
                        //   value 0x00 = priority 0
                        //   tag 0x10 = field 2 (service_id), wire type 0
                        //   varint encodes service_id (1 byte for ids <128).
                        let mut body = Vec::with_capacity(4);
                        body.push(0x08);
                        body.push(0x00);
                        body.push(0x10);
                        prost::encoding::encode_varint(
                            ch.channel_id as u64,
                            &mut body,
                        );
                        outs.push(OutboundMessage::new(
                            ch.channel_id as u8,
                            ControlMessageId::ChannelOpenRequest as u16,
                            body,
                        ));
                        // Queue the per-service Setup right after the open.
                        // KIA processes both within ~5ms — by the time it sees
                        // the Setup the channel-open ack has already been
                        // generated, so the setup lands on an "open" channel.
                        if let Some(setup) = setup_message_for(ch.channel_id, kind) {
                            tracing::info!(
                                channel_id = ch.channel_id,
                                kind = kind,
                                msg_id = format!("0x{:04x}", setup.message_id),
                                "control: sending per-service Setup"
                            );
                            outs.push(setup);
                        }
                    }
                }

                // === AudioFocusRequest on Control (REMOVED 2026-05-24) ===
                // Earlier code unconditionally sent
                // `AudioFocusRequest{audio_focus_type=GAIN}` here (msg 0x0012,
                // body `08 01`). That triggered KIA's UNEXPECTED_MESSAGE on
                // channel 0 at the very next round-trip — the 2026-05-24 KIA
                // capture (kia-test/aabox-aapd.log:12:00:05.063040) shows the
                // UNEXPECTED arrives ~400µs after we send this message, while
                // the BatteryStatusNotification (0x17) and CallAvailabilityStatus
                // (0x18) sent first appear to be silently accepted.
                //
                // Modern AAP HUs (KIA Carnival 2024 included) grant audio focus
                // PROACTIVELY: openautolink's `JniAudioSinkHandler::onMediaChannelSetupRequest`
                // (jni_channel_handlers.cpp:164) calls
                // `session_.sendUnsolicitedAudioFocusGain()` from the head-unit
                // side after each successful audio-channel Setup — sending
                // `AudioFocusNotification{focus_state=GAIN, unsolicited=true}`
                // (msg 0x0013) without a prior request from the phone. KIA
                // empirically does the same: our capture logs an unsolicited
                // AudioFocusNotification at 12:00:05.069765 — 4ms after our
                // last channel-Setup, no AudioFocusRequest needed.
                //
                // The aasdk-era "phone-sends-AudioFocusRequest-first" pattern
                // (from openauto's AudioInputService and gromaudio hu_aap.c)
                // is leftover from pre-2020 firmware that did NOT grant focus
                // unsolicited. KIA's modern firmware treats an unsolicited
                // AudioFocusRequest before the HU has initiated focus as a
                // protocol violation and replies UNEXPECTED.
                //
                // We now wait for KIA's unsolicited AudioFocusNotification and
                // handle it in `dispatch_control` (the existing branch at the
                // top of the function logs the notification body). If we ever
                // need to *change* focus later (e.g. release on voice session
                // end), we send AudioFocusRequest then — but never preemptively
                // at SDR-Response time.
            } else {
                tracing::warn!("control: ServiceDiscoveryResponse undecodable");
            }
            Ok(outs)
        }
        Ok(ControlMessageId::ChannelOpenResponse) => {
            let resp = ChannelOpenResponse::decode(body)
                .map_err(|e| anyhow!("decode ChannelOpenResponse: {e}"))?;
            // status::Enum::Ok = 0; non-zero means the HU rejected the open.
            tracing::info!(
                status = resp.status,
                "control: ChannelOpenResponse received from HU"
            );
            Ok(vec![])
        }
        Ok(ControlMessageId::ServiceDiscoveryRequest) => {
            // Defensive: not expected in source role, but log it so we know.
            // Modern proto: device_brand was removed; use label_text + device_name.
            if let Ok(req) = ServiceDiscoveryRequest::decode(body) {
                tracing::warn!(
                    label_text = %req.label_text,
                    device_name = %req.device_name,
                    "control: unexpected ServiceDiscoveryRequest from HU (we're source); ignoring"
                );
            }
            let _unused = sdr_template;
            Ok(vec![])
        }
        Ok(ControlMessageId::PingRequest) => {
            let req = PingRequest::decode(body)
                .map_err(|e| anyhow!("decode PingRequest: {e}"))?;
            tracing::debug!(ts = req.timestamp, "control: PingRequest");
            let resp = PingResponse {
                timestamp: req.timestamp,
            };
            Ok(vec![encode_msg(
                ChannelId::Control as u8,
                ControlMessageId::PingResponse as u16,
                &resp,
            )])
        }
        Ok(ControlMessageId::AudioFocusRequest) => {
            let req = AudioFocusRequest::decode(body)
                .map_err(|e| anyhow!("decode AudioFocusRequest: {e}"))?;
            tracing::info!(
                req_type = req.audio_focus_type,
                "control: AudioFocusRequest -> GAIN"
            );
            let resp = AudioFocusResponse {
                audio_focus_state: audio_focus_state::Enum::Gain as i32,
            };
            Ok(vec![encode_msg(
                ChannelId::Control as u8,
                ControlMessageId::AudioFocusResponse as u16,
                &resp,
            )])
        }
        Ok(ControlMessageId::NavigationFocusRequest) => {
            // type=1 (NATIVE) — we render nav natively from our side.
            if let Ok(req) = NavigationFocusRequest::decode(body) {
                tracing::info!(
                    req_type = req.r#type,
                    "control: NavigationFocusRequest -> NATIVE"
                );
            }
            let resp = NavigationFocusResponse { r#type: 1 };
            Ok(vec![encode_msg(
                ChannelId::Control as u8,
                ControlMessageId::NavigationFocusResponse as u16,
                &resp,
            )])
        }
        Ok(ControlMessageId::ChannelOpenRequest) => {
            let req = ChannelOpenRequest::decode(body)
                .map_err(|e| anyhow!("decode ChannelOpenRequest: {e}"))?;
            tracing::info!(
                channel = req.channel_id,
                priority = req.priority,
                "control: ChannelOpenRequest -> OK"
            );
            let resp = ChannelOpenResponse {
                status: status::Enum::Ok as i32,
            };
            Ok(vec![encode_msg(
                ChannelId::Control as u8,
                ControlMessageId::ChannelOpenResponse as u16,
                &resp,
            )])
        }
        Ok(ControlMessageId::VoiceSessionRequest) => {
            tracing::info!("control: VoiceSessionRequest (no response)");
            Ok(vec![])
        }
        Ok(ControlMessageId::ShutdownRequest) => {
            tracing::info!("control: ShutdownRequest -> ShutdownResponse");
            let resp = aabox_proto::messages::ShutdownResponse {};
            Ok(vec![encode_msg(
                ChannelId::Control as u8,
                ControlMessageId::ShutdownResponse as u16,
                &resp,
            )])
        }
        Ok(other) => {
            tracing::info!(?other, "control: ignored msg");
            Ok(vec![])
        }
        Err(raw) => {
            tracing::warn!(
                msg_id = format!("0x{:04x}", raw),
                "control: unknown msg id"
            );
            Ok(vec![])
        }
    }
}

fn encode_msg<M: Message>(channel: u8, msg_id: u16, m: &M) -> OutboundMessage {
    let mut buf = Vec::with_capacity(m.encoded_len());
    m.encode(&mut buf).expect("encode");
    OutboundMessage::new(channel, msg_id, buf)
}

/// Classify a `ChannelDescriptor` by which sub-message it carries. Used to
/// decide whether the source should send a `ChannelOpenRequest` for it and to
/// log a human-readable channel type alongside the numeric ID.
///
/// IMPORTANT — modern AAP (aa-proxy-rs/src/mitm.rs:1233) disambiguates a
/// MediaSinkService into VIDEO vs AUDIO based on whether `video_configs` is
/// non-empty, NOT on the `stream_type`/`available_type` enum value. KIA
/// Carnival 2024 advertises its main video sink as `av_channel` with
/// `stream_type=AUDIO(1) audio_type=NONE(0)` but populates `video_configs`.
/// Our previous classifier saw stream_type=AUDIO and classified as
/// "audio_unknown", missing the video channel entirely.
fn classify_channel(ch: &aabox_proto::data::ChannelDescriptor) -> &'static str {
    if ch.sensor_channel.is_some() {
        "sensor"
    } else if let Some(av) = &ch.av_channel {
        // Modern check first: `video_configs` non-empty == video sink,
        // regardless of stream_type. Our `AvChannel` proto still calls the
        // field `video_configs` at tag 4 (same as modern `MediaSinkService`).
        if !av.video_configs.is_empty() {
            "video"
        } else if av.stream_type == aabox_proto::enums::av_stream_type::Enum::Video as i32 {
            // Old-style: explicit Video stream_type.
            "video"
        } else {
            // Audio sink — audio_type tells us what kind. SPEECH=1, SYSTEM=2,
            // MEDIA=3, ALARM=4. Modern AudioStreamType: GUIDANCE=1, SYSTEM=2,
            // MEDIA=3, TELEPHONY=4. The numeric values overlap so the same
            // match works for both old and new HUs.
            match av.audio_type {
                1 => "audio_speech",
                2 => "audio_system",
                3 => "audio_media",
                4 => "audio_alarm",
                _ => "audio_unknown",
            }
        }
    } else if ch.input_channel.is_some() {
        "input"
    } else if ch.av_input_channel.is_some() {
        "av_input"
    } else if ch.bluetooth_channel.is_some() {
        "bluetooth"
    } else if ch.navigation_channel.is_some() {
        "navigation"
    } else if ch.vendor_extension_channel.is_some() {
        "vendor_ext"
    } else {
        "unknown"
    }
}

/// Decide whether the source should send a `ChannelOpenRequest` for this
/// channel kind. We open every projection-relevant channel; vendor extensions
/// and channels whose descriptor we can't identify are skipped.
///
/// `audio_unknown` is included — KIA Carnival's channel 1 has `av_channel`
/// set with `stream_type=AUDIO(1) audio_type=NONE(0)`. Empirically that's the
/// `MediaSinkService` with `available_type=MEDIA_CODEC_AUDIO_PCM` and no
/// declared audio_type, which seems to be KIA's primary media sink. Keeping
/// it closed previously meant we left a service KIA expects open.
fn should_open(kind: &str) -> bool {
    matches!(
        kind,
        "sensor"
            | "video"
            | "input"
            | "av_input"
            | "audio_media"
            | "audio_speech"
            | "audio_system"
            | "audio_alarm"
            | "audio_unknown"
            | "navigation"
            | "bluetooth"
    )
}

/// Build the per-service setup message that follows a successful
/// `ChannelOpenRequest`. The phone (source) drives these — KIA waits for them
/// before progressing past "Reading USB device → Follow instructions on your
/// phone". Returns `None` for kinds with no defined setup (navigation,
/// bluetooth) — those may not need one to unstick projection.
///
/// Channel-message IDs from `aasdk_proto/{AV,Sensor,Input}ChannelMessageIdsEnum.proto`:
///   AVChannel  SETUP_REQUEST          = 0x8000
///   Sensor     SENSOR_START_REQUEST   = 0x8001
///   Input      BINDING_REQUEST        = 0x8002
fn setup_message_for(channel_id: u32, kind: &str) -> Option<OutboundMessage> {
    match kind {
        // AV channels — phone sends AVChannelSetupRequest. Wire body MUST
        // include the codec type (field 1, varint).
        //
        // The MODERN AAP proto (aa-proxy-rs/src/protos/protos.proto) declares
        //   message Setup { required MediaCodecType type = 1; }
        // So `type` is proto2-required — KIA silently drops a Setup with an
        // empty body (same root cause as the proto2 `priority` issue we hit
        // on ChannelOpenRequest). The OLD aasdk proto called field 1
        // `config_index uint32`; the wire tag is identical (`08`).
        //
        // MediaCodecType values from aa-proxy-rs/src/protos/protos.proto:1337:
        //   1 = AUDIO_PCM     3 = VIDEO_H264_BP    5 = VIDEO_VP9
        //   2 = AUDIO_AAC_LC  4 = AUDIO_AAC_LC_ADTS 6 = VIDEO_AV1   7 = VIDEO_H265
        //
        // We send PCM (1) on every AV channel here. KIA Carnival 2024's SDR
        // didn't advertise an explicit video channel (no `av_channel` with
        // `stream_type=VIDEO`), so we don't yet know which channel (if any)
        // wants `type=VIDEO_H264_BP` (3). Once KIA acks PCM setups we can
        // try VIDEO on the audio_unknown channel (1) or one of the still-
        // unidentified channels (10/12/16).
        "audio_media" | "audio_speech" | "audio_system" | "audio_alarm"
        | "audio_unknown" => {
            // Body: `08 01` = field 1 (type) varint 1 (AUDIO_PCM).
            let mut body = Vec::with_capacity(2);
            body.push(0x08);
            body.push(0x01);
            Some(OutboundMessage::new(channel_id as u8, 0x8000, body))
        }
        // av_input is the microphone source channel (HU sends mic audio TO
        // phone). The modern AAP message for this channel is **NOT** the
        // shared `Setup{type=AUDIO_PCM}` (msg 0x8000) we use for sink
        // channels — it's `MicrophoneRequest{open=...}` (msg 0x8005, alias
        // `MEDIA_MESSAGE_MICROPHONE_REQUEST = 32773` in aa-proxy-rs
        // protos.proto:1530, alias `AV_INPUT_OPEN_REQUEST` in old aasdk).
        //
        // KIA empirically rejects msg 0x8000 on this channel with
        // UNEXPECTED_MESSAGE (kia-test/aabox-aapd.log:12:00:05.065387
        // channel=7). The aasdk-era `AVInputServiceChannel.cpp` accepts
        // BOTH 0x8000 (SETUP_REQUEST) and 0x8005 (AV_INPUT_OPEN_REQUEST),
        // but KIA's modern firmware only recognizes 0x8005 as valid setup
        // on a source channel.
        //
        // Modern proto (aa-proxy-rs/src/protos/protos.proto:496):
        //   message MicrophoneRequest {
        //     required bool  open        = 1;
        //     optional bool  anc_enabled = 2;
        //     optional bool  ec_enabled  = 3;
        //     optional int32 max_unacked = 4;
        //   }
        //
        // Direction: HU = mic source, phone = mic sink. HU streams cabin-mic
        // audio TO the phone via `MEDIA_MESSAGE_DATA` (msg 0x0000). The phone
        // controls when the mic is hot by sending `MicrophoneRequest{open=
        // true/false}` to the HU. HU acks with `MicrophoneResponse{status,
        // session_id}` (msg 0x8006) — handled by the dispatch branch below.
        //
        // We send `open=false` here — we're not in a voice session yet, the
        // mic should stay closed. This both engages the channel (KIA gets
        // its expected msg 0x8005) and tells KIA not to stream mic audio.
        // When a voice session later starts, we send `MicrophoneRequest{
        // open=true}` (wire `08 01`) and KIA begins streaming audio frames.
        //
        // Wire: `08 00` = field 1 (open) varint 0 (false).
        "av_input" => {
            let mut body = Vec::with_capacity(2);
            body.push(0x08);
            body.push(0x00);
            Some(OutboundMessage::new(channel_id as u8, 0x8005, body))
        }
        "video" => {
            // Body: `08 03` = field 1 (type) varint 3 (VIDEO_H264_BP).
            let mut body = Vec::with_capacity(2);
            body.push(0x08);
            body.push(0x03);
            Some(OutboundMessage::new(channel_id as u8, 0x8000, body))
        }
        // Sensor — phone sends `SensorRequest` (modern AAP) /
        // `SensorStartRequestMessage` (aasdk-era) with two REQUIRED fields:
        //   field 1: SensorType (varint enum) = DRIVING_STATUS_DATA (13)
        //   field 2: min_update_period / refresh_interval (varint int64)
        //
        // gromaudio/hu_aad.c documents that KIA-class HUs send
        // `SensorEventIndication { driving_status = PARKED }` only AFTER the
        // phone subscribes via SensorStartRequest with sensor_type=
        // DRIVING_STATUS — and that indication is what unblocks the
        // "Reading USB device" UI splash. Subscribing to LOCATION (1)
        // doesn't drive that.
        //
        // **Modern proto bug we hit on 2026-05-24** — the milek7 / aa-proxy-rs
        // proto declares `required int64 min_update_period = 2;` (proto2
        // required). Our previous wire `08 0d` (field 1 only) is missing the
        // required field — KIA's parser rejects with UNEXPECTED_MESSAGE on
        // channel 3 (kia-test/aabox-aapd.log:12:00:05.064994). Same root
        // cause as the ChannelOpenRequest priority=0 and Setup type=AUDIO_PCM
        // proto2-required-field issues we already fixed earlier in the day.
        //
        // gromaudio/android_external_aoaptest/hu_aap.c::aa_pro_sen_b01 also
        // hard-codes the expected wire as `[0x08, type, 0x10, period]` (6
        // bytes total including the 2-byte msg_id header) — confirming
        // field 2 must be present even when the value is 0.
        //
        // Wire: `08 0d 10 00` = field 1 (sensor_type=DRIVING_STATUS_DATA=13)
        // and field 2 (min_update_period=0 → "notify on any change").
        "sensor" => {
            let mut body = Vec::with_capacity(4);
            body.push(0x08);
            body.push(0x0d);
            body.push(0x10);
            body.push(0x00);
            Some(OutboundMessage::new(channel_id as u8, 0x8001, body))
        }
        // Input — phone sends BindingRequest with `scan_codes` listing the
        // Android KeyEvent KEYCODE_* values the phone will accept. A real
        // Pixel binds the AA-relevant set documented in
        // /tmp/gearhead-research/aacs/AAServer/src/InputChannelHandler.cpp.
        // Empty body would still serialize but doesn't bind any keys —
        // KIA may treat it as "phone provides no keys" and never push
        // touch/key events.
        //
        // Keycodes (Android KeyEvent constants):
        //   3=HOME, 4=BACK, 5=CALL, 6=ENDCALL,
        //   19=DPAD_UP, 20=DPAD_DOWN, 21=DPAD_LEFT, 22=DPAD_RIGHT, 23=DPAD_CENTER,
        //   84=SEARCH, 85=MEDIA_PLAY_PAUSE, 87=MEDIA_NEXT, 88=MEDIA_PREVIOUS
        //
        // BindingRequest.scan_codes is `repeated int32` (proto3 → packed
        // by default). Wire encoding:
        //   tag = (1 << 3) | 2 = 0x0a    (field 1, length-delimited)
        //   len = 13 bytes (each varint is 1 byte since all values < 128)
        //   then 13 varint bytes back-to-back.
        "input" => {
            let codes: [u8; 13] = [
                0x03, 0x04, 0x05, 0x06, 0x13, 0x14, 0x15, 0x16, 0x17,
                0x54, 0x55, 0x57, 0x58,
            ];
            let mut body = Vec::with_capacity(2 + codes.len());
            body.push(0x0a);
            body.push(codes.len() as u8);
            body.extend_from_slice(&codes);
            Some(OutboundMessage::new(channel_id as u8, 0x8002, body))
        }
        // Navigation/Bluetooth — no canonical setup message. Leave them alone
        // until we have evidence KIA needs one.
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_sensor() -> SensorHandler {
        SensorHandler::new(std::env::temp_dir().join("aabox-test-gps-unused.log"))
    }

    #[test]
    fn ping_request_echoes_timestamp() {
        let req = PingRequest { timestamp: 12345 };
        let mut body = Vec::new();
        req.encode(&mut body).unwrap();
        let sdr = services::full_response();
        let outs = dispatch(
            ChannelId::Control as u8,
            ControlMessageId::PingRequest as u16,
            &body,
            &sdr,
            &empty_sensor(),
        )
        .unwrap();
        assert_eq!(outs.len(), 1);
        assert_eq!(outs[0].message_id, ControlMessageId::PingResponse as u16);
        let resp = PingResponse::decode(&outs[0].payload[..]).unwrap();
        assert_eq!(resp.timestamp, 12345);
    }

    #[test]
    fn service_discovery_response_triggers_channel_open_requests() {
        // Source-role: when the HU sends us SDR-Response, we issue a
        // ChannelOpenRequest for every classifiable projection channel. Feed
        // our own full_response() back at the dispatcher (it carries the
        // canonical descriptor mix: sensor, video, input, audio, av_input,
        // navigation) and check we emit opens for all of them.
        let sdr_from_hu = services::full_response();
        let mut body = Vec::new();
        sdr_from_hu.encode(&mut body).unwrap();
        let sdr_template = services::full_response();
        let outs = dispatch(
            ChannelId::Control as u8,
            ControlMessageId::ServiceDiscoveryResponse as u16,
            &body,
            &sdr_template,
            &empty_sensor(),
        )
        .unwrap();
        assert!(
            !outs.is_empty(),
            "expected at least one ChannelOpenRequest"
        );
        let opens: Vec<_> = outs
            .iter()
            .filter(|o| o.message_id == ControlMessageId::ChannelOpenRequest as u16)
            .collect();
        for out in &opens {
            let parsed = ChannelOpenRequest::decode(&out.payload[..]).unwrap();
            // Every HU channel ID in full_response() is positive, and the
            // open is addressed to the service channel itself (not Control).
            assert!(parsed.channel_id > 0);
            assert_eq!(out.channel as i32, parsed.channel_id);
        }
        assert!(opens.len() >= 6, "got {} opens", opens.len());
        // Beyond opens, the SDR-Response handler also emits:
        //   - control-channel proactive status: BatteryStatusNotification
        //     (0x0017) and CallAvailabilityStatus (0x0018). AudioFocusRequest
        //     (0x0012) was REMOVED 2026-05-24 because KIA replies
        //     UNEXPECTED_MESSAGE when phones send it unsolicited (modern HUs
        //     grant focus proactively via AudioFocusNotification after each
        //     audio-channel Setup).
        //   - per-service Setup on service channels (msg ids 0x8000+)
        let setups: Vec<_> = outs
            .iter()
            .filter(|o| o.message_id != ControlMessageId::ChannelOpenRequest as u16)
            .collect();
        assert!(!setups.is_empty(), "expected at least one Setup message");
        let mut svc_setups = 0;
        let mut ctrl_msgs = 0;
        for s in &setups {
            if s.channel == ChannelId::Control as u8 {
                ctrl_msgs += 1;
            } else {
                svc_setups += 1;
                assert!(
                    s.message_id >= 0x8000,
                    "service-channel setup has unexpected msg {:#x}",
                    s.message_id
                );
            }
        }
        assert!(svc_setups > 0, "expected per-service Setup msgs");
        assert!(ctrl_msgs >= 2, "expected ≥2 control msgs, got {}", ctrl_msgs);
    }

    #[test]
    fn channel_open_request_acks_ok() {
        let req = ChannelOpenRequest {
            priority: 0,
            channel_id: ChannelId::Video as i32,
        };
        let mut body = Vec::new();
        req.encode(&mut body).unwrap();
        let sdr = services::full_response();
        let outs = dispatch(
            ChannelId::Control as u8,
            ControlMessageId::ChannelOpenRequest as u16,
            &body,
            &sdr,
            &empty_sensor(),
        )
        .unwrap();
        assert_eq!(outs.len(), 1);
        let resp = ChannelOpenResponse::decode(&outs[0].payload[..]).unwrap();
        assert_eq!(resp.status, status::Enum::Ok as i32);
    }

    #[test]
    fn audio_focus_request_replies_gain() {
        let req = AudioFocusRequest {
            audio_focus_type: aabox_proto::enums::audio_focus_type::Enum::Gain as i32,
        };
        let mut body = Vec::new();
        req.encode(&mut body).unwrap();
        let sdr = services::full_response();
        let outs = dispatch(
            ChannelId::Control as u8,
            ControlMessageId::AudioFocusRequest as u16,
            &body,
            &sdr,
            &empty_sensor(),
        )
        .unwrap();
        assert_eq!(outs.len(), 1);
        let resp = AudioFocusResponse::decode(&outs[0].payload[..]).unwrap();
        assert_eq!(resp.audio_focus_state, audio_focus_state::Enum::Gain as i32);
    }

    #[test]
    fn navigation_focus_request_replies_native() {
        let req = NavigationFocusRequest { r#type: 0 };
        let mut body = Vec::new();
        req.encode(&mut body).unwrap();
        let sdr = services::full_response();
        let outs = dispatch(
            ChannelId::Control as u8,
            ControlMessageId::NavigationFocusRequest as u16,
            &body,
            &sdr,
            &empty_sensor(),
        )
        .unwrap();
        assert_eq!(outs.len(), 1);
        let resp = NavigationFocusResponse::decode(&outs[0].payload[..]).unwrap();
        assert_eq!(resp.r#type, 1);
    }

    #[test]
    fn voice_session_request_silent() {
        let sdr = services::full_response();
        let outs = dispatch(
            ChannelId::Control as u8,
            ControlMessageId::VoiceSessionRequest as u16,
            &[],
            &sdr,
            &empty_sensor(),
        )
        .unwrap();
        assert!(outs.is_empty());
    }
}
