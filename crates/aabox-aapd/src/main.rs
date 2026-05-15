//! Standalone CLI entry point for the AAP source daemon.
//! Used for desktop/Linux testing against DHU before deploying to Android.

use clap::Parser;

#[derive(Parser, Debug)]
#[command(name = "aabox-aapd", version)]
struct Args {
    /// Path to the FunctionFS endpoint (Linux/Android USB gadget).
    /// Ignored when running against DHU over TCP.
    #[arg(long, default_value = "/dev/usb-ffs/aoa")]
    usb: String,

    /// Connect to a Desktop Head Unit over TCP instead of USB.
    /// DHU listens on 5277 by default.
    #[arg(long)]
    dhu: Option<String>,
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
    tracing::info!(?args, "args");

    // TODO Phase 2: bring up USB gadget, AOAv2 handshake.
    // TODO Phase 3: SSL handshake using aasdk's headunit cert, Service Discovery,
    //               Channel Open, sensor bootstrap, video channel.

    Ok(())
}
