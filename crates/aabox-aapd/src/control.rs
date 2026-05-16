//! Control-channel protocol logic.
//!
//! The Control channel (ID 0) carries the protocol's lifecycle exchanges:
//! version handshake (plaintext), SSL tunnel handshake (the rustls handshake
//! bytes are wrapped in AAP frames with msg id `SSL_HANDSHAKE`), then
//! service discovery and channel-open negotiations.
//!
//! This module is transport-agnostic: it takes anything that implements
//! `AsyncRead + AsyncWrite` (a TcpStream for DHU mode, a /dev/usb_accessory
//! file in the hardware path). Phase 3 milestone: get all the way to
//! ServiceDiscoveryResponse sent over the encrypted tunnel.

use crate::framing::{Frame, FrameType};
use aabox_common::{ChannelId, ControlMessageId};
use anyhow::{anyhow, bail, Context, Result};
use bytes::{BufMut, BytesMut};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Major/minor version we advertise. DHU 2.0 (mac-arm64) announces 1.7;
/// real cars in the wild speak 1.x with x = 1..7 typically.
pub const PROTOCOL_MAJOR: u16 = 1;
pub const PROTOCOL_MINOR: u16 = 7;

/// Perform the plaintext version handshake — INITIATOR side (head-unit role
/// in AAP terms). Sends VersionRequest, waits for VersionResponse. Returns
/// the peer's (major, minor, status) tuple. Used when we're acting as the
/// car (e.g., the in-process fake head unit in our integration tests).
pub async fn version_handshake_initiator<S>(stream: &mut S) -> Result<(u16, u16, u16)>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    // Body: major (u16 BE) + minor (u16 BE).
    let mut body = BytesMut::with_capacity(4);
    body.put_u16(PROTOCOL_MAJOR);
    body.put_u16(PROTOCOL_MINOR);

    let req = Frame::bulk_control(
        ChannelId::Control as u8,
        ControlMessageId::VersionRequest as u16,
        &body,
    );
    write_frame(stream, &req).await?;
    tracing::info!(major = PROTOCOL_MAJOR, minor = PROTOCOL_MINOR, "VersionRequest sent");

    let resp = read_frame(stream).await?;
    if resp.channel_id != ChannelId::Control as u8 {
        bail!("expected control channel, got {}", resp.channel_id);
    }
    // DHU 2.0 (mac-arm64) sends a 6-byte VersionResponse: msg_id + major + minor,
    // no status. Older aasdk-shaped head units send 8 bytes with a trailing u16
    // status. Accept either.
    if resp.payload.len() < 6 {
        bail!("VersionResponse payload too short: {} (need >=6)", resp.payload.len());
    }
    tracing::debug!(payload_hex = %hex_dump(&resp.payload), "VersionResponse raw payload");
    let msg_id = u16::from_be_bytes([resp.payload[0], resp.payload[1]]);
    if msg_id != ControlMessageId::VersionResponse as u16 {
        bail!("expected VersionResponse (0x0002), got 0x{:04x}", msg_id);
    }
    let major = u16::from_be_bytes([resp.payload[2], resp.payload[3]]);
    let minor = u16::from_be_bytes([resp.payload[4], resp.payload[5]]);
    let status = if resp.payload.len() >= 8 {
        u16::from_be_bytes([resp.payload[6], resp.payload[7]])
    } else {
        0 // implicit OK when DHU omits the field
    };
    tracing::info!(major, minor, status, "VersionResponse received");
    Ok((major, minor, status))
}

fn hex_dump(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect::<Vec<_>>().join(" ")
}

/// Perform the plaintext version handshake — RESPONDER side (source role,
/// e.g., our daemon when DHU or a real car is the head unit). Reads
/// VersionRequest, replies with VersionResponse. Returns the peer's
/// (major, minor) advertised version.
pub async fn version_handshake_responder<S>(stream: &mut S) -> Result<(u16, u16)>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let req = read_frame(stream).await.context("read VersionRequest frame")?;
    if req.channel_id != ChannelId::Control as u8 {
        bail!("expected control channel, got {}", req.channel_id);
    }
    if req.payload.len() < 6 {
        bail!("VersionRequest payload too short: {}", req.payload.len());
    }
    let msg_id = u16::from_be_bytes([req.payload[0], req.payload[1]]);
    if msg_id != ControlMessageId::VersionRequest as u16 {
        bail!("expected VersionRequest (0x0001), got 0x{:04x}", msg_id);
    }
    let peer_major = u16::from_be_bytes([req.payload[2], req.payload[3]]);
    let peer_minor = u16::from_be_bytes([req.payload[4], req.payload[5]]);
    tracing::debug!(req_hex = %hex_dump(&req.payload), "VersionRequest raw payload");
    tracing::info!(peer_major, peer_minor, "VersionRequest received");

    // Reply with our own version. DHU 2.0 expects a 6-byte VersionResponse:
    // msg_id + major + minor only, no status field (sending status causes DHU
    // to reject the message as "unexpected"). Older aasdk-shaped head units
    // historically tolerated 8 bytes with a u16 status — but DHU and likely
    // newer cars are strict on the 6-byte form.
    let mut body = BytesMut::with_capacity(4);
    body.put_u16(PROTOCOL_MAJOR);
    body.put_u16(PROTOCOL_MINOR);

    let resp = Frame::bulk_control(
        ChannelId::Control as u8,
        ControlMessageId::VersionResponse as u16,
        &body,
    );
    write_frame(stream, &resp).await.context("write VersionResponse")?;
    tracing::info!(major = PROTOCOL_MAJOR, minor = PROTOCOL_MINOR, "VersionResponse sent");
    Ok((peer_major, peer_minor))
}

/// Write one frame to an async stream.
pub async fn write_frame<S>(stream: &mut S, frame: &Frame) -> Result<()>
where
    S: tokio::io::AsyncWrite + Unpin,
{
    let bytes = frame.encode();
    stream.write_all(&bytes).await.context("write frame")?;
    stream.flush().await.context("flush frame")?;
    Ok(())
}

/// Read one frame off an async stream. For Phase 3 we only support BULK frames
/// (FIRST|LAST set) since the control channel never fragments in practice.
/// Multi-fragment reassembly comes when we hit AV payloads in Phase 4.
pub async fn read_frame<S>(stream: &mut S) -> Result<Frame>
where
    S: tokio::io::AsyncRead + Unpin,
{
    let mut header = [0u8; 4];
    stream.read_exact(&mut header).await.context("read frame header")?;
    let channel_id = header[0];
    let flags = header[1];
    let payload_len = u16::from_be_bytes([header[2], header[3]]) as usize;

    let frame_type = match flags & 0x03 {
        0x03 => FrameType::Bulk,
        0x01 => FrameType::First,
        0x02 => FrameType::Last,
        _ => FrameType::Middle,
    };

    let total_length = if frame_type == FrameType::First {
        let mut tl = [0u8; 4];
        stream.read_exact(&mut tl).await.context("read total length")?;
        Some(u32::from_be_bytes(tl))
    } else {
        None
    };

    let mut payload = vec![0u8; payload_len];
    stream.read_exact(&mut payload).await.context("read frame payload")?;

    Ok(Frame {
        channel_id,
        frame_type,
        control: flags & 0x04 != 0,
        encrypted: flags & 0x08 != 0,
        total_length,
        payload: payload.into(),
    })
}

/// Wrap a payload as an SSL_HANDSHAKE message and write it as a BULK control
/// frame. Used to tunnel rustls handshake bytes through AAP.
pub fn ssl_handshake_frame(body: &[u8]) -> Frame {
    Frame::bulk_control(
        ChannelId::Control as u8,
        ControlMessageId::SslHandshake as u16,
        body,
    )
}

/// Extract the inner SSL payload from an SSL_HANDSHAKE frame. Errors if the
/// frame isn't a control-channel SslHandshake.
pub fn ssl_handshake_body(frame: &Frame) -> Result<&[u8]> {
    if frame.channel_id != ChannelId::Control as u8 || !frame.control {
        bail!("not a control-channel frame");
    }
    if frame.payload.len() < 2 {
        bail!("control payload too short");
    }
    let msg_id = u16::from_be_bytes([frame.payload[0], frame.payload[1]]);
    if msg_id != ControlMessageId::SslHandshake as u16 {
        bail!("expected SSL_HANDSHAKE (0x0003), got 0x{:04x}", msg_id);
    }
    Ok(&frame.payload[2..])
}

#[allow(dead_code)]
fn _ensure_used() -> Result<()> {
    Err(anyhow!("placeholder"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssl_handshake_frame_shape() {
        let body = b"\x16\x03\x03\x00\x00"; // fake TLS record header
        let f = ssl_handshake_frame(body);
        let bytes = f.encode();
        // First 4 bytes header, then msg id (2 bytes BE), then body
        assert_eq!(bytes[0], 0); // control channel
        assert_eq!(bytes[1] & 0x07, 0x07); // BULK + CONTROL
        assert_eq!(u16::from_be_bytes([bytes[2], bytes[3]]), 7);
        assert_eq!(u16::from_be_bytes([bytes[4], bytes[5]]), 0x0003);
        assert_eq!(&bytes[6..], body);
    }

    #[test]
    fn ssl_handshake_body_extracts() {
        let body = b"\x16\x03\x03\x00\x00";
        let f = ssl_handshake_frame(body);
        let extracted = ssl_handshake_body(&f).unwrap();
        assert_eq!(extracted, body);
    }

    #[test]
    fn ssl_handshake_body_rejects_wrong_msg_id() {
        let f = Frame::bulk_control(
            ChannelId::Control as u8,
            ControlMessageId::VersionRequest as u16,
            b"oops",
        );
        assert!(ssl_handshake_body(&f).is_err());
    }
}
