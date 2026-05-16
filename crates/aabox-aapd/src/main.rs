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

    /// Connect to a Desktop Head Unit over TCP and run the AAP handshake.
    /// DHU listens on 5277 by default.
    Dhu {
        #[arg(default_value = "127.0.0.1:5277")]
        addr: String,
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
        Some(Cmd::Dhu { addr }) => dhu_connect(addr),
        None => {
            eprintln!("usage: aabox-aapd <usb-bringup|dhu [addr]>");
            std::process::exit(2);
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn usb_bringup() -> anyhow::Result<()> {
    use aabox_aapd::usb;
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        let mut state = usb::gadget::UsbGadgetState::autodetect()?;
        usb::bring_up(&mut state).await?;
        let fd = usb::stream::open()?;
        tracing::info!(?fd, "accessory device opened — ready for AAP framing (Phase 3)");
        Ok(())
    })
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn usb_bringup() -> anyhow::Result<()> {
    anyhow::bail!("usb-bringup is only supported on Linux/Android targets")
}

fn dhu_connect(addr: String) -> anyhow::Result<()> {
    use aabox_aapd::{control, services, tls};
    use tokio::net::TcpStream;

    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        tracing::info!(%addr, "connecting to DHU");
        let mut stream = TcpStream::connect(&addr).await?;
        tracing::info!("connected; running version handshake");

        let (major, minor, status) = control::version_handshake(&mut stream).await?;
        tracing::info!(major, minor, status, "version negotiated");

        // Smoke: build the rustls client config (proves cert+key load). Wiring
        // the actual TLS tunnel over AAP frames is the next slab of Phase 3
        // work — see docs/phase-3-aap.md.
        let _tls_cfg = tls::build_client_config()?;
        tracing::info!("rustls client config built (TLS handshake tunnel pending)");

        // Smoke: build the ServiceDiscoveryResponse payload (proves protobuf
        // schema lines up).
        let resp = services::minimal_response();
        let bytes = services::encode_response(&resp);
        tracing::info!(bytes = bytes.len(), "ServiceDiscoveryResponse encoded");

        Ok(())
    })
}
