//! Per-channel message handlers.
//!
//! After service discovery + ChannelOpenResponse, every non-zero channel has
//! its own message-id namespace. The control-channel message loop reads
//! encrypted frames off the wire and dispatches on the (channel, msg_id) pair
//! to one of the handlers in this module.
//!
//! Handlers are pure functions over a `ChannelContext` — they don't own the
//! I/O directly. Any messages they want to send back come out as
//! `Vec<OutboundMessage>`, and the control loop is responsible for encrypting
//! and framing them. This keeps the TLS session single-owner (no locks needed)
//! and makes handlers trivially testable.

use bytes::Bytes;

pub mod audio;
pub mod input;
pub mod nav;
pub mod sensor;
pub mod video;

/// A message ready to be wrapped in an AAP frame (channel + 2-byte msg-id +
/// payload). The control loop encrypts the (msg_id || payload) blob and
/// sends it on `channel` with the ENCRYPTED flag set.
#[derive(Debug, Clone)]
pub struct OutboundMessage {
    pub channel: u8,
    pub message_id: u16,
    pub payload: Bytes,
}

impl OutboundMessage {
    pub fn new<B: Into<Bytes>>(channel: u8, message_id: u16, payload: B) -> Self {
        Self {
            channel,
            message_id,
            payload: payload.into(),
        }
    }
}

/// Convenience: prost-encode a message and wrap into an OutboundMessage.
pub fn encode_outbound<M: prost::Message>(channel: u8, msg_id: u16, msg: &M) -> OutboundMessage {
    let mut buf = Vec::with_capacity(msg.encoded_len());
    msg.encode(&mut buf).expect("prost encode");
    OutboundMessage::new(channel, msg_id, buf)
}
