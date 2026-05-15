//! AA Protocol message types.
//!
//! Generated from aasdk's `aasdk_proto/` directory at build time. Until the
//! `references/aasdk` submodule is wired up, this crate is empty.

// The build script writes generated modules into $OUT_DIR. Each .proto package
// becomes a Rust module. We re-export them under their original names so callers
// can `use aabox_proto::tag::Tag` etc. once protos are wired.
//
// Example after wiring:
//   pub mod tag { include!(concat!(env!("OUT_DIR"), "/tag.rs")); }
//
// Add per-package re-exports here as the submodule lands.
