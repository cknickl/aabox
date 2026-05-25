//! Real navigation engine.
//!
//! This module sits *above* the navigation **channel** wire layer (which lives
//! in `channels/nav.rs` and just turns a [`NavInstruction`] into the three
//! openauto-shape outbound frames). What this module owns is the *source* of
//! those instructions:
//!
//!   - a [`NavInstructionSource`] trait that converts (destination, GPS fix)
//!     pairs into a stream of [`NavInstruction`]s,
//!   - an [`osrm::OsrmNavSource`] HTTP-based implementation backed by an OSRM
//!     server (defaults to the public demo at https://router.project-osrm.org),
//!   - a [`destination_file::DestinationWatcher`] that polls a small JSON file
//!     on disk so a UI app on the CM5 can change the destination at runtime
//!     without restarting the daemon,
//!   - a [`runner::spawn_nav_runner`] glue task that ties a [`PositionFix`]
//!     stream and a destination feed to a source and pumps the resulting
//!     instructions into the control loop's `mpsc::Sender<NavInstruction>`.
//!
//! The legacy `channels::nav::spawn_demo` is still wired up behind a config
//! flag so a no-router / no-network build (or a dev box with no destination
//! configured) still emits *something* and the head unit doesn't see a stale
//! nav channel.

pub mod destination_file;
pub mod osrm;
pub mod runner;
pub mod source;

pub use destination_file::{watch_destination_file, DestinationFile};
pub use osrm::{OsrmClient, OsrmNavSource};
pub use runner::{spawn_nav_runner, NavRunnerConfig};
pub use source::{
    Destination, NavInstructionSource, NavInstructionSourceExt, PositionFix, StaticDemoSource,
};
