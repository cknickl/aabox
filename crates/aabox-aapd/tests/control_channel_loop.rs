//! End-to-end: post-TLS control-channel loop responds to a synthetic
//! ServiceDiscoveryRequest fed over an in-memory TCP pipe.
//!
//! Builds the same TLS-over-AAP tunnel as `encrypted_roundtrip.rs`, then
//! runs the real [`control_channel::run`] on the server side and drives it
//! as a fake head unit from the client side.

use aabox_aapd::control::{read_frame, write_frame};
use aabox_aapd::control_channel::{self, ControlLoopConfig};
use aabox_aapd::encrypted::{decrypt_payload, encrypt_payload};
use aabox_aapd::framing::{Frame, FrameType};
use aabox_aapd::{tls, tls_tunnel};
use aabox_common::{ChannelId, ControlMessageId};
use aabox_proto::enums::{audio_focus_state, status};
use aabox_proto::messages::{
    AudioFocusRequest, AudioFocusResponse, ChannelOpenRequest, ChannelOpenResponse,
    PingRequest, PingResponse, ServiceDiscoveryRequest, ServiceDiscoveryResponse,
};
use bytes::{BufMut, Bytes, BytesMut};
use prost::Message;
use rustls::pki_types::ServerName;
use rustls::{ClientConnection, ServerConnection};
use std::sync::Arc;
use tokio::net::{TcpListener, TcpStream};

/// Encrypt + frame + send a single control-channel request from the fake
/// head-unit side.
async fn send_req<C>(
    sock: &mut TcpStream,
    cln: &mut C,
    msg_id: u16,
    body: &[u8],
) where
    C: aabox_aapd::encrypted::Side,
{
    let mut plain = BytesMut::with_capacity(2 + body.len());
    plain.put_u16(msg_id);
    plain.put_slice(body);
    let ciphertext = encrypt_payload(cln, &plain).expect("encrypt");
    let frame = Frame {
        channel_id: ChannelId::Control as u8,
        frame_type: FrameType::Bulk,
        control: false,
        encrypted: true,
        total_length: None,
        payload: Bytes::from(ciphertext),
    };
    write_frame(sock, &frame).await.expect("send frame");
}

/// Read frames until we get one on `expected_chan` that decrypts to a
/// (msg_id, body) we can return. Skips plaintext AuthComplete + any
/// unrelated traffic.
async fn next_msg<C>(sock: &mut TcpStream, cln: &mut C) -> (u8, u16, Vec<u8>)
where
    C: aabox_aapd::encrypted::Side,
{
    loop {
        let f = read_frame(sock).await.expect("read frame");
        let plain = if f.encrypted {
            decrypt_payload(cln, &f.payload).expect("decrypt")
        } else {
            f.payload.to_vec()
        };
        if plain.len() < 2 {
            continue;
        }
        let id = u16::from_be_bytes([plain[0], plain[1]]);
        return (f.channel_id, id, plain[2..].to_vec());
    }
}

#[tokio::test]
async fn loop_responds_to_sdr_ping_focus_and_channel_open() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let local_addr = listener.local_addr().unwrap();

    let server_cfg = tls::build_server_config().expect("server cfg");
    let client_cfg = tls::build_client_config().expect("client cfg");

    let server = tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut srv = ServerConnection::new(server_cfg).unwrap();
        tls_tunnel::server_handshake(&mut sock, &mut srv).await.unwrap();

        // Use a non-default GPS log path inside the temp dir so this test
        // doesn't try to write under /data/local/tmp. Disable the demo
        // nav emitter so it doesn't race with the test assertions.
        let mut cfg = ControlLoopConfig::default();
        cfg.gps_log_path = std::env::temp_dir()
            .join(format!("aabox-loop-test-gps-{}.log", std::process::id()));
        cfg.demo_nav = false;
        let _ = control_channel::run(&mut sock, &mut srv, cfg).await;
    });

    let mut client = TcpStream::connect(local_addr).await.unwrap();
    let name: ServerName<'static> =
        ServerName::try_from("android.car").unwrap().to_owned();
    let mut cln = ClientConnection::new(Arc::clone(&client_cfg), name).unwrap();
    tls_tunnel::client_handshake(&mut client, &mut cln).await.unwrap();

    // First message from the server should be AuthComplete (plaintext on the
    // control channel, msg_id = 0x0004).
    let f = read_frame(&mut client).await.unwrap();
    assert_eq!(f.channel_id, ChannelId::Control as u8);
    assert!(!f.encrypted, "AuthComplete is sent plaintext");
    assert!(f.payload.len() >= 2);
    let auth_id = u16::from_be_bytes([f.payload[0], f.payload[1]]);
    assert_eq!(auth_id, ControlMessageId::AuthComplete as u16);

    // 1) Send ServiceDiscoveryRequest, expect ServiceDiscoveryResponse.
    let req = ServiceDiscoveryRequest {
        device_name: "Carnival".into(),
        device_brand: "Kia".into(),
    };
    let mut buf = Vec::new();
    req.encode(&mut buf).unwrap();
    send_req(
        &mut client,
        &mut cln,
        ControlMessageId::ServiceDiscoveryRequest as u16,
        &buf,
    )
    .await;
    let (chan, mid, body) = next_msg(&mut client, &mut cln).await;
    assert_eq!(chan, ChannelId::Control as u8);
    assert_eq!(mid, ControlMessageId::ServiceDiscoveryResponse as u16);
    let sdr = ServiceDiscoveryResponse::decode(&body[..]).unwrap();
    assert_eq!(sdr.head_unit_name, "AABox");
    assert!(
        sdr.channels.len() >= 6,
        "expected >=6 channels, got {}: {:?}",
        sdr.channels.len(),
        sdr.channels.iter().map(|c| c.channel_id).collect::<Vec<_>>()
    );

    // 2) PingRequest -> PingResponse echoing timestamp.
    let req = PingRequest {
        timestamp: 0x1122_3344_5566_7788,
    };
    let mut buf = Vec::new();
    req.encode(&mut buf).unwrap();
    send_req(
        &mut client,
        &mut cln,
        ControlMessageId::PingRequest as u16,
        &buf,
    )
    .await;
    let (_, mid, body) = next_msg(&mut client, &mut cln).await;
    assert_eq!(mid, ControlMessageId::PingResponse as u16);
    let resp = PingResponse::decode(&body[..]).unwrap();
    assert_eq!(resp.timestamp, 0x1122_3344_5566_7788);

    // 3) AudioFocusRequest -> AudioFocusResponse{GAIN}.
    let req = AudioFocusRequest { audio_focus_type: 1 };
    let mut buf = Vec::new();
    req.encode(&mut buf).unwrap();
    send_req(
        &mut client,
        &mut cln,
        ControlMessageId::AudioFocusRequest as u16,
        &buf,
    )
    .await;
    let (_, mid, body) = next_msg(&mut client, &mut cln).await;
    assert_eq!(mid, ControlMessageId::AudioFocusResponse as u16);
    let resp = AudioFocusResponse::decode(&body[..]).unwrap();
    assert_eq!(resp.audio_focus_state, audio_focus_state::Enum::Gain as i32);

    // 4) ChannelOpenRequest on the Video channel -> ChannelOpenResponse{OK}.
    let req = ChannelOpenRequest {
        priority: 0,
        channel_id: ChannelId::Video as i32,
    };
    let mut buf = Vec::new();
    req.encode(&mut buf).unwrap();
    send_req(
        &mut client,
        &mut cln,
        ControlMessageId::ChannelOpenRequest as u16,
        &buf,
    )
    .await;
    let (_, mid, body) = next_msg(&mut client, &mut cln).await;
    assert_eq!(mid, ControlMessageId::ChannelOpenResponse as u16);
    let resp = ChannelOpenResponse::decode(&body[..]).unwrap();
    assert_eq!(resp.status, status::Enum::Ok as i32);

    // Closing the client socket ends the server loop. Drop it explicitly so
    // the loop's read_frame returns Err and run() exits.
    drop(client);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), server).await;
}
