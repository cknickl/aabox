//! `aabox-resigner` CLI.
//!
//! Two modes:
//!
//! * **Offline / one-shot**:
//!     ```ignore
//!     aabox-resigner --input X.apk --output Y.apk \
//!         --key /vendor/aabox-keys/platform.pk8 \
//!         --cert /vendor/aabox-keys/platform.x509.pem
//!     ```
//! * **Daemon / on-device**:
//!     ```ignore
//!     aabox-resigner --watch
//!     ```
//!   Watches `/sdcard/Download` (override with `--dir`), processes any
//!   `carcar*.apk` that lands there, and `pm install`s the patched output.

use std::path::PathBuf;

use clap::Parser;

use aabox_resigner::{
    process_apk, version, watcher, DEFAULT_CERT_PATH, DEFAULT_KEY_PATH, DEFAULT_LOG_PATH,
    DEFAULT_WATCH_DIR,
};

#[derive(Parser, Debug)]
#[command(name = "aabox-resigner", version)]
struct Args {
    /// Daemon mode: watch a directory for new CarCar APKs and process them.
    #[arg(long, conflicts_with_all = ["input", "output"])]
    watch: bool,

    /// Directory to watch (daemon mode).
    #[arg(long, default_value = DEFAULT_WATCH_DIR)]
    dir: PathBuf,

    /// Log file (daemon mode). Pass "-" for stderr only.
    #[arg(long, default_value = DEFAULT_LOG_PATH)]
    log: String,

    /// Input APK to patch + re-sign (one-shot mode).
    #[arg(long, requires = "output")]
    input: Option<PathBuf>,

    /// Output path for the patched APK (one-shot mode).
    #[arg(long)]
    output: Option<PathBuf>,

    /// Path to AABox platform.pk8 (DER, PKCS#8).
    #[arg(long, default_value = DEFAULT_KEY_PATH)]
    key: PathBuf,

    /// Path to AABox platform.x509.pem.
    #[arg(long, default_value = DEFAULT_CERT_PATH)]
    cert: PathBuf,
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    init_logging(&args)?;
    tracing::info!(version = version(), "aabox-resigner starting");

    if args.watch {
        watcher::watch(&args.dir, &args.key, &args.cert)?;
        return Ok(());
    }

    match (args.input.as_deref(), args.output.as_deref()) {
        (Some(input), Some(output)) => {
            let report = process_apk(input, output, &args.key, &args.cert)?;
            tracing::info!(?report, "done");
            println!(
                "patched {} SDK_INT site(s) across {} dex file(s) in {} ms ({} -> {} bytes)",
                report.sdk_int_patches,
                report.dex_files_patched,
                report.elapsed_ms,
                report.input_bytes,
                report.output_bytes,
            );
            Ok(())
        }
        _ => {
            anyhow::bail!(
                "must provide either --watch or both --input and --output (see --help)"
            )
        }
    }
}

fn init_logging(args: &Args) -> anyhow::Result<()> {
    use tracing_subscriber::EnvFilter;
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    // In daemon mode, also tee a non-blocking log file unless caller asked
    // for stderr only via "-".
    if args.watch && args.log != "-" {
        // Attempt to open the log file. If we can't (read-only fs etc.), fall
        // back to stderr-only — don't fail the daemon on a missing log dir.
        match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&args.log)
        {
            Ok(file) => {
                let (nb, guard) = tracing_appender::non_blocking(file);
                // Leak the guard so the worker thread stays alive for the
                // lifetime of the process (daemon never returns from main).
                Box::leak(Box::new(guard));
                tracing_subscriber::fmt()
                    .with_env_filter(env_filter)
                    .with_writer(nb)
                    .with_ansi(false)
                    .init();
                return Ok(());
            }
            Err(e) => {
                eprintln!(
                    "[aabox-resigner] warning: could not open log {}: {}; logging to stderr",
                    args.log, e
                );
            }
        }
    }
    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_ansi(false)
        .init();
    Ok(())
}
