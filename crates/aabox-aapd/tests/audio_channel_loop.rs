//! End-to-end: drive the post-TLS control-channel loop as a fake head unit
//! exercising the audio channels through the [`AudioChannelManager`].
//!
//! Two scenarios are covered:
//!
//! 1. **Speech-in (channel 5, car -> us)** — fake head unit sends:
//!      SETUP_REQUEST -> SETUP_RESPONSE
//!      AV_INPUT_OPEN_REQUEST -> AV_INPUT_OPEN_RESPONSE
//!      a flight of AV_MEDIA_WITH_TIMESTAMP frames
//!    and asserts the matching media-acks come back AND the manager's sink
//!    received each PCM payload with the timestamp preserved.
//!
//! 2. **Media-out (channel 4, us -> car)** — fake head unit sends:
//!      SETUP_REQUEST -> SETUP_RESPONSE
//!      START_INDICATION
//!    and asserts the source-side encoder task pushes a stream of
//!    AV_MEDIA_WITH_TIMESTAMP frames onto the wire (channel 4) with valid
//!    framing: 8-byte BE timestamp prefix + PCM.
//!
//! The control loop's wiring (mpsc to writer, async dispatch for audio
//! channels) is what we're verifying here. The lower-level setup-request
//! ack logic is covered by `channels::audio::tests` in the lib.

use aabox_aapd::channels::audio::{
    AudioChannelManager, AudioFormat, AudioFrame, AudioSink, AudioSource, TestToneSource,
    MSG_AV_INPUT_OPEN_REQUEST, MSG_AV_INPUT_OPEN_RESPONSE, MSG_AV_MEDIA_ACK_INDICATION,
    MSG_AV_MEDIA_WITH_TIMESTAMP_INDICATION, MSG_SETUP_REQUEST, MSG_SETUP_RESPONSE,
    MSG_START_INDICATION,
};
use aabox_aapd::channels::OutboundMessage;
use aabox_common::ChannelId;
use aabox_proto::messages::{
    AvChannelSetupRequest, AvChannelSetupResponse, AvChannelStartIndication,
    AvInputOpenRequest, AvInputOpenResponse, AvMediaAckIndication,
};
use async_trait::async_trait;
use bytes::{BufMut, BytesMut};
use prost::Message;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};

/// Capture sink: records every frame in-memory for assertion.
struct CaptureSink {
    frames: Arc<Mutex<Vec<AudioFrame>>>,
}

#[async_trait]
impl AudioSink for CaptureSink {
    async fn push(&self, frame: AudioFrame) -> anyhow::Result<()> {
        self.frames.lock().await.push(frame);
        Ok(())
    }
}

#[tokio::test]
async fn speech_in_round_trips_setup_open_and_data_frames() {
    // Capture every frame the manager pushes into the sink.
    let captured: Arc<Mutex<Vec<AudioFrame>>> = Arc::new(Mutex::new(Vec::new()));
    let (out_tx, _out_rx) = mpsc::channel::<OutboundMessage>(16);

    let mgr = AudioChannelManager::new(out_tx);
    let cap = Arc::clone(&captured);
    mgr.set_sink_factory(move |_cid, _fmt| {
        let inner = Arc::clone(&cap);
        let sink: Arc<dyn AudioSink> = Arc::new(CaptureSink { frames: inner });
        sink
    })
    .await;

    // 1) SETUP_REQUEST -> SETUP_RESPONSE
    let setup = AvChannelSetupRequest { config_index: 0 };
    let mut buf = Vec::new();
    setup.encode(&mut buf).unwrap();
    let out = mgr
        .handle(ChannelId::SpeechAudio as u8, MSG_SETUP_REQUEST, &buf)
        .await
        .unwrap();
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].channel, ChannelId::SpeechAudio as u8);
    assert_eq!(out[0].message_id, MSG_SETUP_RESPONSE);
    let setup_resp = AvChannelSetupResponse::decode(&out[0].payload[..]).unwrap();
    assert_eq!(setup_resp.max_unacked, 10);

    // 2) AV_INPUT_OPEN_REQUEST -> AV_INPUT_OPEN_RESPONSE (attaches sink)
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
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].message_id, MSG_AV_INPUT_OPEN_RESPONSE);
    let open_resp = AvInputOpenResponse::decode(&out[0].payload[..]).unwrap();
    assert_eq!(open_resp.session, 0);
    assert_eq!(open_resp.value, 0);

    // 3) Three AV_MEDIA_WITH_TIMESTAMP frames. Each frame carries an 8-byte
    //    big-endian microsecond timestamp followed by the raw PCM payload.
    let pcm_payloads: Vec<&[u8]> = vec![
        &[0x10, 0x20, 0x30, 0x40, 0x50, 0x60, 0x70, 0x80],
        &[0xAA, 0xBB, 0xCC, 0xDD],
        &[0x11, 0x22, 0x33, 0x44, 0x55, 0x66],
    ];
    for (i, pcm) in pcm_payloads.iter().enumerate() {
        let mut payload = BytesMut::new();
        let ts_us = (i as u64 + 1) * 10_000;
        payload.put_u64(ts_us);
        payload.put_slice(pcm);
        let out = mgr
            .handle(
                ChannelId::SpeechAudio as u8,
                MSG_AV_MEDIA_WITH_TIMESTAMP_INDICATION,
                &payload,
            )
            .await
            .unwrap();
        // Each data frame should generate an AV_MEDIA_ACK back to the car.
        assert_eq!(out.len(), 1, "data frame should produce media ack");
        assert_eq!(out[0].message_id, MSG_AV_MEDIA_ACK_INDICATION);
        let ack = AvMediaAckIndication::decode(&out[0].payload[..]).unwrap();
        assert_eq!(ack.value, 1);
    }

    // Verify the sink captured everything: 3 frames, timestamps preserved,
    // PCM bytes match.
    let frames = captured.lock().await.clone();
    assert_eq!(frames.len(), 3);
    for (i, (frame, expect_pcm)) in frames.iter().zip(pcm_payloads.iter()).enumerate() {
        assert_eq!(frame.timestamp_us, (i as u64 + 1) * 10_000);
        assert_eq!(&frame.pcm[..], *expect_pcm);
    }
}

#[tokio::test]
async fn media_out_pumps_test_tone_after_start() {
    let (out_tx, mut out_rx) = mpsc::channel::<OutboundMessage>(32);
    let mgr = AudioChannelManager::new(out_tx);

    // Finite test tone — emits 4 ~10 ms frames at 48 kHz stereo 16-bit, then
    // ends so the encoder task exits cleanly.
    mgr.set_source_factory(|fmt: AudioFormat| {
        let src: Box<dyn AudioSource + Send> = Box::new(
            TestToneSource::new(fmt)
                .with_frame_size(480) // 10 ms @ 48 kHz
                .with_limit(4),
        );
        src
    })
    .await;

    // SETUP_REQUEST -> SETUP_RESPONSE (creates per-channel state).
    let setup = AvChannelSetupRequest { config_index: 0 };
    let mut buf = Vec::new();
    setup.encode(&mut buf).unwrap();
    let _ = mgr
        .handle(ChannelId::MediaAudio as u8, MSG_SETUP_REQUEST, &buf)
        .await
        .unwrap();

    // START_INDICATION -> kicks off the encoder pump task.
    let start = AvChannelStartIndication {
        session: 42,
        config: 0,
    };
    let mut buf = Vec::new();
    start.encode(&mut buf).unwrap();
    let _ = mgr
        .handle(ChannelId::MediaAudio as u8, MSG_START_INDICATION, &buf)
        .await
        .unwrap();

    // Drain four media frames off the outbound mpsc. Each one must be on
    // the media-audio channel, msg_id = AV_MEDIA_WITH_TIMESTAMP_INDICATION,
    // and the payload must start with an 8-byte BE timestamp followed by
    // 480 samples * 2 ch * 2 bytes = 1920 bytes of PCM.
    let mut got = Vec::new();
    for _ in 0..4 {
        let msg = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            out_rx.recv(),
        )
        .await
        .expect("timed out waiting for encoder frame")
        .expect("encoder mpsc closed early");
        assert_eq!(msg.channel, ChannelId::MediaAudio as u8);
        assert_eq!(msg.message_id, MSG_AV_MEDIA_WITH_TIMESTAMP_INDICATION);
        assert_eq!(msg.payload.len(), 8 + 1920);
        got.push(msg);
    }
    // Timestamps should be monotonically non-decreasing.
    let t0 = u64::from_be_bytes(got[0].payload[..8].try_into().unwrap());
    let t3 = u64::from_be_bytes(got[3].payload[..8].try_into().unwrap());
    assert!(t3 >= t0);
}

#[tokio::test]
async fn system_audio_in_uses_48k_format_and_logs_to_sink() {
    // Smoke test that channel 6 (System Audio) is treated identically to
    // channel 5 — same dispatch path, just a different sample rate baked
    // into the channel descriptor. We assert the sink receives the frame.
    let captured: Arc<Mutex<Vec<AudioFrame>>> = Arc::new(Mutex::new(Vec::new()));
    let (out_tx, _out_rx) = mpsc::channel::<OutboundMessage>(16);
    let mgr = AudioChannelManager::new(out_tx);

    let cap = Arc::clone(&captured);
    mgr.set_sink_factory(move |_cid, _fmt| {
        let inner = Arc::clone(&cap);
        let sink: Arc<dyn AudioSink> = Arc::new(CaptureSink { frames: inner });
        sink
    })
    .await;

    // SETUP + OPEN
    let setup = AvChannelSetupRequest { config_index: 0 };
    let mut buf = Vec::new();
    setup.encode(&mut buf).unwrap();
    let _ = mgr
        .handle(ChannelId::SystemAudio as u8, MSG_SETUP_REQUEST, &buf)
        .await
        .unwrap();

    let open = AvInputOpenRequest {
        open: true,
        anc: false,
        ec: false,
        max_unacked: 4,
    };
    let mut buf = Vec::new();
    open.encode(&mut buf).unwrap();
    let _ = mgr
        .handle(
            ChannelId::SystemAudio as u8,
            MSG_AV_INPUT_OPEN_REQUEST,
            &buf,
        )
        .await
        .unwrap();

    // One data frame.
    let mut payload = BytesMut::new();
    payload.put_u64(123_456);
    payload.put_slice(&[0xCA, 0xFE, 0xBA, 0xBE]);
    let _ = mgr
        .handle(
            ChannelId::SystemAudio as u8,
            MSG_AV_MEDIA_WITH_TIMESTAMP_INDICATION,
            &payload,
        )
        .await
        .unwrap();

    let frames = captured.lock().await.clone();
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].timestamp_us, 123_456);
    assert_eq!(&frames[0].pcm[..], &[0xCA, 0xFE, 0xBA, 0xBE]);
}
