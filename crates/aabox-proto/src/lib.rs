//! AA Protocol message types.
//!
//! Generated at build time from the .proto files in
//! `references/aasdk/aasdk_proto/` (which has package `f1x.aasdk.proto`).
//!
//! Use the convenience re-exports under the crate root, e.g.:
//!
//! ```ignore
//! use aabox_proto::enums::ChannelId;
//! use aabox_proto::messages::ServiceDiscoveryResponse;
//! ```
//!
//! The full path `aabox_proto::f1x::aasdk::proto::*` also works.

#[allow(clippy::all, clippy::pedantic, non_snake_case, non_camel_case_types)]
pub mod f1x {
    pub mod aasdk {
        pub mod proto {
            pub mod data {
                include!(concat!(env!("OUT_DIR"), "/f1x.aasdk.proto.data.rs"));
            }
            pub mod enums {
                include!(concat!(env!("OUT_DIR"), "/f1x.aasdk.proto.enums.rs"));
            }
            pub mod ids {
                include!(concat!(env!("OUT_DIR"), "/f1x.aasdk.proto.ids.rs"));
            }
            pub mod messages {
                include!(concat!(env!("OUT_DIR"), "/f1x.aasdk.proto.messages.rs"));
            }
        }
    }
}

// Convenience re-exports
pub use f1x::aasdk::proto::{data, enums, ids, messages};
