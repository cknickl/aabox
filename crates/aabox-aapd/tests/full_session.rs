//! Comprehensive end-to-end session test.
//!
//! This is the integration test that unblocks our DHU-less CI loop. It
//! stands up a daemon-side control loop on a TCP socket (the same code path
//! the `dhu-listen` CLI subcommand uses) and drives every channel the
//! daemon advertises with a `FakeHeadUnit` — see
//! `tests/common/fake_head_unit.rs` for the design rationale.
//!
//! What this test covers, in order:
//!
//!   1.  AAP version handshake.
//!   2.  TLS-over-AAP handshake (the daemon presents the embedded JVC
//!       Kenwood cert; the fake HU presents the same cert as client cert).
//!   3.  Plaintext `AuthComplete{OK}` from daemon.
//!   4.  `ServiceDiscoveryRequest` -> response with all 7 advertised channels.
//!   5.  `ChannelOpenRequest` on each non-control channel from the SDR ->
//!       `ChannelOpenResponse{OK}`.
//!   6.  `PingRequest{ts}` -> `PingResponse{ts}` (echo).
//!   7.  `NavigationFocusRequest` -> `NavigationFocusResponse{NATIVE=1}`.
//!   8.  `AudioFocusRequest` -> `AudioFocusResponse{GAIN}`.
//!   9.  Sensor: `SENSOR_START_REQUEST{LOCATION}` -> response; then a fake
//!       `SENSOR_EVENT_INDICATION` carrying a `GPSLocation` -> daemon writes
//!       a single NDJSON line to its GPS log (asserted).
//!  10.  Input: `INPUT_EVENT_INDICATION` carrying a `TouchEvent` (no reply
//!       expected; we just don't crash).
//!  11.  Each AV channel: `SETUP_REQUEST` -> `SETUP_RESPONSE{OK}`.
//!  12.  AV input channels (speech / system audio): `AV_INPUT_OPEN_REQUEST`
//!       -> `AV_INPUT_OPEN_RESPONSE`.
//!  13.  Video: `START_INDICATION` (no reply asserted — the encoder isn't
//!       wired up yet; we tolerate silence per the project plan).
//!  14.  `ShutdownRequest` -> `ShutdownResponse`.
//!
//! The whole run takes ~1 second on a modest dev VM. Hard timeout is 30s
//! to keep CI honest if anything starts hanging.

mod common;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use aabox_aapd::control as aap_control;
use aabox_aapd::control_channel::{self, ControlLoopConfig};
use aabox_aapd::{tls, tls_tunnel};
use aabox_common::ChannelId;
use aabox_proto::enums::{audio_focus_state, sensor_type, status};
use common::fake_head_unit::{FakeHeadUnit, Gps};
use rustls::ServerConnection;
use tokio::net::TcpListener;

/// Run the daemon-side AAP stack against a TcpStream — mirrors what the
/// `aabox-aapd dhu-listen` binary does, minus the CLI parsing and tracing
/// setup. Pulls the GPS log path out of the test's tempdir so the test
/// doesn't try to write to `/data/local/tmp` (which doesn't exist on the
/// build VM).
async fn run_daemon_side(
    sock: tokio::net::TcpStream,
    gps_log_path: PathBuf,
) -> anyhow::Result<()> {
    let mut sock = sock;

    // (1) AAP version handshake, responder role.
    aap_control::version_handshake_responder(&mut sock).await?;

    // (2) Send empty SslHandshake kickoff probe (DHU expects this).
    let probe = aap_control::ssl_handshake_frame(b"");
    aap_control::write_frame(&mut sock, &probe).await?;

    // (3) TLS handshake (we are the TLS server).
    let cfg = tls::build_server_config()?;
    let mut conn = ServerConnection::new(Arc::clone(&cfg))?;
    tls_tunnel::server_handshake(&mut sock, &mut conn).await?;

    // (4) Run the post-handshake control loop. Override the GPS log path,
    // disable the periodic demo nav emitter (so it doesn't race with our
    // assertions on the wire), and otherwise use defaults.
    let mut cfg = ControlLoopConfig::default();
    cfg.gps_log_path = gps_log_path;
    cfg.demo_nav = false;
    control_channel::run(&mut sock, &mut conn, cfg).await?;
    Ok(())
}

#[tokio::test]
async fn end_to_end_full_session() {
    // Hard wall-clock bound so a regression can't hang the test runner.
    tokio::time::timeout(
        Duration::from_secs(30),
        end_to_end_full_session_inner(),
    )
    .await
    .expect("full session must complete in <30s");
}

async fn end_to_end_full_session_inner() {
    // Per-test scratch for the GPS log. Use the pid in case multiple test
    // runs share /tmp on the same box.
    let gps_log_path = std::env::temp_dir().join(format!(
        "aabox-fullsession-gps-{}.log",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&gps_log_path);

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let local_addr = listener.local_addr().expect("local_addr");

    // Daemon task: accept one connection, run the full source-role flow.
    let gps_log_for_daemon = gps_log_path.clone();
    let daemon = tokio::spawn(async move {
        let (sock, _peer) = listener.accept().await.expect("accept");
        // Don't unwrap — when the client cleanly shuts down at the end of
        // the test, control_channel::run returns Ok(()), but on stream
        // close it also returns Ok(()). Any other error is a real fail.
        run_daemon_side(sock, gps_log_for_daemon).await.expect("daemon side");
    });

    // ------ Client side (the FakeHeadUnit) --------------------------------
    let mut hu = FakeHeadUnit::connect(local_addr).await.expect("connect");

    // (3) Plaintext AuthComplete should be the daemon's first post-TLS frame.
    hu.expect_auth_complete().await.expect("AuthComplete");

    // (4) ServiceDiscovery: expect at least the 7 advertised channels.
    let sdr = hu.service_discovery().await.expect("service_discovery");
    assert_eq!(
        sdr.head_unit_name, "AABox",
        "daemon should self-identify as AABox"
    );
    assert!(
        sdr.channels.len() >= 7,
        "expected at least 7 channels, got {}: {:?}",
        sdr.channels.len(),
        sdr.channels.iter().map(|c| c.channel_id).collect::<Vec<_>>()
    );

    // (5) Open every non-control channel the daemon advertises.
    for cd in &sdr.channels {
        let cid = cd.channel_id as i32;
        assert_ne!(
            cid, ChannelId::Control as i32,
            "control channel must not appear in the SDR channel list"
        );
        hu.channel_open(cid)
            .await
            .unwrap_or_else(|e| panic!("channel_open({cid}) failed: {e:#}"));
    }

    // (6) Ping echoes timestamp.
    let ts: i64 = 0x0011_2233_4455_6677;
    hu.ping(ts).await.expect("ping");

    // (7) NavigationFocusRequest -> NATIVE (=1).
    let nav_type = hu.navigation_focus().await.expect("navigation_focus");
    assert_eq!(nav_type, 1, "expected NATIVE nav focus response");

    // (8) AudioFocusRequest -> GAIN.
    let af = hu.audio_focus().await.expect("audio_focus");
    assert_eq!(
        af,
        audio_focus_state::Enum::Gain as i32,
        "expected GAIN audio focus state"
    );

    // (9) Sensor channel: start LOCATION, then push one GPS fix.
    let s = hu
        .sensor_start(sensor_type::Enum::Location as i32)
        .await
        .expect("sensor_start");
    assert_eq!(s, status::Enum::Ok as i32, "sensor start should be OK");

    let fix = Gps {
        timestamp: 1_747_440_000_000, // ms since epoch — arbitrary
        latitude: 388_976_750,        // 38.8976750° (E7)
        longitude: -770_365_000,      // -77.0365° (E7)
        accuracy: 5_000,
        altitude: 50_000,
        speed: 13_410,
        bearing: 90_000_000,
    };
    hu.send_gps_event(fix.clone()).await.expect("send_gps_event");

    // (10) Input touch event. No reply, so just emit and let the daemon
    // log it.
    hu.send_touch_event(640, 480).await.expect("send_touch_event");

    // (11) AV channel setups — media-audio, speech-in, system-audio-in, video.
    for chan in [
        ChannelId::MediaAudio,
        ChannelId::SpeechAudio,
        ChannelId::SystemAudio,
        ChannelId::Video,
    ] {
        hu.av_setup(chan)
            .await
            .unwrap_or_else(|e| panic!("av_setup({chan:?}) failed: {e:#}"));
    }

    // (12) AV input opens — mic channels.
    for chan in [ChannelId::SpeechAudio, ChannelId::SystemAudio] {
        hu.av_input_open(chan)
            .await
            .unwrap_or_else(|e| panic!("av_input_open({chan:?}) failed: {e:#}"));
    }

    // (13) Video START. No reply expected (the encoder isn't wired up yet).
    hu.av_start(ChannelId::Video).await.expect("video START");

    // Give the daemon a beat to flush the GPS log to disk before we read it.
    tokio::time::sleep(Duration::from_millis(50)).await;

    // (9 cont.) Assert the GPS NDJSON log line is present.
    let log_contents =
        std::fs::read_to_string(&gps_log_path).expect("GPS log should exist");
    assert!(
        log_contents.contains("\"lat\":38.8976750"),
        "GPS log missing lat: {log_contents}"
    );
    assert!(
        log_contents.contains("\"lon\":-77.0365000"),
        "GPS log missing lon: {log_contents}"
    );
    assert!(
        log_contents.ends_with('\n'),
        "GPS log should be newline-terminated"
    );

    // (14) Shutdown handshake.
    hu.shutdown().await.expect("shutdown");

    // Close the client side; the daemon's read_frame returns an error and
    // run() exits cleanly.
    hu.close().await.expect("close");

    // Wait for the daemon task to wind down. With the client gone its
    // read_frame errors out and control_channel::run returns Ok(()).
    let _ = tokio::time::timeout(Duration::from_secs(5), daemon).await;

    // Cleanup
    let _ = std::fs::remove_file(&gps_log_path);
}
