//! OSRM-backed navigation source.
//!
//! Pulls a turn-by-turn route from an OSRM HTTP server (default: the public
//! demo at https://router.project-osrm.org) and walks the user along its
//! `steps[]` array as GPS fixes arrive. The OSRM "v1 driving" response shape
//! is stable across the 5.x releases that the public demo + most self-hosted
//! deployments run.
//!
//! Request:
//!
//!   GET /route/v1/driving/{lon_a},{lat_a};{lon_b},{lat_b}
//!       ?steps=true
//!       &geometries=geojson
//!       &overview=full
//!
//! Response (trimmed):
//!
//!   {
//!     "code": "Ok",
//!     "routes": [{
//!       "legs": [{
//!         "steps": [{
//!           "distance": 132.7,
//!           "name": "Main St",
//!           "maneuver": {
//!             "type": "turn",
//!             "modifier": "left",
//!             "location": [lon, lat]
//!           }
//!         }, ...]
//!       }]
//!     }]
//!   }
//!
//! Translation strategy:
//!
//!   - Each step has a maneuver describing what the driver should *do at the
//!     end of the step* (i.e. at maneuver.location). So the nav instruction
//!     associated with step `i` is "in N metres, do step[i+1].maneuver". The
//!     `distance_m` is the haversine distance from the current GPS fix to
//!     step[i+1].maneuver.location.
//!   - We advance to step i+1 once the user is within `ADVANCE_RADIUS_M` of
//!     its maneuver point.
//!   - If the user is more than `RECALC_RADIUS_M` away from *any* point on
//!     the current step, we trigger a recalc — fetch a fresh route from the
//!     current GPS fix to the destination and replace the route. This is a
//!     cheap heuristic (real routers project onto the route geometry); good
//!     enough until we have offline routing on the CM5.

use super::source::{Destination, NavInstructionSource, PositionFix};
// haversine_m is currently only used transitively via PositionFix::distance_to;
// keep the path commented as a hint for future use (Valhalla port etc.).
#[allow(unused_imports)]
use super::source::haversine_m;
use crate::channels::nav::NavInstruction;
use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use serde::Deserialize;
use std::time::Duration;

/// Distance below which we say "user reached the next maneuver" and advance
/// the step pointer. 25 m matches OSRM's own `arrive` distance in its DEMO UI.
const ADVANCE_RADIUS_M: f64 = 25.0;

/// Distance above which we say "user is off-route" and trigger a recalc. The
/// public OSRM demo can recompute a 50-km route in ~250ms; we don't want to
/// hammer it but also can't leave a wrong instruction up for long, so 75 m is
/// a reasonable balance.
const RECALC_RADIUS_M: f64 = 75.0;

/// Default OSRM endpoint — the public demo server run by the OSRM team. Fine
/// for a few requests per minute (their AUP). Self-hosted deployments
/// override via [`OsrmClient::with_base_url`].
pub const DEFAULT_OSRM_BASE_URL: &str = "https://router.project-osrm.org";

/// HTTP request timeout for a single route fetch. The public demo is usually
/// <500 ms; we give it 5 s to handle a cellular dropout without the daemon
/// stalling.
const HTTP_TIMEOUT: Duration = Duration::from_secs(5);

// ---------------------------------------------------------------------------
// OSRM response model. We only deserialise the fields we actually use.
// Anything else is dropped silently.
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct OsrmResponse {
    code: String,
    #[serde(default)]
    routes: Vec<OsrmRoute>,
    #[serde(default)]
    message: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OsrmRoute {
    #[serde(default)]
    legs: Vec<OsrmLeg>,
    // `distance` / `duration` come back from OSRM too; we don't *act* on them
    // yet (the legs[].steps[].distance is what the source state machine
    // needs), so they're dropped on parse. Left as a documentation comment
    // because future "ETA on the head unit" work will pull them.
}

#[derive(Debug, Deserialize)]
struct OsrmLeg {
    #[serde(default)]
    steps: Vec<OsrmStep>,
}

#[derive(Debug, Deserialize)]
struct OsrmStep {
    #[serde(default)]
    distance: f64,
    #[serde(default)]
    name: String,
    maneuver: OsrmManeuver,
}

#[derive(Debug, Deserialize)]
struct OsrmManeuver {
    /// [lon, lat] per GeoJSON convention.
    location: [f64; 2],
    /// e.g. "turn", "depart", "arrive", "fork", "continue", "merge", "roundabout"
    #[serde(rename = "type")]
    #[serde(default)]
    maneuver_type: String,
    /// e.g. "left", "right", "slight left", "uturn", "straight"
    #[serde(default)]
    modifier: Option<String>,
}

// ---------------------------------------------------------------------------
// HTTP client wrapper. Trivial — but factored out so tests can swap the base
// URL to a wiremock instance without standing up real DNS.
// ---------------------------------------------------------------------------

/// Thin OSRM HTTP client. Holds a single shared [`reqwest::Client`] (which
/// internally pools connections) and the base URL. Construct via
/// `OsrmClient::new()` for the public demo, or `with_base_url` for self-hosted.
#[derive(Clone)]
pub struct OsrmClient {
    http: reqwest::Client,
    base_url: String,
}

impl Default for OsrmClient {
    fn default() -> Self {
        Self::new()
    }
}

impl OsrmClient {
    pub fn new() -> Self {
        let http = reqwest::Client::builder()
            .timeout(HTTP_TIMEOUT)
            .user_agent(concat!(
                "aabox-aapd/",
                env!("CARGO_PKG_VERSION"),
                " (+https://github.com/cknickl/aabox)"
            ))
            .build()
            .expect("build reqwest client");
        Self {
            http,
            base_url: DEFAULT_OSRM_BASE_URL.to_string(),
        }
    }

    pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = url.into();
        self
    }

    /// Fetch a driving route from `(start_lat, start_lon)` to `(end_lat,
    /// end_lon)`. Returns the first route in the response, or an error if
    /// OSRM said `code != "Ok"` / returned no routes / the HTTP call failed.
    pub async fn route(
        &self,
        start_lat: f64,
        start_lon: f64,
        end_lat: f64,
        end_lon: f64,
    ) -> Result<Route> {
        // OSRM coordinate order is lon,lat (GeoJSON convention) — easy to get
        // wrong, easy to test for, so keep it isolated here.
        let url = format!(
            "{}/route/v1/driving/{:.6},{:.6};{:.6},{:.6}?steps=true&geometries=geojson&overview=full",
            self.base_url.trim_end_matches('/'),
            start_lon,
            start_lat,
            end_lon,
            end_lat,
        );
        tracing::debug!(%url, "osrm: fetching route");
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .with_context(|| format!("osrm: GET {url} failed"))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(anyhow!("osrm: HTTP {} from {}", status, url));
        }
        let body: OsrmResponse = resp
            .json()
            .await
            .context("osrm: parse JSON response")?;
        if body.code != "Ok" {
            return Err(anyhow!(
                "osrm: code={}{}",
                body.code,
                body.message
                    .as_ref()
                    .map(|m| format!(" ({})", m))
                    .unwrap_or_default()
            ));
        }
        let route = body.routes.pop_first()
            .ok_or_else(|| anyhow!("osrm: empty routes[]"))?;
        Ok(Route::from_osrm(route))
    }
}

// `Vec::pop_first()` doesn't exist; we want the first route, not last. Use
// a manual helper for clarity.
trait PopFirst<T> {
    fn pop_first(self) -> Option<T>;
}
impl<T> PopFirst<T> for Vec<T> {
    fn pop_first(mut self) -> Option<T> {
        if self.is_empty() { None } else { Some(self.swap_remove(0)) }
    }
}

// ---------------------------------------------------------------------------
// Internal route representation. Decoupled from the OSRM response model so we
// can later add a Valhalla / offline implementation that fills the same
// struct without touching the source state machine.
// ---------------------------------------------------------------------------

/// One step of a route, abstracted away from the OSRM response.
#[derive(Debug, Clone)]
pub struct RouteStep {
    /// Lat of the maneuver point (end of this step).
    pub lat: f64,
    pub lon: f64,
    /// Distance of this step in metres (OSRM's leg.distance).
    pub distance_m: f64,
    /// Road name OSRM associated with the step (may be empty for unnamed roads).
    pub name: String,
    /// AA turn-event ID (1..=8 as enumerated in `channels::nav::NavInstruction::turn_id`).
    pub turn_id: u32,
    /// Human-readable verb derived from (type, modifier). Pre-rendered for
    /// the status text — the source merges this with `name` and remaining
    /// metres to build the final string.
    pub verb: String,
}

/// A full route — a sequence of [`RouteStep`]s. Step 0 is `depart`; the last
/// step is `arrive` (zero-distance, marks the destination).
#[derive(Debug, Clone)]
pub struct Route {
    pub steps: Vec<RouteStep>,
}

impl Route {
    fn from_osrm(r: OsrmRoute) -> Self {
        let mut steps = Vec::new();
        for leg in r.legs {
            for s in leg.steps {
                steps.push(RouteStep {
                    lat: s.maneuver.location[1],
                    lon: s.maneuver.location[0],
                    distance_m: s.distance,
                    name: s.name,
                    turn_id: maneuver_to_turn_id(
                        &s.maneuver.maneuver_type,
                        s.maneuver.modifier.as_deref(),
                    ),
                    verb: maneuver_to_verb(
                        &s.maneuver.maneuver_type,
                        s.maneuver.modifier.as_deref(),
                    ),
                });
            }
        }
        Self { steps }
    }
}

/// Map the OSRM `(type, modifier)` pair to an AA turn-event ID.
///
/// AA encodes the ID range as (from openauto / headunit-go evidence):
///   1 = straight / depart / continue
///   2 = slight left
///   3 = left
///   4 = sharp left
///   5 = u-turn
///   6 = sharp right
///   7 = right
///   8 = slight right
/// We extend with:
///   9 = arrive (we re-use depart's icon for now — the car shows a flag for 9)
fn maneuver_to_turn_id(maneuver_type: &str, modifier: Option<&str>) -> u32 {
    match maneuver_type {
        "depart" | "continue" | "new name" => 1,
        "arrive" => 9,
        // u-turn shows up either as type=turn modifier=uturn or its own type.
        "rotary" | "roundabout" => 3, // approximate as "left" — most cars show a custom roundabout icon when paired with our nav image
        _ => match modifier {
            Some("uturn") => 5,
            Some("sharp left") => 4,
            Some("left") => 3,
            Some("slight left") => 2,
            Some("slight right") => 8,
            Some("right") => 7,
            Some("sharp right") => 6,
            Some("straight") | None => 1,
            Some(_) => 1, // unknown modifier — fall back to straight icon
        },
    }
}

/// Map the OSRM `(type, modifier)` pair to a human-readable verb. Used as the
/// status-text prefix ("Turn left", "Slight right", etc.).
fn maneuver_to_verb(maneuver_type: &str, modifier: Option<&str>) -> String {
    match maneuver_type {
        "depart" => "Head".to_string(),
        "arrive" => "You have arrived".to_string(),
        "continue" | "new name" => "Continue".to_string(),
        "fork" => match modifier {
            Some("left") | Some("slight left") => "Keep left".to_string(),
            _ => "Keep right".to_string(),
        },
        "roundabout" | "rotary" => "Take the roundabout".to_string(),
        "merge" => "Merge".to_string(),
        "on ramp" => "Take the on-ramp".to_string(),
        "off ramp" => "Take the exit".to_string(),
        _ => match modifier {
            Some("uturn") => "Make a U-turn".to_string(),
            Some("sharp left") => "Turn sharp left".to_string(),
            Some("left") => "Turn left".to_string(),
            Some("slight left") => "Bear left".to_string(),
            Some("slight right") => "Bear right".to_string(),
            Some("right") => "Turn right".to_string(),
            Some("sharp right") => "Turn sharp right".to_string(),
            Some("straight") | None => "Continue".to_string(),
            Some(other) => format!("Turn ({})", other),
        },
    }
}

// ---------------------------------------------------------------------------
// The OSRM-backed source itself: holds an `OsrmClient`, the current route,
// the current step pointer, and the user's last destination so a recalc can
// re-request the same endpoint.
// ---------------------------------------------------------------------------

/// OSRM-backed navigation source. Holds the active route and an index into
/// `route.steps`. Each `update_position` advances the index if the user has
/// reached the next maneuver point, or triggers a recalc if they're off-route.
pub struct OsrmNavSource {
    client: OsrmClient,
    destination: Option<Destination>,
    route: Option<Route>,
    /// Index of the *next* maneuver — i.e. the step we are en-route *toward*.
    /// Always 0..route.steps.len(). When this hits `steps.len() - 1` we're
    /// approaching the arrive step.
    next_idx: usize,
    last_fix: Option<PositionFix>,
    /// Cached current instruction (so `current_instruction` is cheap and
    /// doesn't need to recompute the verb on every poll).
    current: Option<NavInstruction>,
}

impl OsrmNavSource {
    pub fn new(client: OsrmClient) -> Self {
        Self {
            client,
            destination: None,
            route: None,
            next_idx: 0,
            last_fix: None,
            current: None,
        }
    }

    /// Convenience: construct an OSRM source pointing at the public demo.
    pub fn public() -> Self {
        Self::new(OsrmClient::new())
    }

    /// Internal — rebuild the cached current instruction from the active route
    /// and the user's last known position. If we don't have a position yet
    /// the distance is `step.distance_m` (OSRM's own per-step distance, which
    /// is a perfectly fine first-display value).
    fn rebuild_current(&mut self) {
        let Some(route) = self.route.as_ref() else {
            self.current = None;
            return;
        };
        if route.steps.is_empty() {
            self.current = None;
            return;
        }
        // The instruction *is* the maneuver at `next_idx`. If next_idx is past
        // the last step we've arrived.
        let idx = self.next_idx.min(route.steps.len() - 1);
        let step = &route.steps[idx];

        // Distance: prefer the live haversine to the maneuver point if we
        // have a fix; else fall back to OSRM's per-step distance (which is
        // the length of the *previous* step, not exact but close enough for
        // a first impression).
        let dist_m = if let Some(p) = self.last_fix {
            p.distance_to(step.lat, step.lon)
        } else if idx > 0 {
            // OSRM's step.distance is the length of the segment that *led to*
            // this maneuver. Using the previous step's distance is therefore
            // a reasonable upper bound on "metres until you do this".
            route.steps[idx - 1].distance_m
        } else {
            // First step. The depart maneuver has distance 0; show the
            // immediate next maneuver's segment instead.
            route.steps.get(1).map(|s| s.distance_m).unwrap_or(0.0)
        };

        let status_text = if step.name.is_empty() {
            format!("{} in {} m", step.verb, dist_m.round() as i64)
        } else {
            format!("{} in {} m onto {}", step.verb, dist_m.round() as i64, step.name)
        };

        self.current = Some(NavInstruction {
            status_text,
            turn_id: step.turn_id,
            distance_m: dist_m.round().max(0.0) as u32,
        });
    }

    /// Compute the user's haversine distance to the nearest *step maneuver
    /// point* on the current route. Used as a (cheap) off-route heuristic —
    /// a real implementation projects onto the geometry, but the geometry's
    /// `overview` polyline is enough for that follow-up.
    fn min_dist_to_route(&self, fix: PositionFix) -> Option<f64> {
        let route = self.route.as_ref()?;
        route
            .steps
            .iter()
            .map(|s| fix.distance_to(s.lat, s.lon))
            .fold(None, |acc, d| match acc {
                None => Some(d),
                Some(a) if d < a => Some(d),
                Some(a) => Some(a),
            })
    }

    /// Test-only accessor for the active route.
    #[cfg(test)]
    pub(crate) fn route(&self) -> Option<&Route> {
        self.route.as_ref()
    }

    /// Test-only accessor for the step pointer.
    #[cfg(test)]
    pub(crate) fn next_idx(&self) -> usize {
        self.next_idx
    }
}

#[async_trait]
impl NavInstructionSource for OsrmNavSource {
    async fn set_destination(
        &mut self,
        dest: Destination,
    ) -> Result<Option<NavInstruction>> {
        tracing::info!(
            label = %dest.label,
            lat = dest.lat,
            lon = dest.lon,
            "nav: OSRM source installing destination"
        );
        // Without a start position we can't fetch a route; remember the
        // destination so the first GPS fix triggers the initial route
        // request. This is the common case: the daemon learns the
        // destination from a CLI flag at boot, then the car starts
        // streaming GPS a few seconds later.
        self.destination = Some(dest);
        self.route = None;
        self.next_idx = 0;
        self.current = None;

        // Maybe-immediate fetch if we already have a fix.
        if let Some(p) = self.last_fix {
            let d = self.destination.clone().unwrap();
            match self.client.route(p.lat, p.lon, d.lat, d.lon).await {
                Ok(r) => {
                    self.route = Some(r);
                    // depart=step0, first turn=step1. We want the user to see
                    // the first turn, so next_idx=1 unless there's no step 1.
                    if let Some(route) = self.route.as_ref() {
                        self.next_idx = if route.steps.len() >= 2 { 1 } else { 0 };
                    }
                    self.rebuild_current();
                    Ok(self.current.clone())
                }
                Err(e) => {
                    tracing::warn!("osrm: initial route fetch failed: {e:#}");
                    Err(e)
                }
            }
        } else {
            // No fix yet — return nothing, the runner will keep showing
            // whatever was current before (or fall back to demo).
            Ok(None)
        }
    }

    async fn update_position(
        &mut self,
        fix: PositionFix,
    ) -> Result<Option<NavInstruction>> {
        self.last_fix = Some(fix);

        // No destination? Nothing to do.
        let Some(dest) = self.destination.clone() else {
            return Ok(None);
        };

        // No route yet? Bootstrap path — first fix after destination was set.
        if self.route.is_none() {
            match self.client.route(fix.lat, fix.lon, dest.lat, dest.lon).await {
                Ok(r) => {
                    self.route = Some(r);
                    let len = self.route.as_ref().map(|r| r.steps.len()).unwrap_or(0);
                    self.next_idx = if len >= 2 { 1 } else { 0 };
                    self.rebuild_current();
                    return Ok(self.current.clone());
                }
                Err(e) => {
                    tracing::warn!("osrm: bootstrap route fetch failed: {e:#}");
                    return Err(e);
                }
            }
        }

        // Advance the step pointer if we've reached the next maneuver.
        let mut emitted = false;
        if let Some(route) = self.route.as_ref() {
            if self.next_idx < route.steps.len() {
                let step = &route.steps[self.next_idx];
                let d_to_man = fix.distance_to(step.lat, step.lon);
                if d_to_man < ADVANCE_RADIUS_M && self.next_idx + 1 < route.steps.len() {
                    self.next_idx += 1;
                    emitted = true;
                }
            }
        }

        // Off-route check: are we close to *any* step's maneuver point? If
        // the min distance is huge we should recalc. (Real impl would project
        // onto the polyline geometry; this is the cheap proxy.)
        let needs_recalc = self
            .min_dist_to_route(fix)
            .map(|d| d > RECALC_RADIUS_M)
            .unwrap_or(false);
        if needs_recalc {
            tracing::info!(
                deviation_m = self.min_dist_to_route(fix).unwrap_or(0.0),
                "osrm: route deviation, recalculating"
            );
            match self.client.route(fix.lat, fix.lon, dest.lat, dest.lon).await {
                Ok(r) => {
                    self.route = Some(r);
                    let len = self.route.as_ref().map(|r| r.steps.len()).unwrap_or(0);
                    self.next_idx = if len >= 2 { 1 } else { 0 };
                    emitted = true;
                }
                Err(e) => {
                    tracing::warn!("osrm: recalc failed: {e:#}");
                    // Keep the existing route and don't emit a new instruction.
                }
            }
        }

        // Either an advance/recalc happened (verb changed) or it's the same
        // step with a fresher distance. Either way we rebuild the cached
        // instruction so the car's countdown ticks down.
        self.rebuild_current();
        let _ = emitted; // (advance state already reflected in `current`).
        Ok(self.current.clone())
    }

    fn current_instruction(&self) -> Option<NavInstruction> {
        self.current.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};

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

    // A canned OSRM "Ok" response with three steps:
    //   step 0: depart at (38.000, -77.000)
    //   step 1: turn left at (38.001, -77.000) onto "Main St", 100 m away
    //   step 2: arrive at (38.001, -76.999), 50 m away
    fn canned_response() -> &'static str {
        r#"{
          "code": "Ok",
          "routes": [{
            "distance": 150.0,
            "duration": 30.0,
            "legs": [{
              "steps": [
                {
                  "distance": 100.0,
                  "name": "",
                  "maneuver": {
                    "type": "depart",
                    "location": [-77.000, 38.000]
                  }
                },
                {
                  "distance": 50.0,
                  "name": "Main St",
                  "maneuver": {
                    "type": "turn",
                    "modifier": "left",
                    "location": [-77.000, 38.001]
                  }
                },
                {
                  "distance": 0.0,
                  "name": "Main St",
                  "maneuver": {
                    "type": "arrive",
                    "location": [-76.999, 38.001]
                  }
                }
              ]
            }]
          }]
        }"#
    }

    #[test]
    fn maneuver_to_turn_id_table() {
        assert_eq!(maneuver_to_turn_id("turn", Some("left")), 3);
        assert_eq!(maneuver_to_turn_id("turn", Some("right")), 7);
        assert_eq!(maneuver_to_turn_id("turn", Some("uturn")), 5);
        assert_eq!(maneuver_to_turn_id("depart", None), 1);
        assert_eq!(maneuver_to_turn_id("arrive", None), 9);
        assert_eq!(maneuver_to_turn_id("turn", Some("sharp right")), 6);
        assert_eq!(maneuver_to_turn_id("turn", Some("slight left")), 2);
    }

    #[test]
    fn maneuver_verb_includes_modifier() {
        let v = maneuver_to_verb("turn", Some("left"));
        assert!(v.contains("left"), "got: {v}");
        let v = maneuver_to_verb("arrive", None);
        assert!(v.to_lowercase().contains("arrived"), "got: {v}");
    }

    #[tokio::test]
    async fn osrm_client_parses_canned_response() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_regex(r"^/route/v1/driving/.*"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/json")
                    .set_body_string(canned_response()),
            )
            .mount(&server)
            .await;

        let client = OsrmClient::new().with_base_url(server.uri());
        let route = client.route(38.000, -77.000, 38.001, -76.999).await.unwrap();

        assert_eq!(route.steps.len(), 3);
        assert_eq!(route.steps[0].turn_id, 1); // depart
        assert_eq!(route.steps[1].turn_id, 3); // turn left
        assert_eq!(route.steps[1].name, "Main St");
        assert_eq!(route.steps[2].turn_id, 9); // arrive
    }

    #[tokio::test]
    async fn osrm_client_propagates_non_ok_code() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_regex(r"^/route/v1/driving/.*"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"code": "NoRoute", "message": "Impossible route"}"#,
            ))
            .mount(&server)
            .await;

        let client = OsrmClient::new().with_base_url(server.uri());
        let err = client
            .route(0.0, 0.0, 0.0, 0.0)
            .await
            .expect_err("expected NoRoute");
        let s = format!("{err:#}");
        assert!(s.contains("NoRoute"), "got: {s}");
    }

    #[tokio::test]
    async fn osrm_source_advances_step_on_arrival() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_regex(r"^/route/v1/driving/.*"))
            .respond_with(ResponseTemplate::new(200).set_body_string(canned_response()))
            .mount(&server)
            .await;

        let mut src =
            OsrmNavSource::new(OsrmClient::new().with_base_url(server.uri()));

        // Set destination. No fix yet, so this returns Ok(None) and *doesn't*
        // hit the network — there's nothing to start from.
        let r = src
            .set_destination(Destination::new(38.001, -76.999, "Test"))
            .await
            .unwrap();
        assert!(r.is_none());
        assert!(src.route().is_none());

        // First fix near step 0 (depart). Source should fetch the route now,
        // set next_idx=1 (the turn), and emit a "Turn left in N m onto Main
        // St" instruction.
        let instr = src
            .update_position(fix(38.000, -77.000))
            .await
            .unwrap()
            .expect("first instruction after bootstrap");
        assert_eq!(instr.turn_id, 3);
        assert!(
            instr.status_text.contains("Main St"),
            "got: {}",
            instr.status_text
        );
        assert_eq!(src.next_idx(), 1);

        // Now move within ADVANCE_RADIUS_M of step 1 (the turn). Source
        // should advance to step 2 (arrive).
        let instr = src
            .update_position(fix(38.001, -77.000))
            .await
            .unwrap()
            .expect("instruction after advance");
        assert_eq!(instr.turn_id, 9, "expected arrive icon");
        assert_eq!(src.next_idx(), 2);
    }
}
