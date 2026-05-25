//! Destination configured via a JSON file on disk.
//!
//! On a deployed AABox the user picks a destination from a UI app (TODO,
//! Phase 7+); for now the simplest way to plumb a destination into the
//! daemon at runtime is a tiny JSON file the UI writes:
//!
//! ```json
//! {"lat": 37.7749, "lon": -122.4194, "label": "Apple HQ"}
//! ```
//!
//! Path default: `/data/local/tmp/aabox-nav-destination.json` (matches the
//! sensor channel's NDJSON log path convention).
//!
//! We don't pull `inotify` (extra dep, Linux-only API). Instead we poll the
//! file's mtime every `poll_interval` and re-parse when it changes. The
//! polling overhead is negligible (one stat() per second), and it works
//! identically on any FS — including the CM5's overlayfs / vendor partition
//! where inotify is unreliable.
//!
//! The CLI can also point at the same file format, so a one-shot
//! `--nav-destination "lat,lon,label"` becomes "write that to the watch file"
//! at startup and the watcher does the rest.

use super::source::Destination;
use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use tokio::sync::mpsc;

/// On-disk schema. We accept `lat`/`lon` (preferred) but also `latitude` /
/// `longitude` aliases so a hand-written file in either style works.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DestinationFile {
    #[serde(alias = "latitude")]
    pub lat: f64,
    #[serde(alias = "longitude")]
    pub lon: f64,
    #[serde(default)]
    pub label: String,
}

impl DestinationFile {
    pub fn into_destination(self) -> Destination {
        let label = if self.label.is_empty() {
            format!("{:.4},{:.4}", self.lat, self.lon)
        } else {
            self.label
        };
        Destination::new(self.lat, self.lon, label)
    }

    /// Read and parse the JSON file at `path`. Missing file = `Ok(None)`,
    /// parse failure = `Err`.
    pub fn read_from<P: AsRef<Path>>(path: P) -> Result<Option<Self>> {
        let p = path.as_ref();
        let s = match std::fs::read_to_string(p) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(anyhow!("read {}: {}", p.display(), e)),
        };
        let parsed: Self = serde_json::from_str(&s)
            .with_context(|| format!("parse JSON at {}", p.display()))?;
        Ok(Some(parsed))
    }

    /// Parse a CLI-style `lat,lon[,label]` triple. Whitespace is trimmed
    /// around each field. Empty label is allowed.
    pub fn parse_cli(s: &str) -> Result<Self> {
        let parts: Vec<&str> = s.splitn(3, ',').map(|p| p.trim()).collect();
        if parts.len() < 2 {
            return Err(anyhow!(
                "expected 'lat,lon[,label]', got: {:?}",
                s
            ));
        }
        let lat: f64 = parts[0]
            .parse()
            .with_context(|| format!("parse lat from {:?}", parts[0]))?;
        let lon: f64 = parts[1]
            .parse()
            .with_context(|| format!("parse lon from {:?}", parts[1]))?;
        let label = parts.get(2).map(|s| s.to_string()).unwrap_or_default();
        Ok(Self { lat, lon, label })
    }
}

/// Spawn a tokio task that polls `path` and pushes a `Destination` onto `tx`
/// every time the file's mtime changes. The initial read (at task start) is
/// also pushed if the file exists. Returns the JoinHandle so callers can
/// cancel.
///
/// `poll_interval` is typically 1s for the daemon; tests use 50ms.
pub fn watch_destination_file<P: AsRef<Path>>(
    path: P,
    poll_interval: std::time::Duration,
    tx: mpsc::Sender<Destination>,
) -> tokio::task::JoinHandle<()> {
    let path: PathBuf = path.as_ref().to_path_buf();
    tokio::spawn(async move {
        let mut last_mtime: Option<SystemTime> = None;
        let mut iv = tokio::time::interval(poll_interval);
        // Skip the immediate first tick of `interval` (defaults to fire-now)
        // — we want the *next* tick so the loop body runs exactly once per
        // poll_interval. That said, we *do* want to do the initial read
        // immediately, so we just call the body directly first.
        check_and_push(&path, &mut last_mtime, &tx).await;
        loop {
            iv.tick().await;
            if tx.is_closed() {
                tracing::info!(path = %path.display(), "nav-watch: receiver dropped, stopping");
                return;
            }
            check_and_push(&path, &mut last_mtime, &tx).await;
        }
    })
}

async fn check_and_push(
    path: &Path,
    last_mtime: &mut Option<SystemTime>,
    tx: &mpsc::Sender<Destination>,
) {
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // File not (yet) present. Reset our state so a future create
            // counts as a change.
            *last_mtime = None;
            return;
        }
        Err(e) => {
            tracing::warn!(path = %path.display(), "nav-watch: stat failed: {e}");
            return;
        }
    };
    let mtime = meta.modified().ok();
    if mtime == *last_mtime {
        return;
    }
    *last_mtime = mtime;
    match DestinationFile::read_from(path) {
        Ok(Some(df)) => {
            let dest = df.into_destination();
            tracing::info!(
                path = %path.display(),
                label = %dest.label,
                lat = dest.lat,
                lon = dest.lon,
                "nav-watch: new destination"
            );
            let _ = tx.send(dest).await;
        }
        Ok(None) => {}
        Err(e) => {
            tracing::warn!(path = %path.display(), "nav-watch: parse failed: {e:#}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_cli_basic() {
        let d = DestinationFile::parse_cli("37.7749,-122.4194,Apple HQ").unwrap();
        assert!((d.lat - 37.7749).abs() < 1e-9);
        assert!((d.lon - -122.4194).abs() < 1e-9);
        assert_eq!(d.label, "Apple HQ");
    }

    #[test]
    fn parse_cli_no_label() {
        let d = DestinationFile::parse_cli("1.5,2.5").unwrap();
        assert!(d.label.is_empty());
    }

    #[test]
    fn parse_cli_rejects_malformed() {
        assert!(DestinationFile::parse_cli("not-a-number,0").is_err());
        assert!(DestinationFile::parse_cli("oneonly").is_err());
    }

    #[test]
    fn read_from_missing_returns_none() {
        let p = std::env::temp_dir().join(format!(
            "aabox-nav-watch-missing-{}.json",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&p);
        assert!(DestinationFile::read_from(&p).unwrap().is_none());
    }

    #[test]
    fn read_from_supports_aliases() {
        let p = std::env::temp_dir().join(format!(
            "aabox-nav-watch-alias-{}.json",
            std::process::id()
        ));
        std::fs::write(
            &p,
            r#"{"latitude": 40.0, "longitude": -75.0, "label": "Phila"}"#,
        )
        .unwrap();
        let d = DestinationFile::read_from(&p).unwrap().unwrap();
        assert!((d.lat - 40.0).abs() < 1e-9);
        assert!((d.lon - -75.0).abs() < 1e-9);
        let _ = std::fs::remove_file(&p);
    }

    #[tokio::test]
    async fn watch_emits_on_initial_and_change() {
        let p = std::env::temp_dir().join(format!(
            "aabox-nav-watch-test-{}-{}.json",
            std::process::id(),
            uniq_id()
        ));
        let _ = std::fs::remove_file(&p);
        std::fs::write(
            &p,
            r#"{"lat": 1.0, "lon": 2.0, "label": "first"}"#,
        )
        .unwrap();

        let (tx, mut rx) = mpsc::channel::<Destination>(4);
        let _h = watch_destination_file(&p, std::time::Duration::from_millis(20), tx);

        let first =
            tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
                .await
                .expect("initial recv timed out")
                .unwrap();
        assert_eq!(first.label, "first");

        // Rewrite the file with a different label. Sleep enough for the
        // filesystem mtime resolution to change.
        tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
        std::fs::write(
            &p,
            r#"{"lat": 3.0, "lon": 4.0, "label": "second"}"#,
        )
        .unwrap();

        let second =
            tokio::time::timeout(std::time::Duration::from_secs(3), rx.recv())
                .await
                .expect("second recv timed out")
                .unwrap();
        assert_eq!(second.label, "second");
        assert!((second.lat - 3.0).abs() < 1e-9);
        let _ = std::fs::remove_file(&p);
    }

    fn uniq_id() -> u64 {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        COUNTER.fetch_add(1, Ordering::Relaxed)
    }
}

