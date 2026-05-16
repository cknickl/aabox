//! End-to-end test: aabox-aapd's version handshake round-trips against a fake
//! head unit running in the same process. Proves the framing + control logic
//! work against a real TCP stream, not just unit-test byte slices.

use aabox_aapd::control;
use aabox_aapd::framing::{Frame, FrameType};
use aabox_common::{ChannelId, ControlMessageId};
use bytes::{BufMut, BytesMut};
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};

#[tokio::test]
async fn version_handshake_against_fake_head_unit() {
    // Start a fake head unit on an OS-chosen port.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let local_addr = listener.local_addr().unwrap();

    let server = tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        // Expect: a control-channel BULK frame with VersionRequest (msg id 0x0001).
        // DHU 2.0 doesn't set the CONTROL flag bit, and we mirror that.
        let req = control::read_frame(&mut sock).await.unwrap();
        assert_eq!(req.channel_id, ChannelId::Control as u8);
        assert_eq!(req.frame_type, FrameType::Bulk);
        assert!(!req.control);
        let msg_id = u16::from_be_bytes([req.payload[0], req.payload[1]]);
        assert_eq!(msg_id, ControlMessageId::VersionRequest as u16);
        let req_major = u16::from_be_bytes([req.payload[2], req.payload[3]]);
        let req_minor = u16::from_be_bytes([req.payload[4], req.payload[5]]);
        assert_eq!(req_major, control::PROTOCOL_MAJOR);
        assert_eq!(req_minor, control::PROTOCOL_MINOR);

        // Respond: VersionResponse w/ same major/minor + status=0 (OK)
        let mut body = BytesMut::with_capacity(6);
        body.put_u16(control::PROTOCOL_MAJOR);
        body.put_u16(control::PROTOCOL_MINOR);
        body.put_u16(0);
        let resp = Frame::bulk_control(
            ChannelId::Control as u8,
            ControlMessageId::VersionResponse as u16,
            &body,
        );
        sock.write_all(&resp.encode()).await.unwrap();
        sock.flush().await.unwrap();
    });

    // Client side: connect and run the handshake.
    let mut client = TcpStream::connect(local_addr).await.unwrap();
    let (major, minor, status) = control::version_handshake_initiator(&mut client).await.unwrap();
    assert_eq!(major, control::PROTOCOL_MAJOR);
    assert_eq!(minor, control::PROTOCOL_MINOR);
    assert_eq!(status, 0);

    server.await.unwrap();
}

/// Inverse direction — proves our `version_handshake_responder` works
/// against an *external* initiator (the role DHU plays). The "head unit"
/// here is the test driver itself; the responder is what `dhu-listen` runs.
#[tokio::test]
async fn version_handshake_responder_accepts_external_initiator() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let local_addr = listener.local_addr().unwrap();

    // Daemon side: listen + run responder.
    let daemon = tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let (peer_major, peer_minor) = control::version_handshake_responder(&mut sock).await.unwrap();
        assert_eq!(peer_major, control::PROTOCOL_MAJOR);
        assert_eq!(peer_minor, control::PROTOCOL_MINOR);
    });

    // Head unit (test driver): connect and send VersionRequest, expect VersionResponse.
    let mut hu = TcpStream::connect(local_addr).await.unwrap();
    let mut body = BytesMut::with_capacity(4);
    body.put_u16(control::PROTOCOL_MAJOR);
    body.put_u16(control::PROTOCOL_MINOR);
    let req = Frame::bulk_control(
        ChannelId::Control as u8,
        ControlMessageId::VersionRequest as u16,
        &body,
    );
    hu.write_all(&req.encode()).await.unwrap();
    hu.flush().await.unwrap();

    let resp = control::read_frame(&mut hu).await.unwrap();
    assert_eq!(resp.channel_id, ChannelId::Control as u8);
    let msg_id = u16::from_be_bytes([resp.payload[0], resp.payload[1]]);
    assert_eq!(msg_id, ControlMessageId::VersionResponse as u16);
    let major = u16::from_be_bytes([resp.payload[2], resp.payload[3]]);
    let minor = u16::from_be_bytes([resp.payload[4], resp.payload[5]]);
    assert_eq!((major, minor), (control::PROTOCOL_MAJOR, control::PROTOCOL_MINOR));

    daemon.await.unwrap();
}
