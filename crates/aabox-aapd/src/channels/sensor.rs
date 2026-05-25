//! Sensor channel handler.
//!
//! Wire shape (`SensorChannelMessageIdsEnum.proto`):
//!   - 0x8001 SENSOR_START_REQUEST     : car -> us  (`SensorStartRequestMessage`)
//!   - 0x8002 SENSOR_START_RESPONSE    : us -> car  (`SensorStartResponseMessage`)
//!   - 0x8003 SENSOR_EVENT_INDICATION  : car -> us  (`SensorEventIndication`)
//!
//! For Phase 4 we don't *produce* sensor data — there are no real sensors on
//! the CM5. Instead we use this channel to *receive* car telemetry, the most
//! valuable being GPS (`gps_location`). Every SENSOR_EVENT_INDICATION that
//! carries one or more `GPSLocation` records gets appended as NDJSON to
//! `/data/local/tmp/aabox-gps.log`.
//!
//! The AA `GPSLocation` fields are scaled integers:
//!   - latitude/longitude: 1e7 degrees           (E7)
//!   - altitude:           millimetres
//!   - speed:              millimetres / second
//!   - bearing:            1e6 degrees           (E6)
//!   - accuracy:           millimetres
//!
//! We convert to the more useful SI / WGS84 units before logging.

use super::{encode_outbound, OutboundMessage};
use crate::nav::source::PositionFix;
use aabox_common::ChannelId;
use aabox_proto::data::GpsLocation;
use aabox_proto::enums::status;
use aabox_proto::messages::{
    SensorEventIndication, SensorStartRequestMessage, SensorStartResponseMessage,
};
use anyhow::{anyhow, Result};
use prost::Message;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use tokio::sync::mpsc;

pub const MSG_SENSOR_START_REQUEST: u16 = 0x8001;
pub const MSG_SENSOR_START_RESPONSE: u16 = 0x8002;
pub const MSG_SENSOR_EVENT_INDICATION: u16 = 0x8003;

/// Default location for the GPS log. On the CM5 this is the standard userdebug
/// scratch path; the integration test redirects it via [`SensorHandler::with_gps_log_path`].
pub const DEFAULT_GPS_LOG_PATH: &str = "/data/local/tmp/aabox-gps.log";

/// Sensor channel state. Holds the GPS log path; nothing else is needed yet.
/// Wrapped in a Mutex so the (rare) write contention with a periodic synthetic
/// sender doesn't tangle the file. Writes are line-buffered.
///
/// Optionally also forwards every decoded GPS fix to a tokio mpsc sender so a
/// nav engine downstream (the `NavInstructionSource` runner) can react to
/// position updates. The mpsc is opt-in via [`SensorHandler::with_gps_sink`]
/// — when unset, the handler only writes the NDJSON log.
pub struct SensorHandler {
    gps_log_path: PathBuf,
    file: Mutex<Option<std::fs::File>>,
    gps_sink: Option<mpsc::Sender<PositionFix>>,
}

impl Default for SensorHandler {
    fn default() -> Self {
        Self::new(DEFAULT_GPS_LOG_PATH)
    }
}

impl SensorHandler {
    pub fn new<P: AsRef<Path>>(path: P) -> Self {
        Self {
            gps_log_path: path.as_ref().to_path_buf(),
            file: Mutex::new(None),
            gps_sink: None,
        }
    }

    pub fn with_gps_log_path<P: AsRef<Path>>(mut self, path: P) -> Self {
        self.gps_log_path = path.as_ref().to_path_buf();
        self
    }

    /// Install a tokio mpsc sender that will receive every decoded GPS fix.
    /// Backpressure: the sender is `try_send`'d (never await) so a slow nav
    /// engine can't stall the sensor handler. Drops are logged at debug.
    pub fn with_gps_sink(mut self, sink: mpsc::Sender<PositionFix>) -> Self {
        self.gps_sink = Some(sink);
        self
    }

    /// Handle one inbound sensor-channel message. Returns zero or more
    /// outbound replies.
    pub fn handle(&self, msg_id: u16, payload: &[u8]) -> Result<Vec<OutboundMessage>> {
        match msg_id {
            MSG_SENSOR_START_REQUEST => {
                let req = SensorStartRequestMessage::decode(payload)
                    .map_err(|e| anyhow!("decode SensorStartRequest: {e}"))?;
                tracing::info!(
                    sensor_type = req.sensor_type,
                    refresh_interval = req.refresh_interval,
                    "sensor: SENSOR_START_REQUEST -> ACK"
                );
                let resp = SensorStartResponseMessage {
                    status: status::Enum::Ok as i32,
                };
                Ok(vec![encode_outbound(
                    ChannelId::Sensor as u8,
                    MSG_SENSOR_START_RESPONSE,
                    &resp,
                )])
            }
            MSG_SENSOR_EVENT_INDICATION => {
                let ev = SensorEventIndication::decode(payload)
                    .map_err(|e| anyhow!("decode SensorEventIndication: {e}"))?;
                if !ev.gps_location.is_empty() {
                    for loc in &ev.gps_location {
                        self.append_gps(loc);
                    }
                }
                if !ev.driving_status.is_empty() {
                    tracing::info!(
                        n = ev.driving_status.len(),
                        "sensor: driving_status update"
                    );
                }
                if !ev.gear.is_empty() {
                    tracing::info!(n = ev.gear.len(), "sensor: gear update");
                }
                if !ev.speed.is_empty() {
                    tracing::info!(n = ev.speed.len(), "sensor: speed update");
                }
                Ok(vec![])
            }
            other => {
                tracing::warn!(
                    msg_id = format!("0x{:04x}", other),
                    "sensor: unhandled msg id"
                );
                Ok(vec![])
            }
        }
    }

    fn append_gps(&self, loc: &GpsLocation) {
        // E7 -> degrees; mm -> m; mm/s -> m/s; E6 -> degrees.
        let lat = loc.latitude as f64 / 1e7;
        let lon = loc.longitude as f64 / 1e7;
        let alt_m = loc.altitude as f64 / 1000.0;
        let speed_mps = loc.speed as f64 / 1000.0;
        let bearing_deg = loc.bearing as f64 / 1e6;
        let accuracy_m = loc.accuracy as f64 / 1000.0;

        // Build NDJSON by hand (no serde dep): cheap, deterministic, single
        // line per record.
        let line = format!(
            "{{\"ts\":{},\"lat\":{:.7},\"lon\":{:.7},\"alt\":{:.3},\"speed_mps\":{:.3},\"bearing_deg\":{:.3},\"accuracy_m\":{:.3}}}\n",
            loc.timestamp, lat, lon, alt_m, speed_mps, bearing_deg, accuracy_m
        );

        tracing::info!(
            ts = loc.timestamp,
            lat,
            lon,
            alt_m,
            speed_mps,
            bearing_deg,
            accuracy_m,
            "sensor: GPS fix"
        );

        // Forward to the nav engine if one is wired up. try_send so a
        // backlogged downstream can't stall this handler; the runner is
        // expected to drain quickly so this is rare in practice.
        if let Some(sink) = self.gps_sink.as_ref() {
            let fix = PositionFix {
                timestamp_ms: loc.timestamp as i64,
                lat,
                lon,
                bearing_deg,
                speed_mps,
                accuracy_m,
            };
            match sink.try_send(fix) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(_)) => {
                    tracing::debug!("sensor: nav sink full, dropping fix");
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    tracing::debug!("sensor: nav sink closed");
                }
            }
        }

        let mut guard = match self.file.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        if guard.is_none() {
            // Lazily open. Parent dir might not exist (e.g. running outside
            // Android) — try to create it; if even that fails just drop the
            // log silently and let tracing carry it.
            if let Some(parent) = self.gps_log_path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            match std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.gps_log_path)
            {
                Ok(f) => *guard = Some(f),
                Err(e) => {
                    tracing::warn!(
                        path = %self.gps_log_path.display(),
                        "sensor: cannot open GPS log: {e}"
                    );
                    return;
                }
            }
        }
        if let Some(f) = guard.as_mut() {
            if let Err(e) = f.write_all(line.as_bytes()) {
                tracing::warn!("sensor: GPS log write failed: {e}");
            } else {
                let _ = f.flush();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn start_request_replies_ok() {
        let h = SensorHandler::default();
        let req = SensorStartRequestMessage {
            sensor_type: 1,
            refresh_interval: 1000,
        };
        let mut buf = Vec::new();
        req.encode(&mut buf).unwrap();
        let out = h.handle(MSG_SENSOR_START_REQUEST, &buf).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].channel, ChannelId::Sensor as u8);
        assert_eq!(out[0].message_id, MSG_SENSOR_START_RESPONSE);
        let resp = SensorStartResponseMessage::decode(&out[0].payload[..]).unwrap();
        assert_eq!(resp.status, status::Enum::Ok as i32);
    }

    #[test]
    fn gps_event_writes_log_line() {
        // Pick a temp file inside Cargo's per-test scratch dir.
        let tmp = std::env::temp_dir().join(format!(
            "aabox-gps-test-{}.log",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&tmp);

        let h = SensorHandler::new(&tmp);
        let ev = SensorEventIndication {
            gps_location: vec![GpsLocation {
                timestamp: 1747440000_000, // 2026-05-16T... in ms
                latitude: 38_897_675,      // 3.8897675° (scaled E7 — 38.8976750)
                longitude: -770_365_000,   // -77.0365000° (scaled E7)
                accuracy: 5000,            // 5 m
                altitude: 50_000,          // 50 m
                speed: 13_410,             // 13.410 m/s (~30 mph)
                bearing: 90_000_000,       // 90° E
            }],
            ..Default::default()
        };
        let mut buf = Vec::new();
        ev.encode(&mut buf).unwrap();
        let out = h.handle(MSG_SENSOR_EVENT_INDICATION, &buf).unwrap();
        assert!(out.is_empty(), "sensor events don't generate replies");

        let mut s = String::new();
        std::fs::File::open(&tmp)
            .unwrap()
            .read_to_string(&mut s)
            .unwrap();
        assert!(s.contains("\"lat\":3.8897675"), "got: {s}");
        assert!(s.contains("\"lon\":-77.0365000"), "got: {s}");
        assert!(s.contains("\"speed_mps\":13.410"), "got: {s}");
        assert!(s.contains("\"bearing_deg\":90.000"), "got: {s}");
        assert!(s.ends_with('\n'));
        let _ = std::fs::remove_file(&tmp);
    }
}
