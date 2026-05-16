//! End-to-end: the daemon's TLS-over-AAP handshake completes against an
//! in-process fake head unit. Proves rustls handshake bytes round-trip
//! correctly through SslHandshake control frames in both directions.

use aabox_aapd::{tls, tls_tunnel};
use rustls::pki_types::ServerName;
use rustls::{ClientConnection, ServerConnection};
use std::sync::Arc;
use tokio::net::{TcpListener, TcpStream};

#[tokio::test]
async fn tls_over_aap_handshake_round_trip() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let local_addr = listener.local_addr().unwrap();

    let server_cfg = tls::build_server_config().expect("server cfg");
    let client_cfg = tls::build_client_config().expect("client cfg");

    let server = tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut srv = ServerConnection::new(server_cfg).unwrap();
        tls_tunnel::server_handshake(&mut sock, &mut srv).await.unwrap();
        assert!(!srv.is_handshaking(), "server still handshaking");
    });

    let mut client = TcpStream::connect(local_addr).await.unwrap();
    let name: ServerName<'static> =
        ServerName::try_from("android.car").unwrap().to_owned();
    let mut cln = ClientConnection::new(Arc::clone(&client_cfg), name).unwrap();
    tls_tunnel::client_handshake(&mut client, &mut cln).await.unwrap();
    assert!(!cln.is_handshaking(), "client still handshaking");

    server.await.unwrap();
}
