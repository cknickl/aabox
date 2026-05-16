//! Standalone CLI entry point for the AAP source daemon.
//! Used for desktop/Linux testing against DHU before deploying to Android.

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

    /// Connect to a Desktop Head Unit over TCP. DHU listens on 5277 by default.
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
        Some(Cmd::Dhu { addr }) => {
            tracing::info!(%addr, "DHU mode — TODO: connect (Phase 3)");
            Ok(())
        }
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
