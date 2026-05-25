//! `NavInstructionSource` — the abstract producer of turn-by-turn instructions.
//!
//! Anything that can take "(current GPS fix, destination)" and emit a stream of
//! [`crate::channels::nav::NavInstruction`]s implements this trait. Concrete
//! implementations:
//!
//!   - [`crate::nav::osrm::OsrmNavSource`] — queries an OSRM HTTP server,
//!     advances the step pointer as the user follows the route, and triggers
//!     a recalc on big off-route deviations.
//!   - [`StaticDemoSource`] — emits a hand-built "Continue straight" record
//!     on every position update. Used by unit tests and as a no-network
//!     fallback inside the daemon when the user hasn't set a destination yet.
//!
//! Why a trait at all? Two reasons. First, **tests**: we want to drive the
//! control-channel loop with a deterministic fake source so the integration
//! test in `tests/control_channel_loop.rs` doesn't depend on a public OSRM
//! server. Second, **future routers**: we already know we'll want an offline
//! Valhalla/MapMatch implementation eventually, and it's easier to slot a new
//! one in if the public seam is the trait, not a concrete struct.
//!
//! The trait uses the `async-trait` crate macro (already in the workspace for
//! the audio sink traits). That preserves `dyn NavInstructionSource` dispatch,
//! which the runner relies on so the daemon can pick between OSRM /
//! static-demo / a future offline router at runtime.

use crate::channels::nav::NavInstruction;
use anyhow::Result;
use async_trait::async_trait;

/// A GPS fix expressed in WGS84 + clockwise-from-north bearing in degrees. All
/// units are SI (degrees, m/s, metres). The sensor channel converts the AA
/// E7/E6/mm fields into this shape before forwarding here.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PositionFix {
    /// Wall-clock timestamp of the fix in milliseconds since the unix epoch
    /// (as the car reports it). Used only for ordering / debouncing.
    pub timestamp_ms: i64,
    pub lat: f64,
    pub lon: f64,
    /// Course over ground in degrees, clockwise from true north. The car's
    /// GPSLocation.bearing field after E6 conversion.
    pub bearing_deg: f64,
    /// Speed over ground in m/s. Optional — defaults to 0 if the car doesn't
    /// report it.
    pub speed_mps: f64,
    /// Reported horizontal accuracy in metres. A source may choose to ignore
    /// fixes worse than some threshold; the demo source doesn't.
    pub accuracy_m: f64,
}

impl PositionFix {
    /// Crude great-circle distance via the haversine formula. Output in metres.
    /// Adequate for sub-1km step boundaries and route-deviation checks.
    pub fn distance_to(&self, other_lat: f64, other_lon: f64) -> f64 {
        haversine_m(self.lat, self.lon, other_lat, other_lon)
    }
}

/// Free-function form of the haversine distance — exported so the OSRM
/// implementation can use it on `(f64, f64)` coordinates from the response
/// without first constructing a `PositionFix`.
pub fn haversine_m(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let r = 6_371_000.0_f64; // mean Earth radius, metres
    let to_rad = std::f64::consts::PI / 180.0;
    let (lat1r, lat2r) = (lat1 * to_rad, lat2 * to_rad);
    let dlat = (lat2 - lat1) * to_rad;
    let dlon = (lon2 - lon1) * to_rad;
    let a = (dlat / 2.0).sin().powi(2)
        + lat1r.cos() * lat2r.cos() * (dlon / 2.0).sin().powi(2);
    2.0 * r * a.sqrt().asin()
}

/// A user-visible destination. `label` is for log lines + future UI; the
/// engine only routes on `(lat, lon)`.
#[derive(Debug, Clone, PartialEq)]
pub struct Destination {
    pub lat: f64,
    pub lon: f64,
    pub label: String,
}

impl Destination {
    pub fn new(lat: f64, lon: f64, label: impl Into<String>) -> Self {
        Self { lat, lon, label: label.into() }
    }
}

/// The core trait. Implementations are `Send`. The runner drives them on a
/// single tokio task and holds the source by `&mut`, so concrete impls do
/// not need to be `Sync`.
#[async_trait]
pub trait NavInstructionSource: Send {
    /// Install a new destination. The implementation is free to issue any I/O
    /// it needs (route fetch, MapMatch query, etc.) before returning. The
    /// runner only calls this once per destination, so it's OK if this is
    /// slow (~100s of ms for an online OSRM request).
    ///
    /// On success, returns the immediate first instruction the source wants
    /// the car to display (typically the head of the freshly-fetched route).
    /// On failure (network down, OSRM rejected the request, etc.) the source
    /// must not panic; the runner will surface the error to logs and keep
    /// the existing instruction (or fall back to demo mode).
    async fn set_destination(
        &mut self,
        dest: Destination,
    ) -> Result<Option<NavInstruction>>;

    /// Push a GPS fix at the source. The source decides whether the fix means
    /// "still on the same step" (returns `None`), "advance to next step"
    /// (returns the new step instruction), or "route deviation big enough to
    /// recalc" (also returns the new instruction after recalc).
    async fn update_position(
        &mut self,
        fix: PositionFix,
    ) -> Result<Option<NavInstruction>>;

    /// Returns the source's current best instruction without consuming any
    /// input. Used by the runner on startup, before any GPS fix has arrived,
    /// so the car sees an instruction within a tick of the channel opening.
    fn current_instruction(&self) -> Option<NavInstruction>;
}

/// Convenience methods on top of the trait. Kept in a separate trait so
/// custom impls don't have to override these.
pub trait NavInstructionSourceExt {
    /// Drains the source synchronously: useful when a test wants to know what
    /// the source *thinks* its next instruction should be without driving the
    /// async runner. Default impl just delegates to `current_instruction`.
    fn snapshot(&self) -> Option<NavInstruction>;
}
impl<T: NavInstructionSource + ?Sized> NavInstructionSourceExt for T {
    fn snapshot(&self) -> Option<NavInstruction> {
        self.current_instruction()
    }
}

/// "Continue straight" forever — same shape as `NavInstruction::straight_ahead`
/// but pushed through the source pipeline so the integration test exercises
/// the full path (sensor -> source -> nav channel) even without a network.
/// Production callers should prefer [`crate::nav::osrm::OsrmNavSource`].
#[derive(Default)]
pub struct StaticDemoSource {
    last_dest: Option<Destination>,
    update_count: u64,
}

impl StaticDemoSource {
    /// Public peek for tests — how many GPS fixes have we eaten?
    pub fn updates(&self) -> u64 {
        self.update_count
    }
    /// Public peek for tests — what destination is installed?
    pub fn destination(&self) -> Option<&Destination> {
        self.last_dest.as_ref()
    }
}

#[async_trait]
impl NavInstructionSource for StaticDemoSource {
    async fn set_destination(
        &mut self,
        dest: Destination,
    ) -> Result<Option<NavInstruction>> {
        tracing::info!(
            label = %dest.label,
            lat = dest.lat,
            lon = dest.lon,
            "nav: static-demo source installed destination"
        );
        self.last_dest = Some(dest);
        Ok(Some(NavInstruction::straight_ahead()))
    }

    async fn update_position(
        &mut self,
        _fix: PositionFix,
    ) -> Result<Option<NavInstruction>> {
        self.update_count += 1;
        Ok(Some(NavInstruction::straight_ahead()))
    }

    fn current_instruction(&self) -> Option<NavInstruction> {
        Some(NavInstruction::straight_ahead())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fix(lat: f64, lon: f64) -> PositionFix {
        PositionFix {
            timestamp_ms: 1_747_440_000_000,
            lat,
            lon,
            bearing_deg: 0.0,
            speed_mps: 10.0,
            accuracy_m: 5.0,
        }
    }

    #[test]
    fn haversine_dc_to_baltimore_is_about_55km() {
        // Washington DC -> Baltimore Inner Harbor — straight-line ~55 km.
        let d = haversine_m(38.8951, -77.0364, 39.2854, -76.6105);
        assert!(
            (50_000.0..60_000.0).contains(&d),
            "expected ~55 km, got {} m",
            d
        );
    }

    #[test]
    fn position_distance_helper_matches_free_fn() {
        let p = fix(40.0, -75.0);
        let d_method = p.distance_to(41.0, -75.0);
        let d_free = haversine_m(40.0, -75.0, 41.0, -75.0);
        assert!((d_method - d_free).abs() < 1.0);
    }

    #[tokio::test]
    async fn static_demo_emits_on_every_call() {
        let mut s = StaticDemoSource::default();
        let dest = Destination::new(38.0, -77.0, "Test");
        let first = s.set_destination(dest.clone()).await.unwrap();
        assert!(first.is_some());
        assert_eq!(s.destination(), Some(&dest));

        for _ in 0..3 {
            let upd = s.update_position(fix(38.001, -77.001)).await.unwrap();
            assert!(upd.is_some());
        }
        assert_eq!(s.updates(), 3);
        assert!(s.current_instruction().is_some());
    }
}
