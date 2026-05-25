//! Shared integration-test helpers.
//!
//! Each integration test file in `tests/` is compiled as its own crate by
//! Cargo, so this module exists to be `mod common;`-included by every test
//! that wants to drive the daemon as a fake head unit. Anything used only by
//! one test goes in that test file; anything reusable lives here.
//!
//! The flagship export is [`fake_head_unit::FakeHeadUnit`] — a Rust-native
//! replacement for Google's Desktop Head Unit (DHU) emulator. DHU 2.0 enforces
//! a cert-`O=` allowlist on the TLS handshake that rejects our embedded JVC
//! Kenwood cert, which makes DHU unusable for our CI loop. The `FakeHeadUnit`
//! drives the full client side of an AA session over TCP with no such gate.

#![allow(dead_code)] // Different test files exercise different helpers.

pub mod fake_head_unit;
