//! End-to-end: post-TLS control-channel loop streams real H.264 access
//! units on the video channel after the fake head unit walks through
//! ChannelOpen → SETUP_REQUEST → START_INDICATION.
//!
//! Asserts:
//!   - SETUP_RESPONSE.media_status = OK, max_unacked = 10.
//!   - The first reassembled AV_MEDIA_WITH_TIMESTAMP_INDICATION payload
//!     contains a valid H.264 SPS + PPS + IDR sequence in Annex-B form.
//!   - The 8-byte PTS prefix exists and is reasonable.

use aabox_aapd::channels::video::nal;
use aabox_aapd::control::{read_frame, write_frame};
use aabox_aapd::control_channel::{self, ControlLoopConfig};
use aabox_aapd::encrypted::{decrypt_payload, encrypt_payload};
use aabox_aapd::framing::{Frame, FrameType};
use aabox_aapd::{tls, tls_tunnel};
use aabox_common::{ChannelId, ControlMessageId};
use aabox_proto::enums::av_channel_setup_status;
use aabox_proto::messages::{
    AvChannelSetupRequest, AvChannelSetupResponse, AvChannelStartIndication,
    ChannelOpenRequest, ChannelOpenResponse, ServiceDiscoveryRequest,
};
use bytes::{BufMut, Bytes, BytesMut};
use prost::Message;
use rustls::pki_types::ServerName;
use rustls::{ClientConnection, ServerConnection};
use std::sync::Arc;
use tokio::net::{TcpListener, TcpStream};

// AV-channel msg IDs (mirroring crates/aabox-aapd/src/channels/audio.rs).
const MSG_SETUP_REQUEST: u16 = 0x8000;
const MSG_START_INDICATION: u16 = 0x8001;
const MSG_SETUP_RESPONSE: u16 = 0x8003;
const MSG_AV_MEDIA_WITH_TIMESTAMP_INDICATION: u16 = 0x0000;

/// Encrypt and send one (channel, msg_id, body) on the client side.
async fn send<C>(sock: &mut TcpStream, cln: &mut C, channel: u8, msg_id: u16, body: &[u8])
where
    C: aabox_aapd::encrypted::Side,
{
    let mut plain = BytesMut::with_capacity(2 + body.len());
    plain.put_u16(msg_id);
    plain.put_slice(body);
    let ciphertext = encrypt_payload(cln, &plain).expect("encrypt");
    let frame = Frame {
        channel_id: channel,
        frame_type: FrameType::Bulk,
        control: false,
        encrypted: true,
        total_length: None,
        payload: Bytes::from(ciphertext),
    };
    write_frame(sock, &frame).await.expect("send frame");
}

/// Inbound message + reassembly. Reads frames, decrypts each one, glues
/// FIRST + (MIDDLE*) + LAST chunks back together, and returns
/// `(channel, msg_id, body)` once a complete message is available.
///
/// Plaintext AuthComplete (control-channel, not encrypted) is filtered out
/// so callers only see encrypted application messages.
async fn next_message<C>(sock: &mut TcpStream, cln: &mut C) -> (u8, u16, Vec<u8>)
where
    C: aabox_aapd::encrypted::Side,
{
    let mut accum: Option<(u8, Vec<u8>)> = None;
    loop {
        let f = read_frame(sock).await.expect("read frame");
        let plain = if f.encrypted {
            decrypt_payload(cln, &f.payload).expect("decrypt")
        } else {
            // Skip plaintext frames (just AuthComplete in practice).
            continue;
        };
        match f.frame_type {
            FrameType::Bulk => {
                // Whole message in one frame.
                assert!(plain.len() >= 2, "plaintext too short");
                let id = u16::from_be_bytes([plain[0], plain[1]]);
                return (f.channel_id, id, plain[2..].to_vec());
            }
            FrameType::First => {
                accum = Some((f.channel_id, plain));
            }
            FrameType::Middle => {
                if let Some((_, acc)) = accum.as_mut() {
                    acc.extend_from_slice(&plain);
                }
            }
            FrameType::Last => {
                let (chan, mut acc) =
                    accum.take().expect("LAST without FIRST");
                acc.extend_from_slice(&plain);
                assert!(acc.len() >= 2, "reassembled plaintext too short");
                let id = u16::from_be_bytes([acc[0], acc[1]]);
                return (chan, id, acc[2..].to_vec());
            }
        }
    }
}

#[tokio::test]
async fn video_start_emits_h264_frames_on_wire() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let local_addr = listener.local_addr().unwrap();

    let server_cfg = tls::build_server_config().expect("server cfg");
    let client_cfg = tls::build_client_config().expect("client cfg");

    // Server task: full TLS-over-AAP handshake then run the real control
    // loop. The control loop owns the video pipeline.
    let server = tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut srv = ServerConnection::new(server_cfg).unwrap();
        tls_tunnel::server_handshake(&mut sock, &mut srv).await.unwrap();

        let mut cfg = ControlLoopConfig::default();
        cfg.gps_log_path = std::env::temp_dir()
            .join(format!("aabox-video-test-gps-{}.log", std::process::id()));
        cfg.demo_nav = false;
        let _ = control_channel::run(&mut sock, &mut srv, cfg).await;
    });

    // Client setup.
    let mut client = TcpStream::connect(local_addr).await.unwrap();
    let name: ServerName<'static> =
        ServerName::try_from("android.car").unwrap().to_owned();
    let mut cln = ClientConnection::new(Arc::clone(&client_cfg), name).unwrap();
    tls_tunnel::client_handshake(&mut client, &mut cln).await.unwrap();

    // 1) Service discovery — primes the loop.
    let req = ServiceDiscoveryRequest {
        device_name: "Carnival".into(),
        device_brand: "Kia".into(),
    };
    let mut buf = Vec::new();
    req.encode(&mut buf).unwrap();
    send(
        &mut client,
        &mut cln,
        ChannelId::Control as u8,
        ControlMessageId::ServiceDiscoveryRequest as u16,
        &buf,
    )
    .await;
    let (chan, mid, _body) = next_message(&mut client, &mut cln).await;
    assert_eq!(chan, ChannelId::Control as u8);
    assert_eq!(mid, ControlMessageId::ServiceDiscoveryResponse as u16);

    // 2) Open the video channel.
    let req = ChannelOpenRequest {
        priority: 0,
        channel_id: ChannelId::Video as i32,
    };
    let mut buf = Vec::new();
    req.encode(&mut buf).unwrap();
    send(
        &mut client,
        &mut cln,
        ChannelId::Control as u8,
        ControlMessageId::ChannelOpenRequest as u16,
        &buf,
    )
    .await;
    let (_chan, mid, body) = next_message(&mut client, &mut cln).await;
    assert_eq!(mid, ControlMessageId::ChannelOpenResponse as u16);
    let _ = ChannelOpenResponse::decode(&body[..]).unwrap();

    // 3) Video SETUP_REQUEST → SETUP_RESPONSE OK.
    let req = AvChannelSetupRequest { config_index: 1 }; // 720p config
    let mut buf = Vec::new();
    req.encode(&mut buf).unwrap();
    send(
        &mut client,
        &mut cln,
        ChannelId::Video as u8,
        MSG_SETUP_REQUEST,
        &buf,
    )
    .await;
    let (chan, mid, body) = next_message(&mut client, &mut cln).await;
    assert_eq!(chan, ChannelId::Video as u8);
    assert_eq!(mid, MSG_SETUP_RESPONSE);
    let resp = AvChannelSetupResponse::decode(&body[..]).unwrap();
    assert_eq!(resp.media_status, av_channel_setup_status::Enum::Ok as i32);
    assert_eq!(resp.max_unacked, 10);

    // 4) Send START_INDICATION — daemon should begin streaming.
    let ind = AvChannelStartIndication {
        session: 42,
        config: 1,
    };
    let mut buf = Vec::new();
    ind.encode(&mut buf).unwrap();
    send(
        &mut client,
        &mut cln,
        ChannelId::Video as u8,
        MSG_START_INDICATION,
        &buf,
    )
    .await;

    // 5) Pull the next message on the video channel. Should be the first
    //    AV_MEDIA_WITH_TIMESTAMP_INDICATION carrying the SPS+PPS+IDR triplet.
    let first_video = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        next_message(&mut client, &mut cln),
    )
    .await
    .expect("timed out waiting for first video frame");
    let (chan, mid, body) = first_video;
    assert_eq!(chan, ChannelId::Video as u8, "expected video channel");
    assert_eq!(
        mid, MSG_AV_MEDIA_WITH_TIMESTAMP_INDICATION,
        "expected AV_MEDIA_WITH_TIMESTAMP indication, got 0x{:04x}",
        mid
    );
    assert!(
        body.len() > 8,
        "payload too short: {} bytes (need PTS + at least 1 NAL byte)",
        body.len()
    );

    // 8-byte BE PTS prefix; first frame should be PTS=0.
    let pts = u64::from_be_bytes(body[..8].try_into().unwrap());
    assert_eq!(pts, 0, "first frame PTS should be zero");

    // Annex-B starts immediately after.
    let annex_b = &body[8..];
    assert_eq!(
        &annex_b[..4],
        &[0, 0, 0, 1],
        "expected Annex-B start code at offset 8, got {:02x?}",
        &annex_b[..4]
    );

    // Decompose into NAL units and check SPS + PPS + IDR are all present.
    let nals = nal::split_nals(annex_b);
    let types: Vec<u8> = nals.iter().map(|n| nal::nal_unit_type(n)).collect();
    assert!(
        types.contains(&nal::nut::SPS),
        "first video frame must contain SPS; got NAL types {:?}",
        types
    );
    assert!(
        types.contains(&nal::nut::PPS),
        "first video frame must contain PPS; got NAL types {:?}",
        types
    );
    assert!(
        types.contains(&nal::nut::SLICE_IDR),
        "first video frame must contain an IDR slice; got NAL types {:?}",
        types
    );

    // Drain and discard subsequent frames briefly so the streamer doesn't
    // immediately block on backpressure. Three is enough for sanity.
    for _ in 0..3 {
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            next_message(&mut client, &mut cln),
        )
        .await;
    }

    // Tear down.
    drop(client);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), server).await;
}
