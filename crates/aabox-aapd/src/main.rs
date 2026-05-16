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

    tracing::info!(version = aabox_aapd::version(), "aabox-aapd starting");

    let status = Status::new(&args.status_file);
    status.set("starting");

    match args.cmd {
        Some(Cmd::UsbRun) => usb_run(status),
        Some(Cmd::UsbBringup) => usb_bringup(status),
        Some(Cmd::DhuListen { bind }) => dhu_listen(bind, status),
        None => {
            eprintln!("usage: aabox-aapd <usb-run | usb-bringup | dhu-listen [bind]>");
            std::process::exit(2);
        }
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
    let appender = tracing_appender::rolling::never(dir, file_stem);
    let (nb, guard) = tracing_appender::non_blocking(appender);
    guards.push(guard);
    let file_layer = fmt::layer().with_ansi(false).with_writer(nb).boxed();

    registry.with(stdout_layer).with(file_layer).init();
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
fn usb_run(status: Status) -> anyhow::Result<()> {
    use aabox_aapd::{control, tls, tls_tunnel, usb};
    use tokio::fs::File;

    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async move {
        status.set("usb-run: opening /dev/usb_accessory");
        let fd = usb::wait_for_accessory().await?;
        tracing::info!(?fd, "accessory device opened");

        // Wrap the kernel fd in tokio::fs::File for AsyncRead+AsyncWrite. Tokio
        // uses spawn_blocking internally for char-device I/O which is fine for
        // the slow USB 2.0 wire we're on.
        let std_file: std::fs::File = std::fs::File::from(fd);
        let mut stream = File::from_std(std_file);

        // Step 1: AAP version handshake. Per the DHU evidence, the *peer*
        // (head unit) sends VersionRequest first. We respond.
        status.set("usb-run: waiting for VersionRequest");
        let (peer_major, peer_minor) =
            control::version_handshake_responder(&mut stream).await?;
        tracing::info!(peer_major, peer_minor, "version handshake complete");
        status.set(&format!("usb-run: version OK ({}.{})", peer_major, peer_minor));

        // Step 2: SslHandshake kickoff probe, then TLS server handshake.
        let probe = control::ssl_handshake_frame(b"");
        tracing::info!("sending SslHandshake kickoff probe");
        control::write_frame(&mut stream, &probe).await?;

        let cfg = tls::build_server_config()?;
        let mut conn = rustls::ServerConnection::new(std::sync::Arc::clone(&cfg))?;
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
            }
            Err(_) => {
                tracing::error!("TLS handshake timed out after 15s");
                status.set("usb-run: TLS timed out");
            }
        }

        // Step 3: Whatever DHU/car-mode says next, log it. We don't need to
        // respond intelligently yet — Phase 4+ will. Capture for analysis.
        status.set("usb-run: post-TLS capture (60s budget)");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        let mut frame_count = 0u64;
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                tracing::info!(frame_count, "post-TLS capture window elapsed");
                break;
            }
            match tokio::time::timeout(remaining, control::read_frame(&mut stream)).await {
                Ok(Ok(f)) => {
                    frame_count += 1;
                    tracing::info!(
                        seq = frame_count,
                        channel = f.channel_id,
                        encrypted = f.encrypted,
                        payload_len = f.payload.len(),
                        "frame from car"
                    );
                }
                Ok(Err(e)) => {
                    tracing::info!("car stream ended: {e:#}");
                    break;
                }
                Err(_) => break,
            }
        }
        status.set(&format!("usb-run: finished, captured {} frames", frame_count));
        Ok(())
    })
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn usb_bringup(_status: Status) -> anyhow::Result<()> {
    anyhow::bail!("usb-bringup is only supported on Linux/Android targets")
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn usb_run(_status: Status) -> anyhow::Result<()> {
    anyhow::bail!("usb-run is only supported on Linux/Android targets")
}

fn dhu_listen(bind: String, status: Status) -> anyhow::Result<()> {
    use aabox_aapd::{control, tls, tls_tunnel};
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
            }
            Err(_) => {
                tracing::error!("TLS handshake timed out after 15s");
                status.set("dhu-listen: TLS timed out");
            }
        }

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            match tokio::time::timeout(remaining, control::read_frame(&mut stream)).await {
                Ok(Ok(f)) => tracing::info!(
                    channel = f.channel_id,
                    encrypted = f.encrypted,
                    payload_len = f.payload.len(),
                    "post-TLS frame from DHU"
                ),
                _ => break,
            }
        }
        Ok(())
    })
}
