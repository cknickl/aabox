//! Navigation Status channel handler.
//!
//! Unlike sensor / input / AV, navigation is *write-heavy* from our side: WE
//! deliver turn-by-turn instructions to the car. The car renders a textual
//! status line, a turn icon, and (optionally) plays the TTS audio prompt we
//! ship through the speech-audio channel.
//!
//! The aasdk .proto files don't ship dedicated `NavigationStatus` /
//! `NavigationTurnEvent` message definitions because that channel is part of
//! the wider Google projection profile and the message *bodies* are not in
//! the public aasdk_proto set. The wire shape is well-known from headunit-go
//! / openauto sources:
//!
//!   - 0x8001 NAVIGATION_STATUS         (us -> car): status text + icon bitmap
//!   - 0x8002 NAVIGATION_TURN_EVENT     (us -> car): per-turn metadata
//!   - 0x8003 NAVIGATION_DISTANCE_EVENT (us -> car): distance to next turn
//!
//! The car generally doesn't send anything back on this channel beyond
//! `ChannelOpenRequest`/`NavigationFocusRequest` (which live on the control
//! channel, not here). So our `handle` mostly logs unexpected inbound.
//!
//! `NavInstruction` is the internal API: anything upstream can push
//! instructions through an `mpsc` channel and they'll come out the wire.
//! Phase 6 will hook this up to a real OSM router; for now there's a
//! `demo_straight_ahead` helper so the channel isn't dead silent.

use super::OutboundMessage;
use aabox_common::ChannelId;
use bytes::{BufMut, BytesMut};

pub const MSG_NAVIGATION_STATUS: u16 = 0x8001;
pub const MSG_NAVIGATION_TURN_EVENT: u16 = 0x8002;
pub const MSG_NAVIGATION_DISTANCE_EVENT: u16 = 0x8003;

/// A high-level navigation instruction emitted by an upstream router.
/// The control loop encodes it into a NavigationStatus / TurnEvent /
/// DistanceEvent triple as needed.
#[derive(Debug, Clone)]
pub struct NavInstruction {
    /// Human-readable status text — "Turn left in 200 m onto Main St".
    pub status_text: String,
    /// Next manoeuvre (TBT icon index, see openauto's TurnEvent::Type).
    /// 1 = straight, 2 = slight left, 3 = left, 4 = sharp left, 5 = u-turn,
    /// 6 = sharp right, 7 = right, 8 = slight right, etc.
    pub turn_id: u32,
    /// Distance to that manoeuvre, in metres.
    pub distance_m: u32,
}

impl NavInstruction {
    pub fn straight_ahead() -> Self {
        Self {
            status_text: "Continue straight".to_string(),
            turn_id: 1,
            distance_m: 0,
        }
    }
}

/// Encode a `NavInstruction` as a flight of OutboundMessages ready for the
/// control loop to encrypt + send. Layout is a hand-rolled non-prost shape
/// matching openauto's NavStatus / TurnEvent / DistanceEvent (these aren't
/// in aasdk_proto so we can't lean on prost here).
pub fn encode_instruction(instr: &NavInstruction) -> Vec<OutboundMessage> {
    // NavigationStatus: status enum (u8) + utf-8 status string with leading
    // length prefix. openauto uses status=1 for "in transit".
    let mut status_body = BytesMut::new();
    status_body.put_u8(1);
    let text = instr.status_text.as_bytes();
    // length-prefixed (u16 BE) UTF-8.
    status_body.put_u16(text.len() as u16);
    status_body.put_slice(text);

    let mut turn_body = BytesMut::new();
    turn_body.put_u32(instr.turn_id);

    let mut dist_body = BytesMut::new();
    dist_body.put_u32(instr.distance_m);

    let ch = ChannelId::Navigation as u8;
    vec![
        OutboundMessage::new(ch, MSG_NAVIGATION_STATUS, status_body.freeze()),
        OutboundMessage::new(ch, MSG_NAVIGATION_TURN_EVENT, turn_body.freeze()),
        OutboundMessage::new(ch, MSG_NAVIGATION_DISTANCE_EVENT, dist_body.freeze()),
    ]
}

/// Handle (rare) inbound nav-channel traffic. The car normally only writes
/// here for synchronisation pings.
pub fn handle(msg_id: u16, payload: &[u8]) -> anyhow::Result<Vec<OutboundMessage>> {
    tracing::info!(
        msg_id = format!("0x{:04x}", msg_id),
        len = payload.len(),
        "nav: inbound (unexpected; logged for analysis)"
    );
    Ok(vec![])
}

/// Spawn a tokio task that emits a periodic synthetic "straight ahead"
/// instruction every `interval`. Intended only as a placeholder so QA can
/// confirm wire-level nav messages reach the head unit; Phase 6 replaces it
/// with the real router. Drop the returned JoinHandle to cancel.
pub fn spawn_demo(
    tx: tokio::sync::mpsc::Sender<NavInstruction>,
    interval: std::time::Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut iv = tokio::time::interval(interval);
        // Skip the immediate first tick — let the car finish channel opens
        // before we start chattering.
        iv.tick().await;
        loop {
            iv.tick().await;
            if tx.send(NavInstruction::straight_ahead()).await.is_err() {
                tracing::info!("nav: demo sender stopping (rx dropped)");
                break;
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_emits_three_messages() {
        let out = encode_instruction(&NavInstruction {
            status_text: "Turn left now".into(),
            turn_id: 3,
            distance_m: 50,
        });
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].message_id, MSG_NAVIGATION_STATUS);
        assert_eq!(out[1].message_id, MSG_NAVIGATION_TURN_EVENT);
        assert_eq!(out[2].message_id, MSG_NAVIGATION_DISTANCE_EVENT);
        // All three go on the nav channel.
        for m in &out {
            assert_eq!(m.channel, ChannelId::Navigation as u8);
        }
        // Distance body is the 4-byte BE u32.
        assert_eq!(&out[2].payload[..], &[0, 0, 0, 50]);
    }
}
