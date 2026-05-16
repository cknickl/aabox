//! AAP wire framing — channel ID + flags + length + payload.
//!
//! Wire format derived from `references/aasdk/src/Messenger/FrameHeader.cpp`:
//!
//! ```text
//!   byte 0   : channel ID  (u8)
//!   byte 1   : flags       (FIRST=0x01 | LAST=0x02 | CONTROL=0x04 | ENCRYPTED=0x08)
//!   bytes 2-3: payload len (u16 BE; SHORT form)
//!   bytes 4-7: total len   (u32 BE; ONLY on FIRST when message is multi-fragment;
//!                          equals the SUM of all fragment payload lengths)
//!   payload  : `payload_len` bytes
//! ```
//!
//! Notes:
//! - A BULK frame (FIRST|LAST set) carries the entire message in one shot — no
//!   total-length field. This is the common case for short control messages.
//! - A FIRST-only frame begins a multi-fragment message; the 4-byte total
//!   length follows the 2-byte fragment length, then the payload.
//! - MIDDLE/LAST fragments have no total-length field; payload follows length.
//! - Video channels use the EXTENDED size form (u32 payload length) instead of
//!   u16. aasdk encodes this via `FrameSize` separately from `FrameHeader`;
//!   we keep it together here for simplicity and switch on channel ID.

use anyhow::Result;
use bytes::{Buf, BufMut, Bytes, BytesMut};

pub const FLAG_MIDDLE: u8 = 0x00;
pub const FLAG_FIRST: u8 = 0x01;
pub const FLAG_LAST: u8 = 0x02;
pub const FLAG_BULK: u8 = FLAG_FIRST | FLAG_LAST;
pub const FLAG_CONTROL: u8 = 0x04;
pub const FLAG_ENCRYPTED: u8 = 0x08;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameType {
    Middle,
    First,
    Last,
    Bulk,
}

impl FrameType {
    fn from_flags(flags: u8) -> Self {
        match flags & FLAG_BULK {
            FLAG_BULK => Self::Bulk,
            FLAG_FIRST => Self::First,
            FLAG_LAST => Self::Last,
            _ => Self::Middle,
        }
    }
    fn bits(self) -> u8 {
        match self {
            Self::Middle => FLAG_MIDDLE,
            Self::First => FLAG_FIRST,
            Self::Last => FLAG_LAST,
            Self::Bulk => FLAG_BULK,
        }
    }
}

/// A single parsed frame off the wire.
#[derive(Debug, Clone)]
pub struct Frame {
    pub channel_id: u8,
    pub frame_type: FrameType,
    pub control: bool,
    pub encrypted: bool,
    /// If `frame_type == First` and the message is multi-fragment, this is the
    /// total length across all fragments.
    pub total_length: Option<u32>,
    pub payload: Bytes,
}

impl Frame {
    pub fn encode_into(&self, dst: &mut BytesMut) {
        let flags = self.frame_type.bits()
            | if self.control { FLAG_CONTROL } else { 0 }
            | if self.encrypted { FLAG_ENCRYPTED } else { 0 };
        dst.put_u8(self.channel_id);
        dst.put_u8(flags);
        dst.put_u16(self.payload.len() as u16);
        if let (FrameType::First, Some(total)) = (self.frame_type, self.total_length) {
            dst.put_u32(total);
        }
        dst.put_slice(&self.payload);
    }

    pub fn encode(&self) -> Bytes {
        let mut buf = BytesMut::with_capacity(8 + self.payload.len());
        self.encode_into(&mut buf);
        buf.freeze()
    }

    /// Try to parse one frame off the front of `buf`. On success consumes the
    /// frame bytes from `buf` and returns the Frame. On `Ok(None)` the caller
    /// should read more bytes and retry. `Err` means a malformed frame —
    /// usually unrecoverable on a stream.
    pub fn parse(buf: &mut BytesMut) -> Result<Option<Frame>> {
        if buf.len() < 4 {
            return Ok(None);
        }
        let channel_id = buf[0];
        let flags = buf[1];
        let payload_len = u16::from_be_bytes([buf[2], buf[3]]) as usize;
        let frame_type = FrameType::from_flags(flags);

        let header_len = if frame_type == FrameType::First {
            8 // 4-byte header + 4-byte total length
        } else {
            4
        };

        if buf.len() < header_len + payload_len {
            return Ok(None);
        }

        buf.advance(4);
        let total_length = if frame_type == FrameType::First {
            Some(buf.get_u32())
        } else {
            None
        };
        let payload = buf.split_to(payload_len).freeze();

        Ok(Some(Frame {
            channel_id,
            frame_type,
            control: flags & FLAG_CONTROL != 0,
            encrypted: flags & FLAG_ENCRYPTED != 0,
            total_length,
            payload,
        }))
    }

    /// Convenience: build a BULK control-channel frame (the shape of most
    /// handshake messages: VersionRequest, ServiceDiscoveryResponse, etc.).
    ///
    /// Empirically (DHU 2.0 wire capture): the `CONTROL` flag bit (0x04) is
    /// NOT set on real wire frames — DHU rejects a `0x07` flag byte even
    /// though aasdk's MessageType::CONTROL enum value says it should be there.
    /// We mirror DHU's encoding and leave `control: false`.
    pub fn bulk_control(channel_id: u8, message_id: u16, body: &[u8]) -> Frame {
        let mut payload = BytesMut::with_capacity(2 + body.len());
        payload.put_u16(message_id);
        payload.put_slice(body);
        Frame {
            channel_id,
            frame_type: FrameType::Bulk,
            control: false,
            encrypted: false,
            total_length: None,
            payload: payload.freeze(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bulk_roundtrip() {
        let f = Frame::bulk_control(0, 1, &[0xDE, 0xAD, 0xBE, 0xEF]);
        let bytes = f.encode();
        // Header: chan=0, flags=BULK=0x03 (no CONTROL bit — DHU doesn't set it),
        // len=6 (msgid 2 + body 4)
        assert_eq!(&bytes[..4], &[0x00, 0x03, 0x00, 0x06]);
        // Payload: msgid 0x0001 + body
        assert_eq!(&bytes[4..], &[0x00, 0x01, 0xDE, 0xAD, 0xBE, 0xEF]);

        let mut buf = BytesMut::from(&bytes[..]);
        let parsed = Frame::parse(&mut buf).unwrap().unwrap();
        assert_eq!(parsed.channel_id, 0);
        assert_eq!(parsed.frame_type, FrameType::Bulk);
        assert!(!parsed.control);
        assert!(!parsed.encrypted);
        assert_eq!(parsed.total_length, None);
        assert_eq!(parsed.payload.len(), 6);
    }

    #[test]
    fn fragmented_first_carries_total_length() {
        let f = Frame {
            channel_id: 3,
            frame_type: FrameType::First,
            control: false,
            encrypted: true,
            total_length: Some(100_000),
            payload: Bytes::from(vec![0xAA; 1024]),
        };
        let bytes = f.encode();
        assert_eq!(bytes[0], 3);
        assert_eq!(bytes[1], FLAG_FIRST | FLAG_ENCRYPTED);
        assert_eq!(u16::from_be_bytes([bytes[2], bytes[3]]), 1024);
        assert_eq!(u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]), 100_000);

        let mut buf = BytesMut::from(&bytes[..]);
        let parsed = Frame::parse(&mut buf).unwrap().unwrap();
        assert_eq!(parsed.total_length, Some(100_000));
        assert_eq!(parsed.payload.len(), 1024);
    }

    #[test]
    fn parse_returns_none_on_partial_header() {
        let mut buf = BytesMut::from(&[0u8, 1u8, 0u8][..]);
        assert!(Frame::parse(&mut buf).unwrap().is_none());
    }

    #[test]
    fn parse_returns_none_on_partial_payload() {
        let mut buf = BytesMut::from(&[0u8, 0x07, 0x00, 0x10, 0xAA, 0xBB][..]);
        assert!(Frame::parse(&mut buf).unwrap().is_none());
    }
}
