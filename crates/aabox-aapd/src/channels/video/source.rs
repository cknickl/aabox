//! `VideoSource` — protocol-agnostic interface for "something that produces
//! a stream of H.264 access units".
//!
//! The control loop drives one of these on every START indication from the
//! car. The trait is deliberately minimal: callers don't care whether the
//! frames come from a hardware encoder, an x264 process, the Android screen
//! recorder, or — for Phase 1 — a precomputed Annex-B elementary stream
//! that loops forever.
//!
//! Each call to [`VideoSource::next_frame`] returns one *access unit*
//! (everything for one displayed picture, including any leading SPS / PPS).
//! It blocks until the next presentation time so that callers don't have
//! to do their own pacing; the source owns the clock.

use super::nal;
use async_trait::async_trait;
use std::time::{Duration, Instant};

/// One H.264 access unit headed for the wire.
#[derive(Debug, Clone)]
pub struct VideoFrame {
    /// Annex-B bytes — `[start_code][NAL]...`. Includes parameter sets when
    /// they should accompany the frame (typically the very first frame and
    /// every IDR).
    pub annex_b: Vec<u8>,
    /// PTS in microseconds since stream start, monotonic and non-decreasing.
    pub pts_us: u64,
    /// True if this access unit contains an IDR slice (keyframe).
    pub keyframe: bool,
}

#[async_trait]
pub trait VideoSource: Send + 'static {
    /// Return the next access unit, or `None` if the source has permanently
    /// closed. May await internally to pace itself.
    async fn next_frame(&mut self) -> Option<VideoFrame>;

    /// Cooperative cancellation. Source implementations should observe this
    /// at every `next_frame` entry; the pipeline calls it on STOP.
    fn close(&mut self) {}
}

/// Phase 1 video source: loops a precomputed H.264 elementary stream baked
/// into the binary. Two resolutions are embedded; pick at construction time.
///
/// We always loop from the start, so the SPS / PPS / IDR triplet that
/// begins the asset will be re-emitted at every wrap — which is exactly
/// what we want (the car can rejoin mid-stream).
pub struct TestPatternVideoSource {
    /// Pre-grouped access units. Each entry is one `[SPS][PPS][IDR]...`
    /// or just `[slice]` chunk ready to ship as a single AAP video frame.
    access_units: Vec<Vec<u8>>,
    /// Index into `access_units` of the next AU to emit.
    next_idx: usize,
    /// Source frame interval (e.g. 33.333 ms at 30 fps).
    frame_interval: Duration,
    /// PTS accumulator in microseconds — monotonic across loops.
    pts_us: u64,
    /// Time origin (set on the first `next_frame` call).
    start: Option<Instant>,
}

/// Embedded 720p30 testsrc2 elementary stream (~450 KB, 3 seconds).
pub const EMBEDDED_720P30: &[u8] =
    include_bytes!("../../../assets/test_pattern_720p.h264");

/// Embedded 1080p30 testsrc2 elementary stream (~700 KB, 3 seconds).
pub const EMBEDDED_1080P30: &[u8] =
    include_bytes!("../../../assets/test_pattern_1080p.h264");

/// Which embedded test pattern to play back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TestPatternProfile {
    /// 1280x720 @ 30 fps, baseline level 3.1.
    P720p30,
    /// 1920x1080 @ 30 fps, baseline level 4.0.
    P1080p30,
}

impl TestPatternProfile {
    /// Frame interval = 1 / fps.
    pub fn frame_interval(self) -> Duration {
        match self {
            Self::P720p30 | Self::P1080p30 => Duration::from_micros(1_000_000 / 30),
        }
    }

    /// Bytes of the embedded elementary stream.
    pub fn bytes(self) -> &'static [u8] {
        match self {
            Self::P720p30 => EMBEDDED_720P30,
            Self::P1080p30 => EMBEDDED_1080P30,
        }
    }
}

impl TestPatternVideoSource {
    /// Build a source that replays the embedded pattern in a loop, paced to
    /// the profile's frame rate.
    pub fn new(profile: TestPatternProfile) -> Self {
        let access_units = nal::group_access_units(profile.bytes());
        // Sanity: the embedded asset must have produced at least one AU.
        debug_assert!(
            !access_units.is_empty(),
            "embedded test pattern decoded to zero access units"
        );
        Self {
            access_units,
            next_idx: 0,
            frame_interval: profile.frame_interval(),
            pts_us: 0,
            start: None,
        }
    }

    /// Number of access units in the pre-parsed embedded stream.
    pub fn frame_count(&self) -> usize {
        self.access_units.len()
    }
}

#[async_trait]
impl VideoSource for TestPatternVideoSource {
    async fn next_frame(&mut self) -> Option<VideoFrame> {
        if self.access_units.is_empty() {
            return None;
        }

        // Establish the time origin on first call so the first frame goes
        // out immediately (no startup latency penalty).
        let start = *self.start.get_or_insert_with(Instant::now);
        let target = start + Duration::from_micros(self.pts_us);
        let now = Instant::now();
        if target > now {
            tokio::time::sleep(target - now).await;
        }

        let au = &self.access_units[self.next_idx];
        let keyframe = au_has_idr(au);
        let frame = VideoFrame {
            annex_b: au.clone(),
            pts_us: self.pts_us,
            keyframe,
        };

        // Advance state.
        self.next_idx = (self.next_idx + 1) % self.access_units.len();
        self.pts_us = self.pts_us.saturating_add(self.frame_interval.as_micros() as u64);

        Some(frame)
    }
}

/// Does any NAL in this access unit have nal_unit_type == 5 (IDR)?
fn au_has_idr(au: &[u8]) -> bool {
    nal::split_nals(au)
        .iter()
        .any(|n| nal::nal_unit_type(n) == nal::nut::SLICE_IDR)
}

/// Does any NAL in this access unit have a slice (IDR or non-IDR)? Used by
/// `ScreenRecordVideoSource` to distinguish a "complete" AU (codec params
/// + slice) from a partial trailing AU (codec params only, no slice yet).
fn au_has_slice(au: &[u8]) -> bool {
    nal::split_nals(au)
        .iter()
        .any(|n| nal::is_slice(nal::nal_unit_type(n)))
}

/// Phase 2 video source: spawn Android's built-in `/system/bin/screenrecord`
/// against the primary display and read its H.264 Annex-B output from stdout.
/// This is the real Android UI projecting to the car — same hardware encoder
/// path that `screenrecord` uses on every Pixel, with correct profile/level
/// negotiation against the SoC's MediaCodec implementation (which on RK3588
/// resolves to `c2.rk.video.encoder.avc` → Rockchip MPP under the hood).
///
/// We pipe stdout into a 256 KiB buffer, split into access units with the
/// existing `nal::group_access_units`, and emit one AU per `next_frame()`
/// call. The encoder's frame rate paces us — we don't add our own sleep.
///
/// `screenrecord` has a hard `--time-limit` default of 180 seconds; we pass
/// `0` to disable. If the child exits unexpectedly we return `None` from
/// `next_frame` (the pipeline streamer then exits, watchdog restarts the
/// daemon, fresh USB session re-spawns us).
pub struct ScreenRecordVideoSource {
    child: Option<tokio::process::Child>,
    stdout: Option<tokio::process::ChildStdout>,
    /// Bytes read from stdout that haven't yet formed a complete AU.
    leftover: Vec<u8>,
    /// AUs grouped from a single read chunk, waiting to be drained.
    queued: std::collections::VecDeque<Vec<u8>>,
    /// PTS counter — incremented per emitted AU based on frame interval.
    pts_us: u64,
    frame_interval_us: u64,
}

impl ScreenRecordVideoSource {
    /// Spawn `screenrecord` with the given dimensions and bitrate.
    /// `--output-format=h264` makes it emit raw Annex-B; `-` writes to stdout.
    pub fn spawn(width: u32, height: u32, fps: u32, bitrate_bps: u32) -> std::io::Result<Self> {
        use tokio::process::Command;
        let mut child = Command::new("/system/bin/screenrecord")
            .arg("--output-format=h264")
            .arg("--time-limit").arg("0")
            .arg("--size").arg(format!("{}x{}", width, height))
            .arg("--bit-rate").arg(bitrate_bps.to_string())
            .arg("-")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()?;
        let stdout = child.stdout.take();
        Ok(Self {
            child: Some(child),
            stdout,
            leftover: Vec::with_capacity(256 * 1024),
            queued: std::collections::VecDeque::new(),
            pts_us: 0,
            frame_interval_us: 1_000_000 / (fps.max(1) as u64),
        })
    }
}

#[async_trait]
impl VideoSource for ScreenRecordVideoSource {
    async fn next_frame(&mut self) -> Option<VideoFrame> {
        use tokio::io::AsyncReadExt;
        loop {
            // Drain a queued AU first.
            if let Some(au) = self.queued.pop_front() {
                let keyframe = au_has_idr(&au);
                let pts = self.pts_us;
                self.pts_us = self.pts_us.saturating_add(self.frame_interval_us);
                return Some(VideoFrame { annex_b: au, pts_us: pts, keyframe });
            }
            // Read more bytes from screenrecord's stdout.
            let stdout = match self.stdout.as_mut() {
                Some(s) => s,
                None => return None,
            };
            let mut buf = [0u8; 65536];
            let n = match stdout.read(&mut buf).await {
                Ok(0) => {
                    tracing::warn!("screenrecord stdout EOF");
                    return None;
                }
                Ok(n) => n,
                Err(e) => {
                    tracing::warn!(error = %e, "screenrecord stdout read error");
                    return None;
                }
            };
            self.leftover.extend_from_slice(&buf[..n]);
            // Find the LAST start code in leftover — bytes after it are an
            // incomplete AU still in flight from the encoder. Group everything
            // before it into complete AUs.
            if let Some(last_start) = find_last_start_code(&self.leftover) {
                let completed: Vec<u8> = self.leftover.drain(..last_start).collect();
                if !completed.is_empty() {
                    let mut aus = nal::group_access_units(&completed);
                    // `group_access_units` emits trailing non-slice NALs
                    // (SPS/PPS/SEI/AUD) as their own standalone "AU" when
                    // the buffer ends before the next slice. Sending those
                    // to the car would split codec parameters from the
                    // following IDR — KIA's decoder then receives an IDR
                    // with no preceding SPS/PPS and renders black. Push
                    // any non-slice trailing AU back to the FRONT of
                    // `leftover` so it gets re-grouped with the next
                    // slice on the next read.
                    if let Some(last) = aus.last() {
                        if !au_has_slice(last) {
                            let trailing = aus.pop().expect("checked Some above");
                            // Prepend to leftover (the partial AU bytes
                            // we kept earlier go AFTER it; the trailing
                            // non-slice NALs always come BEFORE the next
                            // slice in stream order).
                            let mut combined =
                                Vec::with_capacity(trailing.len() + self.leftover.len());
                            combined.extend_from_slice(&trailing);
                            combined.extend_from_slice(&self.leftover);
                            self.leftover = combined;
                        }
                    }
                    for au in aus {
                        if !au.is_empty() {
                            self.queued.push_back(au);
                        }
                    }
                }
            }
            // If still nothing queued, loop and read more.
        }
    }

    fn close(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.start_kill();
        }
        self.stdout = None;
    }
}

/// Find the byte index of the LAST Annex-B start code (`0x000001` or
/// `0x00000001`) in `buf`. Used by `ScreenRecordVideoSource` to chunk the
/// streaming stdout into complete-AU boundaries.
fn find_last_start_code(buf: &[u8]) -> Option<usize> {
    if buf.len() < 3 { return None; }
    let mut i = buf.len() - 3;
    loop {
        if buf[i] == 0 && buf[i + 1] == 0 && buf[i + 2] == 1 {
            // 4-byte start code prefers the earlier zero if present.
            if i > 0 && buf[i - 1] == 0 {
                return Some(i - 1);
            }
            return Some(i);
        }
        if i == 0 { return None; }
        i -= 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_assets_are_non_empty() {
        assert!(EMBEDDED_720P30.len() > 1024);
        assert!(EMBEDDED_1080P30.len() > 1024);
        // Annex-B starts with a start code.
        assert_eq!(&EMBEDDED_720P30[..4], &[0, 0, 0, 1]);
        assert_eq!(&EMBEDDED_1080P30[..4], &[0, 0, 0, 1]);
    }

    #[test]
    fn test_pattern_first_frame_contains_sps_pps_idr() {
        let src = TestPatternVideoSource::new(TestPatternProfile::P720p30);
        // Frame 0 should be the SPS+PPS+IDR triplet.
        let nals = nal::split_nals(&src.access_units[0]);
        let types: Vec<u8> = nals.iter().map(|n| nal::nal_unit_type(n)).collect();
        assert!(types.contains(&nal::nut::SPS), "expected SPS NAL, got {:?}", types);
        assert!(types.contains(&nal::nut::PPS), "expected PPS NAL, got {:?}", types);
        assert!(
            types.contains(&nal::nut::SLICE_IDR),
            "expected IDR slice, got {:?}",
            types
        );
        let _ = src.frame_count();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_pattern_emits_first_frame_marked_keyframe() {
        let mut src = TestPatternVideoSource::new(TestPatternProfile::P720p30);
        let f = src.next_frame().await.expect("first frame");
        assert!(f.keyframe, "first AU should be an IDR");
        assert_eq!(f.pts_us, 0);
        assert!(f.annex_b.len() > 100);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_pattern_pts_monotonic_and_paced() {
        let mut src = TestPatternVideoSource::new(TestPatternProfile::P720p30);
        let f0 = src.next_frame().await.unwrap();
        let f1 = src.next_frame().await.unwrap();
        assert_eq!(f0.pts_us, 0);
        // Frame interval is 1/30 s = 33_333 us; tolerate exact division.
        assert!(f1.pts_us > f0.pts_us);
        assert!(f1.pts_us <= 35_000);
    }

    #[test]
    fn test_pattern_access_units_contain_at_least_one_idr() {
        // We don't iterate via `next_frame` here — that would block on the
        // real-time pacer for ~one-frame-interval per frame, blowing out
        // test runtimes for a 3-second pattern. The access-units array is
        // pre-parsed at construction time, so inspecting it directly is
        // exactly what we want.
        let src = TestPatternVideoSource::new(TestPatternProfile::P720p30);
        let total = src.frame_count();
        let idrs = (0..total)
            .filter(|i| {
                nal::split_nals(&src.access_units[*i])
                    .iter()
                    .any(|n| nal::nal_unit_type(n) == nal::nut::SLICE_IDR)
            })
            .count();
        assert!(
            idrs >= 1,
            "expected at least one IDR in the parsed pattern, got {} of {}",
            idrs,
            total
        );
    }
}
