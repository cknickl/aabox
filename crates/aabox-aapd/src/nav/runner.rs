//! The nav runner: one tokio task that owns a [`NavInstructionSource`] and
//! pumps the instructions it emits into the control-channel's outbound mpsc.
//!
//! Inputs (all `tokio::sync::mpsc::Receiver`s):
//!
//!   - `dest_rx`: stream of new destinations. Typically driven by the
//!     [`crate::nav::destination_file::watch_destination_file`] poller, but
//!     a test can push directly.
//!   - `gps_rx`: stream of [`PositionFix`]es decoded from the AA sensor
//!     channel.
//!
//! Output:
//!
//!   - `nav_tx`: the `mpsc::Sender<NavInstruction>` owned by the
//!     control-channel loop. Each `Some(instr)` returned by the source is
//!     pushed straight through.
//!
//! Lifecycle: the runner stops when *any* of its inputs close (typically
//! `nav_tx`, when the control loop ends). On stop it logs an info-level
//! message; no special cleanup needed.

use super::source::{Destination, NavInstructionSource, PositionFix};
use crate::channels::nav::NavInstruction;
use tokio::sync::mpsc;

/// Configurable knobs for the runner.
pub struct NavRunnerConfig {
    /// If true, the runner emits an initial `current_instruction()` from the
    /// source as soon as it starts, *before* any GPS fix or destination
    /// arrives. Useful so the head unit sees nav traffic right away when the
    /// source is a [`crate::nav::source::StaticDemoSource`].
    pub emit_initial: bool,
}

impl Default for NavRunnerConfig {
    fn default() -> Self {
        Self { emit_initial: false }
    }
}

/// Spawn the runner. Returns the JoinHandle so the caller (the daemon's
/// control loop bring-up code) can `.abort()` it during shutdown.
pub fn spawn_nav_runner(
    mut source: Box<dyn NavInstructionSource + Send>,
    mut dest_rx: mpsc::Receiver<Destination>,
    mut gps_rx: mpsc::Receiver<PositionFix>,
    nav_tx: mpsc::Sender<NavInstruction>,
    cfg: NavRunnerConfig,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        if cfg.emit_initial {
            if let Some(instr) = source.current_instruction() {
                if nav_tx.send(instr).await.is_err() {
                    tracing::info!("nav-runner: nav_tx closed, exiting");
                    return;
                }
            }
        }

        loop {
            tokio::select! {
                Some(dest) = dest_rx.recv() => {
                    match source.set_destination(dest).await {
                        Ok(Some(instr)) => {
                            if nav_tx.send(instr).await.is_err() {
                                tracing::info!("nav-runner: nav_tx closed, exiting");
                                return;
                            }
                        }
                        Ok(None) => {
                            tracing::debug!("nav-runner: destination installed, awaiting fix");
                        }
                        Err(e) => {
                            tracing::warn!("nav-runner: set_destination failed: {e:#}");
                        }
                    }
                }
                Some(fix) = gps_rx.recv() => {
                    match source.update_position(fix).await {
                        Ok(Some(instr)) => {
                            if nav_tx.send(instr).await.is_err() {
                                tracing::info!("nav-runner: nav_tx closed, exiting");
                                return;
                            }
                        }
                        Ok(None) => {}
                        Err(e) => {
                            tracing::warn!("nav-runner: update_position failed: {e:#}");
                        }
                    }
                }
                else => {
                    tracing::info!("nav-runner: all input streams closed, exiting");
                    return;
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nav::source::StaticDemoSource;

    #[tokio::test]
    async fn runner_emits_on_destination_then_gps() {
        let (dest_tx, dest_rx) = mpsc::channel::<Destination>(4);
        let (gps_tx, gps_rx) = mpsc::channel::<PositionFix>(8);
        let (nav_tx, mut nav_rx) = mpsc::channel::<NavInstruction>(8);

        let src = Box::new(StaticDemoSource::default());
        let _h = spawn_nav_runner(
            src,
            dest_rx,
            gps_rx,
            nav_tx,
            NavRunnerConfig::default(),
        );

        dest_tx
            .send(Destination::new(38.0, -77.0, "test"))
            .await
            .unwrap();
        let instr = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            nav_rx.recv(),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(instr.turn_id, 1);

        gps_tx
            .send(PositionFix {
                timestamp_ms: 1,
                lat: 38.0,
                lon: -77.0,
                bearing_deg: 0.0,
                speed_mps: 0.0,
                accuracy_m: 5.0,
            })
            .await
            .unwrap();
        let instr = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            nav_rx.recv(),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(instr.turn_id, 1);
    }

    #[tokio::test]
    async fn runner_exits_when_nav_tx_closed() {
        let (_dest_tx, dest_rx) = mpsc::channel::<Destination>(4);
        let (gps_tx, gps_rx) = mpsc::channel::<PositionFix>(8);
        let (nav_tx, nav_rx) = mpsc::channel::<NavInstruction>(1);

        let src = Box::new(StaticDemoSource::default());
        let h = spawn_nav_runner(
            src,
            dest_rx,
            gps_rx,
            nav_tx,
            NavRunnerConfig { emit_initial: true },
        );

        // Drop the receiver so the *first* send fails.
        drop(nav_rx);
        // Push a fix to force the runner to try sending.
        let _ = gps_tx
            .send(PositionFix {
                timestamp_ms: 0,
                lat: 0.0,
                lon: 0.0,
                bearing_deg: 0.0,
                speed_mps: 0.0,
                accuracy_m: 0.0,
            })
            .await;

        tokio::time::timeout(std::time::Duration::from_secs(2), h)
            .await
            .expect("runner did not exit")
            .expect("runner panicked");
    }
}
