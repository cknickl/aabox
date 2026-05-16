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
    use aabox_aapd::{control, services, tls};
    use tokio::net::TcpListener;

    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        tracing::info!(%bind, "listening for incoming DHU connection (source role)");
        let listener = TcpListener::bind(&bind).await?;
        let (mut stream, peer) = listener.accept().await?;
        tracing::info!(?peer, "DHU connected");

        // AAP role direction: the SOURCE (us) initiates the version exchange.
        // DHU (head-unit role) replies. We use _initiator on the daemon side.
        let (peer_major, peer_minor, status) = control::version_handshake_initiator(&mut stream).await?;
        tracing::info!(peer_major, peer_minor, status, "version handshake complete");

        // Smoke: build TLS config (proves cert load) — full TLS handshake
        // over the AAP tunnel is the next Phase 3 deliverable.
        let _tls_cfg = tls::build_client_config()?;
        tracing::info!("rustls client config built (TLS-over-AAP next)");

        // Smoke: pre-encode the SDR so the next iteration just sends it.
        let resp = services::minimal_response();
        let bytes = services::encode_response(&resp);
        tracing::info!(bytes = bytes.len(), "ServiceDiscoveryResponse pre-encoded");

        // Keep the socket open briefly so DHU sees we're alive; in practice
        // the next chunk of code (TLS handshake driver) will start here.
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        Ok(())
    })
}
