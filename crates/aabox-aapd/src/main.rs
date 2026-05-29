//! Standalone CLI entry point for the AAP source daemon.

use clap::{Parser, Subcommand};
use std::path::PathBuf;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::Layer;

#[derive(Parser, Debug)]
#[command(name = "aabox-aapd", version)]
struct Args {
    /// Path to also write logs to (in addition to stdout). Disabled if empty.
    #[arg(long, default_value = "/data/local/tmp/aabox-aapd.log")]
    log_file: String,

    /// Path to a single-line status file the daemon overwrites at each
    /// protocol stage. Useful for "is it working" polling from the user side.
    /// Disabled if empty.
    #[arg(long, default_value = "/data/local/tmp/aabox-aapd.status")]
    status_file: String,

    /// Initial navigation destination as `lat,lon[,label]`. When set, the
    /// daemon installs the OSRM-backed nav source and routes from the first
    /// GPS fix the car emits to this point. Without this flag (and without
    /// `--nav-destination-file`) the daemon falls back to the static-demo
    /// "Continue straight" loop so the head unit still sees nav traffic.
    #[arg(long)]
    nav_destination: Option<String>,

    /// Path to a JSON file the daemon polls for navigation destinations.
    /// Format: `{"lat":<f>, "lon":<f>, "label":<string>}`. Whenever the
    /// file's mtime changes, the daemon re-reads it and pushes a new
    /// destination through the nav source (triggering a re-route). Empty
    /// disables.
    #[arg(long, default_value = "/data/local/tmp/aabox-nav-destination.json")]
    nav_destination_file: String,

    /// OSRM HTTP base URL. Defaults to the public demo server; self-hosted
    /// deployments point this at e.g. http://10.0.0.5:5000.
    #[arg(long, default_value = aabox_aapd::nav::osrm::DEFAULT_OSRM_BASE_URL)]
    osrm_base_url: String,

    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Open /dev/usb_accessory, wait for host AOAv2 handshake, then drive AAP
    /// over the resulting stream. Captures every frame to the log. This is
    /// the in-car test entry point.
    UsbRun,

    /// Just open /dev/usb_accessory and exit (Phase 2 smoke test).
    UsbBringup,

    /// Listen for an incoming DHU connection over TCP, run AAP source role.
    DhuListen {
        #[arg(default_value = "127.0.0.1:5277")]
        bind: String,
    },
}

fn main() -> anyhow::Result<()> {
    // Stash guards so the non-blocking file writer flushes on shutdown.
    // We must keep these alive for the lifetime of the process.
    let args = Args::parse();
    let _guards = install_tracing(&args.log_file);

    // Panic hook: route every panic through tracing so it lands in the
    // log file. Daemon stdout/stderr go to /dev/null under init, so without
    // this any panic message vanishes.
    std::panic::set_hook(Box::new(|info| {
        let location = info.location().map(|l| format!("{}:{}", l.file(), l.line()))
            .unwrap_or_else(|| "<unknown>".to_string());
        let payload = if let Some(s) = info.payload().downcast_ref::<&str>() {
            (*s).to_string()
        } else if let Some(s) = info.payload().downcast_ref::<String>() {
            s.clone()
        } else {
            "<non-string payload>".to_string()
        };
        tracing::error!(location = %location, panic = %payload, "PANIC");
    }));

    tracing::info!(version = aabox_aapd::version(), "aabox-aapd starting");

    // Force system clock into the live Carlinkit Client.crt validity window.
    // Current embedded cert is the qcache-pulled one from a live Carlinkit
    // UHD (the cert it actually uses at runtime, not the expired one in
    // /system/etc/):
    //   NotBefore  2014-07-04 00:00:00 UTC
    //   NotAfter   2026-08-05 16:47:49 UTC
    // Today (May 2026) is comfortably inside this window so this is now a
    // sanity-check belt-and-suspenders against the RTC default of
    // 2021-01-01 on Rock 5B+ (no RTC battery).
    {
        use std::time::{SystemTime, UNIX_EPOCH};
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        // 2026-06-01 12:00:00 UTC = 1780315200 — safely inside the window.
        const TARGET_EPOCH: u64 = 1780315200;
        const WINDOW_START: u64 = 1404432000; // 2014-07-04 00:00:00 UTC
        const WINDOW_END: u64 = 1785948469;   // 2026-08-05 16:47:49 UTC
        if now < WINDOW_START || now > WINDOW_END {
            let tv = libc::timeval {
                tv_sec: TARGET_EPOCH as libc::time_t,
                tv_usec: 0,
            };
            // SAFETY: settimeofday takes a valid timeval and null tz pointer.
            let rc = unsafe { libc::settimeofday(&tv, std::ptr::null()) };
            if rc == 0 {
                tracing::info!(epoch = TARGET_EPOCH, was = now, "system clock forced into Client.crt validity window");
            } else {
                let e = std::io::Error::last_os_error();
                tracing::warn!(err = %e, was = now, "settimeofday failed — KIA will likely reject cert as expired");
            }
        }
    }

    let status = Status::new(&args.status_file);
    status.set("starting");

    let nav = NavOptions {
        destination: args.nav_destination.clone(),
        destination_file: args.nav_destination_file.clone(),
        osrm_base_url: args.osrm_base_url.clone(),
    };

    match args.cmd {
        Some(Cmd::UsbRun) => usb_run(status, nav),
        Some(Cmd::UsbBringup) => usb_bringup(status),
        Some(Cmd::DhuListen { bind }) => dhu_listen(bind, status, nav),
        None => {
            eprintln!("usage: aabox-aapd <usb-run | usb-bringup | dhu-listen [bind]>");
            std::process::exit(2);
        }
    }
}

/// CLI-derived nav options. Kept as a struct so `usb_run` / `dhu_listen`
/// only have one extra parameter and the wiring to `ControlLoopConfig` is
/// centralised in [`build_nav_config`].
#[derive(Clone, Debug)]
struct NavOptions {
    destination: Option<String>,
    destination_file: String,
    osrm_base_url: String,
}

/// Translate CLI nav options into the three `ControlLoopConfig` fields that
/// drive the nav engine. Returns:
///
///   - the boxed `NavInstructionSource` to install (OSRM if anything is
///     configured, static-demo as the explicit no-router fallback),
///   - the `Destination` to push at startup (from `--nav-destination`),
///   - the `Receiver<Destination>` for the file watcher (from
///     `--nav-destination-file`).
///
/// When both `destination` and `destination_file` are unset, returns
/// `(None, None, None)` and the control loop falls back to the legacy demo
/// emitter. That preserves the no-config dev workflow.
fn build_nav_config(
    nav: &NavOptions,
) -> (
    Option<Box<dyn aabox_aapd::nav::source::NavInstructionSource + Send>>,
    Option<aabox_aapd::nav::source::Destination>,
    Option<tokio::sync::mpsc::Receiver<aabox_aapd::nav::source::Destination>>,
) {
    use aabox_aapd::nav::destination_file::{watch_destination_file, DestinationFile};
    use aabox_aapd::nav::{OsrmClient, OsrmNavSource};

    let cli_dest = nav.destination.as_ref().and_then(|s| {
        match DestinationFile::parse_cli(s) {
            Ok(df) => Some(df.into_destination()),
            Err(e) => {
                tracing::error!(value = %s, "invalid --nav-destination: {e:#}");
                None
            }
        }
    });

    // Watcher only fires up when a path is configured (default is the
    // userdebug scratch path, so this is virtually always on).
    let (dest_rx, _watcher_handle) = if !nav.destination_file.is_empty() {
        let (tx, rx) = tokio::sync::mpsc::channel::<aabox_aapd::nav::source::Destination>(4);
        let h = watch_destination_file(
            &nav.destination_file,
            std::time::Duration::from_secs(1),
            tx,
        );
        (Some(rx), Some(h))
    } else {
        (None, None)
    };

    // Spin up OSRM if EITHER a CLI destination is supplied OR a watcher is
    // active (since the watcher might emit later). Otherwise fall back to
    // the static demo.
    //
    // The watcher's JoinHandle is dropped here — tokio detaches the task so
    // it keeps running until the process exits. The runtime is
    // process-scoped (block_on in `usb_run` / `dhu_listen`) so this is fine.
    drop(_watcher_handle);
    if cli_dest.is_some() || dest_rx.is_some() {
        let client = OsrmClient::new().with_base_url(nav.osrm_base_url.clone());
        let src = OsrmNavSource::new(client);
        (Some(Box::new(src)), cli_dest, dest_rx)
    } else {
        tracing::info!(
            "nav: no --nav-destination and no --nav-destination-file; nav channel falls back to demo"
        );
        (None, None, None)
    }
}

/// Configure tracing with stdout + file output. Returns worker guards that
/// must live for the lifetime of the process so the non-blocking file writer
/// flushes on Drop.
fn install_tracing(log_file: &str) -> Vec<tracing_appender::non_blocking::WorkerGuard> {
    use tracing_subscriber::{fmt, EnvFilter};

    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let stdout_layer = fmt::layer().with_writer(std::io::stdout);

    let mut guards = Vec::new();
    let registry = tracing_subscriber::registry().with(env_filter);

    if log_file.is_empty() {
        registry.with(stdout_layer).init();
        return guards;
    }

    let path = PathBuf::from(log_file);
    let dir = path.parent().unwrap_or_else(|| std::path::Path::new("."));
    let file_stem = path.file_name().unwrap_or_else(|| std::ffi::OsStr::new("aabox-aapd.log"));
    // Synchronous file appender: tracing_appender::non_blocking buffers in a
    // background thread, so when the process dies suddenly (SIGKILL, USB
    // disconnect → kernel teardown, init service restart), the buffered tail
    // is lost. We need every byte on disk so we can see the failure mode.
    let appender = tracing_appender::rolling::never(dir, file_stem);
    let file_layer = fmt::layer().with_ansi(false).with_writer(appender).boxed();

    // Mirror INFO+ events to /dev/kmsg so dmesg + UART show daemon state.
    // No-op if /dev/kmsg can't be opened (host build, lack of permission).
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let kmsg_layer = aabox_aapd::kmsg::KmsgLayer::try_new().map(|l| l.boxed());
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    let kmsg_layer: Option<Box<dyn Layer<_> + Send + Sync>> = None;

    registry
        .with(stdout_layer)
        .with(file_layer)
        .with(kmsg_layer)
        .init();
    guards
}

/// Tiny single-line status file abstraction. Best-effort writes — never panics.
struct Status {
    path: Option<PathBuf>,
}
impl Status {
    fn new(p: &str) -> Self {
        Self {
            path: if p.is_empty() { None } else { Some(PathBuf::from(p)) },
        }
    }
    fn set(&self, s: &str) {
        tracing::info!(status = s, "[status]");
        if let Some(ref p) = self.path {
            let _ = std::fs::write(p, format!("{}\n", s));
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn usb_bringup(status: Status) -> anyhow::Result<()> {
    use aabox_aapd::usb;
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        status.set("usb-bringup: opening /dev/usb_accessory");
        let fd = usb::wait_for_accessory().await?;
        tracing::info!(?fd, "accessory device opened");
        status.set("usb-bringup: open OK; exiting");
        Ok(())
    })
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn usb_run(status: Status, nav: NavOptions) -> anyhow::Result<()> {
    use aabox_aapd::{control, control_channel, tls, tls_tunnel, usb};

    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async move {
        status.set("usb-run: opening /dev/usb_accessory");
        let fd = usb::wait_for_accessory().await?;
        let mut stream = usb::stream::UsbAccessoryStream::new(fd)
            .map_err(|e| anyhow::anyhow!("UsbAccessoryStream::new: {e}"))?;
        tracing::info!("/dev/usb_accessory open — reads will block until host completes AOA");

        // Step 1: AAP version handshake — but DHU `-u` mode SKIPS this.
        //
        // Two observed protocol variants for the *first* control frame:
        //   - Carnival head unit / DHU adb-forward mode: VersionRequest
        //     (msg_id 0x0001). We send VersionResponse and continue.
        //   - DHU `-u` USB-direct mode: SslHandshake kickoff (msg_id 0x0003),
        //     skipping version negotiation entirely (uses announced 1.7).
        //
        // Detect by peeking the first frame. If it's VersionRequest, fall
        // through to the responder path. If it's SslHandshake, we already
        // consumed the kickoff — skip our own kickoff send.
        const MSG_VERSION_REQUEST: u16 = 0x0001;
        const MSG_SSL_HANDSHAKE: u16 = 0x0003;

        status.set("usb-run: peeking first control frame");
        let first = control::read_frame(&mut stream).await?;
        if first.payload.len() < 2 {
            anyhow::bail!("first frame payload too short ({} bytes)", first.payload.len());
        }
        let first_msg_id = u16::from_be_bytes([first.payload[0], first.payload[1]]);
        tracing::info!(first_msg_id = format_args!("0x{:04x}", first_msg_id), "first control frame received");

        let (peer_major, peer_minor) = if first_msg_id == MSG_VERSION_REQUEST {
            if first.payload.len() < 6 {
                anyhow::bail!("VersionRequest payload too short");
            }
            let peer_major = u16::from_be_bytes([first.payload[2], first.payload[3]]);
            let peer_minor = u16::from_be_bytes([first.payload[4], first.payload[5]]);
            tracing::info!(peer_major, peer_minor, "VersionRequest received");
            // Carlinkit (per disassembly of libSdAutoReverse.so
            // Controller::sendFakeVersionResponse) sends an 8-byte payload:
            //   msg_id=0x0002, major=1, minor=6, status=0x0000 (MATCH)
            // Total wire bytes: 00 03 00 08 00 02 00 01 00 06 00 00 (12 bytes).
            // We send the same — version 1.6 (NOT 1.7), with MATCH status.
            // The previous 1.7 + 6-byte form is what we'd been doing and it
            // didn't help; 1.7 + 8-byte form broke TLS; this is the
            // *Carlinkit-canonical* form.
            use tokio::io::AsyncWriteExt;
            let mut resp = [0u8; 12];
            resp[0] = 0; resp[1] = 0x03;            // channel + flags
            resp[2] = 0; resp[3] = 8;               // length = 8
            resp[4] = 0; resp[5] = 0x02;            // msg_id = VersionResponse
            resp[6] = 0; resp[7] = 1;               // major = 1
            resp[8] = 0; resp[9] = 6;               // minor = 6 (Carlinkit's value)
            resp[10] = 0; resp[11] = 0;             // status = 0x0000 (MATCH)
            stream.write_all(&resp).await?;
            tracing::info!(major = 1, minor = 6, peer_major, peer_minor, "VersionResponse sent (Carlinkit 8-byte form 1.6 + MATCH)");
            (peer_major, peer_minor)
        } else if first_msg_id == MSG_SSL_HANDSHAKE {
            tracing::info!("first frame is SslHandshake — DHU -u mode, skipping version handshake, assuming 1.7");
            (1u16, 7u16)
        } else {
            anyhow::bail!("unexpected first message id 0x{:04x}", first_msg_id);
        };
        status.set(&format!("usb-run: version OK ({}.{})", peer_major, peer_minor));

        // If the first frame was already an SslHandshake containing a real TLS
        // record (starts with 0x16 = handshake), DHU bundled its ClientHello
        // into the kickoff. Save those bytes so we can pre-seed rustls after
        // the ServerConnection is created — preventing a deadlock in
        // server_handshake which would otherwise wait for a ClientHello that
        // already arrived.
        let tls_preseed: Option<Vec<u8>> = if first_msg_id == MSG_SSL_HANDSHAKE {
            let body = &first.payload[2..];
            if body.first().copied() == Some(0x16) {
                tracing::info!(bytes = body.len(), "SslHandshake kickoff contains TLS bytes; will pre-seed rustls");
                Some(body.to_vec())
            } else {
                None
            }
        } else {
            None
        };

        // Step 2: SslHandshake kickoff probe — DHU PATH ONLY.
        //
        // DHU `-u` sends its own kickoff first (4-byte body `00 06 00 01`,
        // msg_id 0x0003) but then waits for OUR kickoff back before sending
        // the TLS ClientHello. Without this echo, DHU hangs.
        //
        // Real HUs (KIA, etc.) initiate AAP with VersionRequest and then send
        // their TLS ClientHello unprompted as the first SslHandshake frame.
        // Sending an unsolicited empty SslHandshake to a real HU is undefined
        // and per aasdk/AAServer reference code, the source never emits one.
        // So only send the kickoff when we're clearly in DHU mode (first frame
        // was already an SslHandshake).
        if first_msg_id == MSG_SSL_HANDSHAKE {
            let probe = control::ssl_handshake_frame(b"");
            tracing::info!("DHU mode: sending SslHandshake kickoff probe");
            control::write_frame(&mut stream, &probe).await?;
        } else {
            tracing::info!("KIA/real-HU path: skipping SslHandshake kickoff (HU will send ClientHello unprompted)");
        }

        let cfg = tls::build_server_config()?;
        let mut conn = rustls::ServerConnection::new(std::sync::Arc::clone(&cfg))?;

        // Pre-seed rustls if DHU's kickoff frame already contained a ClientHello.
        if let Some(ref preseed) = tls_preseed {
            conn.read_tls(&mut std::io::Cursor::new(preseed.as_slice()))
                .map_err(|e| anyhow::anyhow!("pre-seed read_tls: {e}"))?;
            conn.process_new_packets()
                .map_err(|e| anyhow::anyhow!("pre-seed process_new_packets: {e}"))?;
            tracing::info!("rustls pre-seeded with ClientHello from SslHandshake kickoff frame");
        }

        status.set("usb-run: starting TLS handshake");
        tracing::info!("starting TLS handshake over AAP (we are TLS server, mTLS)");
        let handshake_result = tokio::time::timeout(
            std::time::Duration::from_secs(15),
            tls_tunnel::server_handshake(&mut stream, &mut conn),
        )
        .await;
        match handshake_result {
            Ok(Ok(())) => {
                tracing::info!(
                    negotiated = ?conn.protocol_version(),
                    cipher = ?conn.negotiated_cipher_suite().map(|c| c.suite()),
                    "TLS handshake complete"
                );
                status.set("usb-run: TLS OK");
            }
            Ok(Err(e)) => {
                tracing::error!("TLS handshake failed: {e:#}");
                status.set(&format!("usb-run: TLS failed: {}", e));
                return Ok(());
            }
            Err(_) => {
                tracing::error!("TLS handshake timed out after 15s");
                status.set("usb-run: TLS timed out");
                return Ok(());
            }
        }

        // Step 3: post-TLS control-channel message loop. This is what makes
        // the car *progress* past discovery into actual projection.
        status.set("usb-run: control loop running");
        let (nav_source, initial_dest, dest_rx) = build_nav_config(&nav);
        let mut loop_cfg = control_channel::ControlLoopConfig::default();
        if nav_source.is_some() {
            loop_cfg.demo_nav = false;
        }
        loop_cfg.nav_source = nav_source;
        loop_cfg.initial_destination = initial_dest;
        loop_cfg.destination_rx = dest_rx;
        if let Err(e) = control_channel::run(&mut stream, &mut conn, loop_cfg).await {
            tracing::error!("control loop ended with error: {e:#}");
            status.set(&format!("usb-run: control loop error: {}", e));
        } else {
            status.set("usb-run: control loop ended cleanly");
        }
        Ok(())
    })
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn usb_bringup(_status: Status) -> anyhow::Result<()> {
    anyhow::bail!("usb-bringup is only supported on Linux/Android targets")
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn usb_run(_status: Status, _nav: NavOptions) -> anyhow::Result<()> {
    anyhow::bail!("usb-run is only supported on Linux/Android targets")
}

fn dhu_listen(bind: String, status: Status, nav: NavOptions) -> anyhow::Result<()> {
    use aabox_aapd::{control, control_channel, tls, tls_tunnel};
    use tokio::net::TcpListener;

    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        status.set("dhu-listen: binding");
        tracing::info!(%bind, "listening for incoming DHU connection (source role)");
        let listener = TcpListener::bind(&bind).await?;
        let (mut stream, peer) = listener.accept().await?;
        tracing::info!(?peer, "DHU connected");
        status.set("dhu-listen: DHU connected");

        let (peer_major, peer_minor) = control::version_handshake_responder(&mut stream).await?;
        tracing::info!(peer_major, peer_minor, "version handshake complete");
        status.set("dhu-listen: version OK");

        let probe = control::ssl_handshake_frame(b"");
        control::write_frame(&mut stream, &probe).await?;

        let cfg = tls::build_server_config()?;
        let mut conn = rustls::ServerConnection::new(std::sync::Arc::clone(&cfg))?;
        let handshake_result = tokio::time::timeout(
            std::time::Duration::from_secs(15),
            tls_tunnel::server_handshake(&mut stream, &mut conn),
        )
        .await;
        match handshake_result {
            Ok(Ok(())) => {
                tracing::info!(
                    negotiated = ?conn.protocol_version(),
                    cipher = ?conn.negotiated_cipher_suite().map(|c| c.suite()),
                    "TLS handshake complete"
                );
                status.set("dhu-listen: TLS OK");
            }
            Ok(Err(e)) => {
                tracing::error!("TLS handshake failed: {e:#}");
                status.set(&format!("dhu-listen: TLS failed: {}", e));
                return Ok(());
            }
            Err(_) => {
                tracing::error!("TLS handshake timed out after 15s");
                status.set("dhu-listen: TLS timed out");
                return Ok(());
            }
        }

        status.set("dhu-listen: control loop running");
        let (nav_source, initial_dest, dest_rx) = build_nav_config(&nav);
        let mut loop_cfg = control_channel::ControlLoopConfig::default();
        if nav_source.is_some() {
            loop_cfg.demo_nav = false;
        }
        loop_cfg.nav_source = nav_source;
        loop_cfg.initial_destination = initial_dest;
        loop_cfg.destination_rx = dest_rx;
        if let Err(e) = control_channel::run(&mut stream, &mut conn, loop_cfg).await {
            tracing::error!("control loop ended with error: {e:#}");
            status.set(&format!("dhu-listen: control loop error: {}", e));
        } else {
            status.set("dhu-listen: control loop ended cleanly");
        }
        Ok(())
    })
}
