//! The `VideoChannel` task — owns a `VideoSource` and pumps its frames into
//! the control loop's outbound mpsc as `OutboundMessage`s.
//!
//! Lifecycle:
//!   1. The control loop spawns one of these at startup (idle).
//!   2. On `SETUP_REQUEST -> SETUP_RESPONSE` we ack with OK and don't touch
//!      the pipeline; we wait for START before producing bytes.
//!   3. On `START_INDICATION` we kick the source: open a new generation,
//!      start a sub-task that reads from `VideoSource::next_frame` and
//!      writes `OutboundMessage`s for each access unit.
//!   4. On `STOP_INDICATION` we cancel the generation: the sub-task exits,
//!      the source's `close()` is called, no more frames hit the wire.
//!   5. On `AV_MEDIA_ACK_INDICATION` we tick the in-flight counter so flow
//!      control doesn't run away from the car (max_unacked = 10, see
//!      services.rs).
//!
//! Concurrency model: the pipeline owns a `tokio::sync::watch::Sender<u64>`
//! for the "current generation". The streaming sub-task watches it and
//! exits when the value bumps (i.e. on STOP or a re-START). This is a
//! lockfree cooperative cancellation primitive that doesn't require an
//! abort handle.

use super::source::{
    ScreenRecordVideoSource, TestPatternProfile, TestPatternVideoSource, VideoSource,
};
use crate::channels::OutboundMessage;
use aabox_common::ChannelId;
use bytes::{BufMut, BytesMut};
use std::sync::atomic::{AtomicI32, AtomicU32, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, watch};

/// AV_MEDIA_WITH_TIMESTAMP_INDICATION on an AV channel uses msg_id 0x0000.
/// The plaintext payload layout is:
///   [u64 BE PTS (microseconds)] [Annex-B H.264 bytes]
pub const MSG_AV_MEDIA_WITH_TIMESTAMP_INDICATION: u16 = 0x0000;

/// State shared between the channel handler (which lives in the dispatch
/// path) and the streaming task (which owns the source). All fields are
/// atomic / `mpsc` so the handler can poke them without holding a lock.
pub struct VideoPipeline {
    /// Outbound queue: every frame the streaming task produces lands here,
    /// and the control loop drains it onto the wire.
    outbound_tx: mpsc::Sender<OutboundMessage>,
    /// Receiver half of the same queue. The control loop owns this end.
    outbound_rx: Mutex<Option<mpsc::Receiver<OutboundMessage>>>,
    /// "Current generation" counter. Each START increments it; each
    /// streaming sub-task checks `*watch_rx.borrow() == my_gen` and
    /// exits when it differs.
    generation_tx: watch::Sender<u64>,
    /// Session ID the car established at START. Used in MEDIA_ACK responses.
    session_id: Arc<AtomicI32>,
    /// In-flight outstanding frame count (frames sent but not yet acked).
    /// Bounded by the AVChannelSetupResponse.max_unacked we advertised.
    in_flight: Arc<AtomicU32>,
    /// Hard cap on in-flight frames (max_unacked we told the car).
    max_in_flight: u32,
    /// Channel byte to use on the wire. The HU assigns this in its
    /// ServiceDiscoveryResponse — KIA Carnival 2024 puts the video sink on
    /// channel 1, NOT our internal canonical ChannelId::Video (3). The
    /// dispatch path writes this byte when it learns the HU's mapping
    /// (typically from the VideoFocusNotification arrival channel).
    wire_channel: Arc<AtomicU8>,
}

impl VideoPipeline {
    /// Create a new pipeline. The returned object will be moved into the
    /// control loop; the channel handler holds a [`VideoPipelineHandle`]
    /// cloned from it.
    pub fn new(max_in_flight: u32) -> Self {
        let (outbound_tx, outbound_rx) = mpsc::channel(64);
        let (generation_tx, _) = watch::channel(0u64);
        Self {
            outbound_tx,
            outbound_rx: Mutex::new(Some(outbound_rx)),
            generation_tx,
            session_id: Arc::new(AtomicI32::new(0)),
            in_flight: Arc::new(AtomicU32::new(0)),
            max_in_flight,
            // Initial value of 255 (= ChannelId::None) is a sentinel meaning
            // "we don't know KIA's video channel byte yet". Setting it to
            // the canonical ChannelId::Video=3 would land it on KIA Carnival's
            // SENSOR channel byte — and KIA sends `SensorStartResponse`
            // (msg 0x8002) on the sensor channel right after our
            // `SensorStartRequest`. Our cross-channel STOP_INDICATION
            // intercept matches `msg 0x8002 on the video wire_channel`,
            // so a sentinel here prevents that false stop. The real value
            // gets written by `set_wire_channel()` when
            // `VideoFocusNotification` arrives.
            wire_channel: Arc::new(AtomicU8::new(ChannelId::None as u8)),
        }
    }

    /// Pull out the receiver end of the outbound queue. Must be called
    /// exactly once by the owner of the I/O loop.
    pub fn take_outbound_rx(&self) -> Option<mpsc::Receiver<OutboundMessage>> {
        self.outbound_rx.lock().expect("video pipeline mutex").take()
    }

    /// Build a cheap handle that the channel handler holds and uses to
    /// request start/stop on inbound messages.
    pub fn handle(&self) -> VideoPipelineHandle {
        VideoPipelineHandle {
            outbound_tx: self.outbound_tx.clone(),
            generation_tx: self.generation_tx.clone(),
            session_id: Arc::clone(&self.session_id),
            in_flight: Arc::clone(&self.in_flight),
            max_in_flight: self.max_in_flight,
            wire_channel: Arc::clone(&self.wire_channel),
        }
    }
}

/// Cheap clone of the bits the channel handler needs. The actual streaming
/// task is spawned from here on START.
#[derive(Clone)]
pub struct VideoPipelineHandle {
    outbound_tx: mpsc::Sender<OutboundMessage>,
    generation_tx: watch::Sender<u64>,
    session_id: Arc<AtomicI32>,
    in_flight: Arc<AtomicU32>,
    max_in_flight: u32,
    wire_channel: Arc<AtomicU8>,
}

impl VideoPipelineHandle {
    /// Set the channel byte the streamer will write on. Called by the
    /// dispatch loop the first time it learns the HU's video channel
    /// assignment (typically from the arrival channel of the
    /// `VideoFocusNotification` msg 0x8008). Has effect on the next frame.
    pub fn set_wire_channel(&self, ch: u8) {
        self.wire_channel.store(ch, Ordering::SeqCst);
    }

    /// Read the current outbound wire channel byte. Used by the dispatch
    /// loop to gate `STOP_INDICATION` (msg 0x8002) against THE video channel
    /// specifically — msg 0x8002 on other channels means SensorStartResponse
    /// / BindingResponse / MicrophoneResponse.
    pub fn wire_channel(&self) -> u8 {
        self.wire_channel.load(Ordering::SeqCst)
    }
}

impl VideoPipelineHandle {
    /// Begin streaming. Cancels any previous generation. The default Phase
    /// 1 source is the embedded test pattern.
    pub fn start(&self, session: i32, config: u32) {
        self.session_id.store(session, Ordering::SeqCst);
        // Reset flow-control counter so a re-START doesn't carry over.
        self.in_flight.store(0, Ordering::SeqCst);

        // Bump generation: any previously running streamer will see this
        // and exit on its next iteration. We must subscribe BEFORE
        // sending — `send` on a watch channel with zero live receivers is
        // a no-op (returns Err), and we'd then hand the streamer a fresh
        // subscriber whose current value is the *old* generation, causing
        // the streamer to think it's already cancelled and exit on its
        // first iteration.
        let gen_rx = self.generation_tx.subscribe();
        let new_gen = self.generation_tx.borrow().wrapping_add(1);
        let _ = self.generation_tx.send(new_gen);

        // Phase 2 source preference: spawn Android's `screenrecord` against
        // the primary display so the car sees the actual Android UI. Falls
        // back to the embedded test pattern only if screenrecord can't be
        // spawned (binary missing, exec denied by SELinux, etc.) — this
        // happens on the build VM where /system/bin/screenrecord doesn't
        // exist, so the unit tests keep working.
        //
        // Config index from `Start{configuration_index}` selects what we
        // ask screenrecord to capture at:
        //   0 -> 1920x1080@30 (KIA Carnival 2024 config index 0)
        //   1 -> 1280x720@30  (matches our embedded 720p test pattern)
        //   2 -> 854x480@30   (matches embedded 480p)
        let (w, h, fps) = match config {
            0 => (1920u32, 1080u32, 30u32),
            1 => (1280, 720, 30),
            _ => (854, 480, 30),
        };
        let bitrate = match config {
            0 => 6_000_000u32, // 6 Mbps for 1080p
            1 => 3_000_000,    // 3 Mbps for 720p
            _ => 1_500_000,    // 1.5 Mbps for 480p
        };
        // 2026-05-26 diagnostic: force the embedded test pattern instead of
        // screenrecord. Screenrecord against a blank Android primary display
        // (no Launcher, headless userdebug) produces sparse h264 — variable
        // and low bitrate when the screen has nothing changing. The KIA
        // Carnival 2024 ran our pipeline through VideoFocusNotification +
        // Start but bailed after ~7 frames in ~720ms. Test pattern has
        // continuous motion and a known-good SPS/PPS+IDR triplet at the
        // top, plus a stable 30 fps cadence. If KIA accepts it → the bail
        // was screenrecord output; if it still bails → deeper protocol or
        // transport issue. Restore ScreenRecord path after diagnosis.
        let _ = (w, h, fps, bitrate);
        let profile = if config == 0 {
            TestPatternProfile::P1080p30
        } else {
            TestPatternProfile::P720p30
        };
        tracing::info!(
            ?profile,
            config,
            "video: using embedded test pattern (screenrecord path bypassed for diagnostic)"
        );
        let source: Box<dyn VideoSource> = Box::new(TestPatternVideoSource::new(profile));

        spawn_streamer(
            new_gen,
            gen_rx,
            self.outbound_tx.clone(),
            self.in_flight.clone(),
            self.max_in_flight,
            source,
            Arc::clone(&self.wire_channel),
        );

        tracing::info!(
            session,
            config,
            gen = new_gen,
            "video: pipeline started"
        );
    }

    /// Stop streaming. Bumps the generation watcher; the running streamer
    /// sees the change and exits at its next poll point.
    pub fn stop(&self) {
        let new_gen = self.generation_tx.borrow().wrapping_add(1);
        let _ = self.generation_tx.send(new_gen);
        self.in_flight.store(0, Ordering::SeqCst);
        tracing::info!(gen = new_gen, "video: pipeline stopped");
    }

    /// Called when an AV_MEDIA_ACK arrives — we can release one in-flight slot.
    pub fn on_media_ack(&self) {
        // Saturating subtract: if the car ever over-acks (e.g. ack after
        // restart), don't underflow.
        let _ = self.in_flight.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |v| {
            if v > 0 {
                Some(v - 1)
            } else {
                Some(0)
            }
        });
    }

    /// Current session ID (the most recent START indication's session).
    pub fn session_id(&self) -> i32 {
        self.session_id.load(Ordering::SeqCst)
    }
}

/// The streaming task: read from a `VideoSource`, encode each access unit
/// as an `OutboundMessage`, push it onto `outbound_tx`. Exits when the
/// generation counter changes (cooperative cancellation).
fn spawn_streamer(
    my_gen: u64,
    mut gen_rx: watch::Receiver<u64>,
    outbound_tx: mpsc::Sender<OutboundMessage>,
    in_flight: Arc<AtomicU32>,
    max_in_flight: u32,
    mut source: Box<dyn VideoSource>,
    wire_channel: Arc<AtomicU8>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            // Cancellation: did somebody bump the generation while we were
            // sleeping?
            if *gen_rx.borrow() != my_gen {
                tracing::debug!(my_gen, "video: streamer exiting (generation changed)");
                source.close();
                return;
            }

            // Backpressure: don't outrun the car's max_unacked. If we're
            // at the cap, wait either for an ack (in_flight drop) or for
            // cancellation. Polling here is fine — ack rate is per frame
            // and the contention window is small.
            while in_flight.load(Ordering::SeqCst) >= max_in_flight {
                tokio::select! {
                    _ = tokio::time::sleep(std::time::Duration::from_millis(5)) => {}
                    _ = gen_rx.changed() => {
                        if *gen_rx.borrow() != my_gen {
                            source.close();
                            return;
                        }
                    }
                }
            }

            let frame = match source.next_frame().await {
                Some(f) => f,
                None => {
                    tracing::info!(my_gen, "video: source exhausted, exiting streamer");
                    return;
                }
            };

            // Build the wire payload: [u64 BE PTS][Annex-B bytes].
            let mut payload =
                BytesMut::with_capacity(8 + frame.annex_b.len());
            payload.put_u64(frame.pts_us);
            payload.put_slice(&frame.annex_b);

            let out = OutboundMessage::new(
                wire_channel.load(Ordering::SeqCst),
                MSG_AV_MEDIA_WITH_TIMESTAMP_INDICATION,
                payload.freeze(),
            );

            // Send: if the consumer dropped, the control loop is gone and
            // we can exit.
            if outbound_tx.send(out).await.is_err() {
                tracing::info!(my_gen, "video: outbound receiver dropped; streamer exiting");
                source.close();
                return;
            }

            in_flight.fetch_add(1, Ordering::SeqCst);
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test(flavor = "current_thread")]
    async fn start_emits_frames_with_pts_prefix() {
        let pipeline = VideoPipeline::new(4);
        let handle = pipeline.handle();
        let mut rx = pipeline.take_outbound_rx().expect("rx");

        // In production this is done by `handle_frame` when the HU sends
        // `VideoFocusNotification`. Tests must do it manually because they
        // bypass the dispatch layer. Without it the streamer would write on
        // the sentinel channel 255 (ChannelId::None).
        handle.set_wire_channel(ChannelId::Video as u8);
        handle.start(/*session=*/ 7, /*config=*/ 1);

        let m0 = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("timeout waiting for video frame")
            .expect("first frame");

        assert_eq!(m0.channel, ChannelId::Video as u8);
        assert_eq!(m0.message_id, MSG_AV_MEDIA_WITH_TIMESTAMP_INDICATION);
        assert!(m0.payload.len() > 8, "payload should have PTS + bytes");
        // First 8 bytes are PTS = 0 (first frame).
        assert_eq!(&m0.payload[..8], &[0u8; 8]);
        // Annex-B begins immediately after.
        assert_eq!(&m0.payload[8..12], &[0, 0, 0, 1]);

        handle.stop();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn stop_cancels_streamer() {
        let pipeline = VideoPipeline::new(64);
        let handle = pipeline.handle();
        let mut rx = pipeline.take_outbound_rx().unwrap();

        handle.start(1, 1);
        // Pull a few frames and then stop.
        for _ in 0..3 {
            let _ = tokio::time::timeout(Duration::from_secs(2), rx.recv())
                .await
                .expect("timed out")
                .expect("frame");
        }
        handle.stop();
        // Drain any in-flight enqueued frames within a window.
        let _ = tokio::time::timeout(Duration::from_millis(100), async {
            while rx.recv().await.is_some() {}
        })
        .await;

        // After stop, no more frames within 200 ms.
        let nothing = tokio::time::timeout(Duration::from_millis(200), rx.recv()).await;
        assert!(nothing.is_err(), "expected no frames after stop");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn restart_records_new_session_id() {
        let pipeline = VideoPipeline::new(2);
        let handle = pipeline.handle();
        let _rx = pipeline.take_outbound_rx().unwrap();
        handle.start(1, 1);
        handle.stop();
        handle.start(2, 1);
        assert_eq!(handle.session_id(), 2);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn media_ack_releases_in_flight_slot() {
        let pipeline = VideoPipeline::new(64);
        let handle = pipeline.handle();
        // Manually bump in_flight then ack — should clamp at 0 without underflow.
        handle.in_flight.store(3, Ordering::SeqCst);
        handle.on_media_ack();
        assert_eq!(handle.in_flight.load(Ordering::SeqCst), 2);
        handle.on_media_ack();
        handle.on_media_ack();
        handle.on_media_ack(); // 4th: should stay at 0
        assert_eq!(handle.in_flight.load(Ordering::SeqCst), 0);
    }
}
