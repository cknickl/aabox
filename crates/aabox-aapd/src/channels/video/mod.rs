//! Video channel — H.264 streaming, source side.
//!
//! Video shares the AV-channel msg-id namespace with audio (the proto file
//! is literally `AVChannelMessageIdsEnum.proto`). Where audio's handler is
//! a pure function (audio is just per-frame ack-and-log right now), video
//! is **stateful**: we have to maintain a streaming task that pumps H.264
//! frames onto the wire between START_INDICATION and STOP_INDICATION.
//!
//! Architecture:
//!
//! ```text
//!   control_channel::run
//!     ├── owns a VideoPipeline { outbound_rx, channel handle }
//!     ├── select! includes `outbound_rx.recv()` to drain video frames
//!     │     onto the encrypted TLS tunnel
//!     └── dispatch -> channels::video::handle(payload, &pipeline_handle)
//!                       which, on START, .start(session, config) → spawns
//!                       a streaming task off the pipeline handle. Frames
//!                       flow back through outbound_rx.
//! ```
//!
//! The streaming task itself owns a [`source::VideoSource`]. Phase 1 ships
//! [`source::TestPatternVideoSource`], a precomputed-Annex-B replay loop.
//! Phase 2 will introduce a screen-capture source (see [`PHASE2_NOTES`]
//! below).

use super::{encode_outbound, OutboundMessage};
use aabox_proto::enums::av_channel_setup_status;
use aabox_proto::messages::{
    AvChannelSetupRequest, AvChannelSetupResponse, AvChannelStartIndication,
    AvChannelStopIndication, AvMediaAckIndication,
};
use anyhow::{anyhow, Result};
use prost::Message;

pub mod nal;
pub mod pipeline;
pub mod source;

pub use pipeline::{VideoPipeline, VideoPipelineHandle, MSG_AV_MEDIA_WITH_TIMESTAMP_INDICATION};
pub use source::{TestPatternProfile, TestPatternVideoSource, VideoFrame, VideoSource};

// Re-export the AV control message ids we share with audio. Keeps the
// control loop's dispatch table happy without a circular import.
pub use crate::channels::audio::{
    MSG_AV_INPUT_OPEN_REQUEST, MSG_AV_INPUT_OPEN_RESPONSE, MSG_AV_MEDIA_ACK_INDICATION,
    MSG_AV_MEDIA_INDICATION, MSG_SETUP_REQUEST, MSG_SETUP_RESPONSE, MSG_START_INDICATION,
    MSG_STOP_INDICATION,
};

/// Max-unacked we advertise in the SETUP_RESPONSE; mirrored as the
/// pipeline's in-flight cap.
pub const VIDEO_MAX_UNACKED: u32 = 10;

/// Handle one video-channel message. Unlike audio (which is pure), we
/// take a `&VideoPipelineHandle` to mutate stream state.
pub fn handle(
    channel: u8,
    msg_id: u16,
    payload: &[u8],
    pipeline: &VideoPipelineHandle,
) -> Result<Vec<OutboundMessage>> {
    match msg_id {
        MSG_SETUP_REQUEST => {
            let req = AvChannelSetupRequest::decode(payload)
                .map_err(|e| anyhow!("decode AVChannelSetupRequest: {e}"))?;
            tracing::info!(
                channel,
                config_index = req.config_index,
                "video: SETUP_REQUEST -> OK"
            );
            let resp = AvChannelSetupResponse {
                media_status: av_channel_setup_status::Enum::Ok as i32,
                max_unacked: VIDEO_MAX_UNACKED,
                configs: vec![req.config_index],
            };
            Ok(vec![encode_outbound(channel, MSG_SETUP_RESPONSE, &resp)])
        }
        MSG_START_INDICATION => {
            let ind = AvChannelStartIndication::decode(payload)
                .map_err(|e| anyhow!("decode AVChannelStartIndication: {e}"))?;
            tracing::info!(
                channel,
                session = ind.session,
                config = ind.config,
                "video: START_INDICATION -> begin streaming"
            );
            pipeline.start(ind.session, ind.config);
            Ok(vec![])
        }
        MSG_STOP_INDICATION => {
            let _ = AvChannelStopIndication::decode(payload);
            tracing::info!(channel, "video: STOP_INDICATION -> stop streaming");
            pipeline.stop();
            Ok(vec![])
        }
        MSG_AV_MEDIA_ACK_INDICATION => {
            match AvMediaAckIndication::decode(payload) {
                Ok(a) => {
                    tracing::debug!(
                        channel,
                        session = a.session,
                        value = a.value,
                        "video: media ack"
                    );
                    pipeline.on_media_ack();
                }
                Err(e) => tracing::warn!(channel, "video: bad media ack: {e}"),
            }
            Ok(vec![])
        }
        // Anything else lands here only if the car is doing something
        // weird; log and ignore. The AV channel doesn't formally accept
        // incoming AV_MEDIA frames on the *video* descriptor (it's
        // source-out only) but we tolerate them for forward compat.
        other => {
            tracing::debug!(
                channel,
                msg_id = format!("0x{:04x}", other),
                "video: unhandled msg id (ignored)"
            );
            Ok(vec![])
        }
    }
}

// ============================================================================
// PHASE 2 NOTES — hardware-accelerated screen capture
// ============================================================================
//
// Phase 1 (this module) streams a precomputed test pattern from an embedded
// Annex-B file. That's enough to prove the protocol layer end to end:
//   - SETUP_REQUEST/RESPONSE flow
//   - START / STOP lifecycle on the wire
//   - AV_MEDIA_WITH_TIMESTAMP_INDICATION frames carrying valid H.264 NALs
//   - flow control via AV_MEDIA_ACK_INDICATION + max_unacked
//   - large-payload fragmentation (FIRST/MIDDLE/LAST with EXTENDED size)
//
// Phase 2 swaps the source for a real one: an Android-side screen capture
// piped through the hardware H.264 encoder via MediaCodec. Sketch:
//
//   1) New crate `crates/aabox-android-mediacodec/`.
//        - Pure-Rust facing API:
//            pub struct MediaCodecH264Source { ... }
//            impl VideoSource for MediaCodecH264Source { ... }
//          plug-compatible with `TestPatternVideoSource`. The pipeline
//          doesn't change; only the construction switches.
//        - Internally, an `android_mediacodec_sys` raw-FFI shim (or
//          ndk-sys's MediaCodec bindings if those mature) drives the
//          hardware encoder.
//
//   2) New JNI bridge layer (Kotlin/Java glue):
//        - android/app/src/main/kotlin/app/aabox/aapd/VideoCapture.kt
//        - Holds a MediaProjection token obtained from the foreground
//          service. Creates a VirtualDisplay backed by the MediaCodec
//          encoder's input Surface.
//        - Exposes `nativeAttachCaptureSurface(MediaCodec)` from Kotlin
//          to Rust so we don't have to call the framework
//          MediaProjection API from JNI (it's an obnoxious AIDL surface).
//
//   3) MediaCodec configuration to produce Annex-B output:
//        - KEY_MIME              = "video/avc"
//        - KEY_PROFILE           = AVCProfileBaseline
//        - KEY_LEVEL             = AVCLevel40
//        - KEY_BIT_RATE          = ~6 Mbps
//        - KEY_FRAME_RATE        = 30 or 60 (matches advertised video_fps)
//        - KEY_I_FRAME_INTERVAL  = 1 (keyframe per second; matches what
//          the car expects after every START)
//        - On encoded-output the MediaCodec emits NALs already prefixed
//          with start codes in Annex-B form (default on AOSP).
//
//   4) Open questions to validate against the CM5 build:
//        - Does the CM5's Android image include a working `media.codec`
//          service? (AOSP base says yes, but the kernel needs the V4L2
//          M2M AVC encoder driver, which on RPi5 mainline is still
//          partial. Spot-check `MediaCodecList.getCodecInfos()`.)
//        - Does MediaProjection require user consent each session? If
//          so, the foreground service needs a notification that the user
//          taps once at first paired-car connect.
//        - Surface-input vs. ByteBuffer-input: hardware encoders generally
//          require surface input. Our VirtualDisplay path is the standard
//          AOSP screen-record pattern (`screenrecord` itself uses this).
//
//   5) Glue back into the pipeline:
//        - `VideoPipelineHandle::start(session, config)` currently always
//          builds a `TestPatternVideoSource`. In Phase 2 we make this
//          configurable at pipeline construction time, with the runtime
//          source decided by a CLI flag / Android intent extra. Default
//          on a real device: MediaCodec source. Default in tests / DHU
//          mode: test pattern.
//
// Nothing in Phase 1 commits us to a specific Phase 2 design. The
// `VideoSource` trait is the seam.
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use aabox_common::ChannelId;

    #[test]
    fn video_setup_acks_ok_with_max_unacked() {
        let pipeline = VideoPipeline::new(VIDEO_MAX_UNACKED);
        let h = pipeline.handle();
        let req = AvChannelSetupRequest { config_index: 0 };
        let mut buf = Vec::new();
        req.encode(&mut buf).unwrap();
        let out = handle(ChannelId::Video as u8, MSG_SETUP_REQUEST, &buf, &h).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].message_id, MSG_SETUP_RESPONSE);
        let resp = AvChannelSetupResponse::decode(&out[0].payload[..]).unwrap();
        assert_eq!(resp.media_status, av_channel_setup_status::Enum::Ok as i32);
        assert_eq!(resp.max_unacked, VIDEO_MAX_UNACKED);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn video_start_indication_kicks_pipeline() {
        let pipeline = VideoPipeline::new(VIDEO_MAX_UNACKED);
        let h = pipeline.handle();
        let ind = AvChannelStartIndication { session: 5, config: 1 };
        let mut buf = Vec::new();
        ind.encode(&mut buf).unwrap();
        let out = handle(ChannelId::Video as u8, MSG_START_INDICATION, &buf, &h).unwrap();
        // START does not produce an immediate reply on the control thread;
        // frames flow asynchronously on outbound_rx.
        assert!(out.is_empty());
        assert_eq!(h.session_id(), 5);
        // Tear down: stop the spawned task so it doesn't outlive the test.
        h.stop();
    }
}
