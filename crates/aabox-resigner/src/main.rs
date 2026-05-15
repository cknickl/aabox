//! Standalone CLI for the re-signer — useful for offline patching during dev.

use clap::Parser;

#[derive(Parser, Debug)]
#[command(name = "aabox-resigner", version)]
struct Args {
    /// Input APK to patch + re-sign.
    #[arg(long)]
    input: String,

    /// Output path for the patched APK.
    #[arg(long)]
    output: String,

    /// Path to AABox platform.pk8.
    #[arg(long, default_value = "/vendor/aabox-keys/platform.pk8")]
    key: String,

    /// Path to AABox platform.x509.pem.
    #[arg(long, default_value = "/vendor/aabox-keys/platform.x509.pem")]
    cert: String,
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();
    tracing::info!(?args, "aabox-resigner");

    // TODO: load APK, patch dex (clamp SDK_INT version gates), repack, sign.
    Ok(())
}
