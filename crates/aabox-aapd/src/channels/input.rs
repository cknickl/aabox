//! Input channel handler.
//!
//! Wire shape: each payload starts with a 2-byte big-endian msg id from
//! `InputChannelMessage::Enum` (see ControlMessageIdsEnum.proto-style):
//!
//!   - 0x8001 INPUT_EVENT_INDICATION : car -> us (touch / button / wheel).
//!     We log it for now; Phase 5 will turn it into Android InputEvents on
//!     the CM5 side.
//!   - 0x8002 BINDING_REQUEST        : car -> us — "I want to send you these
//!     scancodes; ack so I know they're bound." We reply OK to whatever it
//!     asks for. Refusing here is how a head unit decides we don't accept a
//!     given hardware key; we have nothing to refuse.
//!   - 0x8003 BINDING_RESPONSE       : us -> car (the reply we generate).

use super::{encode_outbound, OutboundMessage};
use aabox_common::ChannelId;
use aabox_proto::enums::status;
use aabox_proto::messages::{BindingRequest, BindingResponse, InputEventIndication};
use anyhow::{anyhow, Result};
use prost::Message;

pub const MSG_INPUT_EVENT_INDICATION: u16 = 0x8001;
pub const MSG_BINDING_REQUEST: u16 = 0x8002;
pub const MSG_BINDING_RESPONSE: u16 = 0x8003;

/// Handle a single inbound input-channel message. `payload` is the inner
/// bytes after the 2-byte msg-id. Returns zero or more reply messages.
pub fn handle(msg_id: u16, payload: &[u8]) -> Result<Vec<OutboundMessage>> {
    match msg_id {
        MSG_BINDING_REQUEST => {
            let req = BindingRequest::decode(payload)
                .map_err(|e| anyhow!("decode BindingRequest: {e}"))?;
            tracing::info!(
                scan_codes = ?req.scan_codes,
                "input: BindingRequest -> ACK"
            );
            let resp = BindingResponse {
                status: status::Enum::Ok as i32,
            };
            Ok(vec![encode_outbound(
                ChannelId::Input as u8,
                MSG_BINDING_RESPONSE,
                &resp,
            )])
        }
        MSG_INPUT_EVENT_INDICATION => {
            // Log every event for visibility while we don't yet have an
            // Android-side InputEvent injector. Touch is the high-frequency
            // case so we summarise rather than dumping every TouchLocation.
            match InputEventIndication::decode(payload) {
                Ok(ev) => {
                    if let Some(t) = &ev.touch_event {
                        let n = t.touch_location.len();
                        let first = t.touch_location.first();
                        tracing::info!(
                            ts = ev.timestamp,
                            action = t.touch_action,
                            points = n,
                            x = first.map(|l| l.x),
                            y = first.map(|l| l.y),
                            "input: touch event"
                        );
                    } else if let Some(b) = &ev.button_event {
                        tracing::info!(
                            ts = ev.timestamp,
                            n_buttons = b.button_events.len(),
                            "input: button event"
                        );
                    } else {
                        tracing::info!(ts = ev.timestamp, "input: event (other)");
                    }
                }
                Err(e) => {
                    tracing::warn!("input: failed to decode InputEventIndication: {e}");
                }
            }
            Ok(vec![])
        }
        other => {
            tracing::warn!(msg_id = format!("0x{:04x}", other), "input: unhandled msg id");
            Ok(vec![])
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binding_request_acks() {
        let req = BindingRequest {
            scan_codes: vec![3, 4, 5],
        };
        let mut buf = Vec::new();
        req.encode(&mut buf).unwrap();
        let out = handle(MSG_BINDING_REQUEST, &buf).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].channel, ChannelId::Input as u8);
        assert_eq!(out[0].message_id, MSG_BINDING_RESPONSE);
        let resp = BindingResponse::decode(&out[0].payload[..]).unwrap();
        assert_eq!(resp.status, status::Enum::Ok as i32);
    }
}
