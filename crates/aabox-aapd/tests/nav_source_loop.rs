//! End-to-end: the control-channel loop, configured with a `StaticDemoSource`
//! nav source, emits a navigation flight (NavigationStatus + TurnEvent +
//! DistanceEvent) on the wire whenever a destination is installed.
//!
//! Like the existing `control_channel_loop.rs` integration test we build a
//! real TLS-over-AAP tunnel inside the test process. The "head unit" side
//! reads frames as they come in; we verify that the three nav-channel
//! messages show up after we drive a destination through the control
//! loop's `destination_rx`.
//!
//! This is the test that proves the seam between sensor channel, nav source,
//! and control loop is wired correctly. The OSRM client itself is unit-tested
//! against wiremock in `src/nav/osrm.rs`.

use aabox_aapd::channels::nav::{
    MSG_NAVIGATION_DISTANCE_EVENT, MSG_NAVIGATION_STATUS, MSG_NAVIGATION_TURN_EVENT,
};
use aabox_aapd::control::read_frame;
use aabox_aapd::control_channel::{self, ControlLoopConfig};
use aabox_aapd::encrypted::decrypt_payload;
use aabox_aapd::nav::source::{Destination, StaticDemoSource};
use aabox_aapd::{tls, tls_tunnel};
use aabox_common::ChannelId;
use rustls::pki_types::ServerName;
use rustls::{ClientConnection, ServerConnection};
use std::sync::Arc;
use tokio::net::{TcpListener, TcpStream};

#[tokio::test]
async fn nav_source_emits_three_frame_flight_on_destination() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let local_addr = listener.local_addr().unwrap();

    let server_cfg = tls::build_server_config().expect("server cfg");
    let client_cfg = tls::build_client_config().expect("client cfg");

    // A destination channel we hand to the control loop. By pushing onto it
    // we trigger the source's set_destination -> Some(instruction) path,
    // which the runner pumps onto the wire.
    let (dest_tx, dest_rx) = tokio::sync::mpsc::channel::<Destination>(2);

    let server = tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut srv = ServerConnection::new(server_cfg).unwrap();
        tls_tunnel::server_handshake(&mut sock, &mut srv).await.unwrap();

        let mut cfg = ControlLoopConfig::default();
        cfg.gps_log_path = std::env::temp_dir()
            .join(format!("aabox-nav-loop-gps-{}.log", std::process::id()));
        // Real source path: disable the demo loop and install a deterministic
        // static source. The runner will react to dest_rx on the loop side.
        cfg.demo_nav = false;
        cfg.nav_source = Some(Box::new(StaticDemoSource::default()));
        cfg.destination_rx = Some(dest_rx);

        let _ = control_channel::run(&mut sock, &mut srv, cfg).await;
    });

    let mut client = TcpStream::connect(local_addr).await.unwrap();
    let name: ServerName<'static> =
        ServerName::try_from("android.car").unwrap().to_owned();
    let mut cln = ClientConnection::new(Arc::clone(&client_cfg), name).unwrap();
    tls_tunnel::client_handshake(&mut client, &mut cln).await.unwrap();

    // Discard AuthComplete (plaintext) — same shape as the existing test.
    let f = read_frame(&mut client).await.unwrap();
    assert_eq!(f.channel_id, ChannelId::Control as u8);

    // The runner is configured with `emit_initial = true`, so the source's
    // `current_instruction()` should land before our destination push. Then
    // pushing a destination should produce *another* flight.
    //
    // We collect nav-channel frames until we've seen the three message IDs
    // we expect (status, turn, distance) — at least one full flight.
    dest_tx
        .send(Destination::new(37.7749, -122.4194, "Apple HQ"))
        .await
        .unwrap();

    let mut saw_status = false;
    let mut saw_turn = false;
    let mut saw_distance = false;
    let mut tries = 0;
    while !(saw_status && saw_turn && saw_distance) && tries < 40 {
        tries += 1;
        let f = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            read_frame(&mut client),
        )
        .await
        .expect("read_frame timed out")
        .expect("read_frame error");
        if f.channel_id != ChannelId::Navigation as u8 {
            continue;
        }
        let plain = if f.encrypted {
            decrypt_payload(&mut cln, &f.payload).expect("decrypt")
        } else {
            f.payload.to_vec()
        };
        assert!(plain.len() >= 2, "nav frame plaintext too short");
        let mid = u16::from_be_bytes([plain[0], plain[1]]);
        match mid {
            MSG_NAVIGATION_STATUS => saw_status = true,
            MSG_NAVIGATION_TURN_EVENT => {
                // Turn event body is 4 bytes BE u32; static demo source uses
                // turn_id=1 (straight ahead). Verify.
                assert_eq!(
                    &plain[2..],
                    &[0, 0, 0, 1],
                    "expected static demo source turn_id=1"
                );
                saw_turn = true;
            }
            MSG_NAVIGATION_DISTANCE_EVENT => saw_distance = true,
            other => panic!(
                "unexpected nav msg id 0x{:04x} on nav channel",
                other
            ),
        }
    }
    assert!(
        saw_status && saw_turn && saw_distance,
        "expected full nav flight: status={} turn={} dist={} after {} frames",
        saw_status,
        saw_turn,
        saw_distance,
        tries
    );

    // Drop the client; the server loop ends.
    drop(client);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), server).await;
}
