//! Standalone CLI entry point for the AAP source daemon.

use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(name = "aabox-aapd", version)]
struct Args {
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Bring up the USB gadget and wait for AOAv2 handshake completion.
    /// Linux/Android only — needs root + ConfigFS + f_accessory kernel driver.
    UsbBringup,

    /// Listen for an incoming DHU (Desktop Head Unit) connection and play the
    /// AAP *source* role: read VersionRequest, reply with VersionResponse,
    /// build TLS config. Default bind 127.0.0.1:5277 to match DHU's expected
    /// "Head Unit Server" address.
    DhuListen {
        #[arg(default_value = "127.0.0.1:5277")]
        bind: String,
    },
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();
    tracing::info!(version = aabox_aapd::version(), "aabox-aapd starting");

    match args.cmd {
        Some(Cmd::UsbBringup) => usb_bringup(),
        Some(Cmd::DhuListen { bind }) => dhu_listen(bind),
        None => {
            eprintln!("usage: aabox-aapd <usb-bringup|dhu-listen [bind]>");
            std::process::exit(2);
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn usb_bringup() -> anyhow::Result<()> {
    use aabox_aapd::usb;
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        let fd = usb::wait_for_accessory().await?;
        tracing::info!(?fd, "accessory device opened — Phase 3 AAP framing comes next");
        // TODO Phase 3: wrap fd in an AsyncRead+AsyncWrite, run version_handshake_responder,
        // run TLS-over-AAP handshake, dispatch channels.
        Ok(())
    })
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn usb_bringup() -> anyhow::Result<()> {
    anyhow::bail!("usb-bringup is only supported on Linux/Android targets")
}

fn dhu_listen(bind: String) -> anyhow::Result<()> {
    use aabox_aapd::{control, tls, tls_tunnel};
    use rustls::ServerConnection;
    use std::sync::Arc;
    use tokio::net::TcpListener;

    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        tracing::info!(%bind, "listening for incoming DHU connection (source role)");
        let listener = TcpListener::bind(&bind).await?;
        let (mut stream, peer) = listener.accept().await?;
        tracing::info!(?peer, "DHU connected");

        // Step 1: plaintext version handshake. DHU sends VersionRequest, we
        // respond with VersionResponse (6-byte form, no CONTROL flag bit).
        let (peer_major, peer_minor) = control::version_handshake_responder(&mut stream).await?;
        tracing::info!(peer_major, peer_minor, "version handshake complete");

        // Step 2: TLS handshake. Empirically (per DHU's BoringSSL log
        // "TLS client read_server_hello"), the AAP head unit is the TLS
        // CLIENT, and the source (us) is the TLS SERVER. aasdk's Cryptor
        // calls setConnectState() — but aasdk targets the HU role, which
        // confirms HU = TLS client. We use ServerConnection here.
        let cfg = tls::build_server_config()?;
        let mut conn = ServerConnection::new(Arc::clone(&cfg))?;
        tracing::info!("starting TLS handshake over AAP (we are TLS server)");
        tls_tunnel::server_handshake(&mut stream, &mut conn).await?;
        tracing::info!(
            negotiated = ?conn.protocol_version(),
            cipher = ?conn.negotiated_cipher_suite().map(|c| c.suite()),
            "TLS handshake complete"
        );

        // Step 3 onwards (AuthComplete, ServiceDiscoveryRequest/Response,
        // ChannelOpenRequests, sensor bootstrap, video channel) — next slab
        // of work. For now keep reading frames and log them so we can see
        // what DHU sends post-TLS.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                tracing::info!("post-TLS capture window elapsed");
                break;
            }
            match tokio::time::timeout(remaining, control::read_frame(&mut stream)).await {
                Ok(Ok(f)) => tracing::info!(
                    channel = f.channel_id,
                    encrypted = f.encrypted,
                    payload_len = f.payload.len(),
                    "post-TLS frame from DHU"
                ),
                Ok(Err(e)) => {
                    tracing::info!("DHU stream ended: {e:#}");
                    break;
                }
                Err(_) => {
                    tracing::info!("capture window timed out");
                    break;
                }
            }
        }
        Ok(())
    })
}
