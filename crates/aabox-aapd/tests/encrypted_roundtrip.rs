//! End-to-end: TLS-over-AAP handshake, then exchange an encrypted
//! ServiceDiscoveryResponse between an in-process client and server.
//!
//! Proves the full Phase 3 critical path:
//!   1. AAP framing
//!   2. TLS handshake tunneled through AAP SslHandshake frames
//!   3. Application data encrypted by rustls, wrapped in an AAP frame with
//!      ENCRYPTED flag, transmitted, decrypted on the other side
//!   4. The decrypted bytes decode back to the original ServiceDiscoveryResponse

use aabox_aapd::control::{read_frame, write_frame};
use aabox_aapd::encrypted::{decrypt_payload, encrypt_payload};
use aabox_aapd::framing::{Frame, FrameType, FLAG_BULK, FLAG_ENCRYPTED};
use aabox_aapd::{services, tls, tls_tunnel};
use aabox_common::{ChannelId, ControlMessageId};
use aabox_proto::messages::ServiceDiscoveryResponse;
use bytes::{Bytes, BytesMut, BufMut};
use prost::Message;
use rustls::pki_types::ServerName;
use rustls::{ClientConnection, ServerConnection};
use std::sync::Arc;
use tokio::net::{TcpListener, TcpStream};

#[tokio::test]
async fn full_handshake_then_encrypted_sdr() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let local_addr = listener.local_addr().unwrap();

    let server_cfg = tls::build_server_config().expect("server cfg");
    let client_cfg = tls::build_client_config().expect("client cfg");

    let server = tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut srv = ServerConnection::new(server_cfg).unwrap();
        tls_tunnel::server_handshake(&mut sock, &mut srv).await.unwrap();
        assert!(!srv.is_handshaking());

        // Receive the encrypted SDR frame from client
        let frame = read_frame(&mut sock).await.unwrap();
        assert_eq!(frame.channel_id, ChannelId::Control as u8);
        assert!(frame.encrypted);
        let plaintext = decrypt_payload(&mut srv, &frame.payload).unwrap();

        // Plaintext layout: [u16 msg id BE][protobuf ServiceDiscoveryResponse]
        let msg_id = u16::from_be_bytes([plaintext[0], plaintext[1]]);
        assert_eq!(msg_id, ControlMessageId::ServiceDiscoveryResponse as u16);
        let sdr = ServiceDiscoveryResponse::decode(&plaintext[2..]).unwrap();
        assert_eq!(sdr.head_unit_name, "AABox");
        assert_eq!(sdr.channels.len(), 2);
    });

    let mut client = TcpStream::connect(local_addr).await.unwrap();
    let name: ServerName<'static> =
        ServerName::try_from("android.car").unwrap().to_owned();
    let mut cln = ClientConnection::new(Arc::clone(&client_cfg), name).unwrap();
    tls_tunnel::client_handshake(&mut client, &mut cln).await.unwrap();
    assert!(!cln.is_handshaking());

    // Build SDR + msg-id prefix, encrypt, send as a single AAP frame
    let sdr = services::minimal_response();
    let body = services::encode_response(&sdr);
    let mut plaintext = BytesMut::with_capacity(2 + body.len());
    plaintext.put_u16(ControlMessageId::ServiceDiscoveryResponse as u16);
    plaintext.put_slice(&body);
    let ciphertext = encrypt_payload(&mut cln, &plaintext).unwrap();
    assert!(!ciphertext.is_empty());

    let frame = Frame {
        channel_id: ChannelId::Control as u8,
        frame_type: FrameType::Bulk,
        control: false,
        encrypted: true,
        total_length: None,
        payload: Bytes::from(ciphertext),
    };
    // Sanity: encoded flags byte should be BULK | ENCRYPTED (no CONTROL bit;
    // DHU 2.0 doesn't use it).
    let encoded = frame.encode();
    assert_eq!(encoded[1], FLAG_BULK | FLAG_ENCRYPTED);
    write_frame(&mut client, &frame).await.unwrap();

    server.await.unwrap();
}
