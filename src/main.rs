//! copycat-trader — entry point.
//!
//! Wires together: config loader -> Geyser stream -> decoder pipeline ->
//! engine (copy logic) -> executor (Jupiter + Jito) -> sqlite + TUI.

use anyhow::{Context, Result};
use clap::Parser;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc};
use tracing::{error, info};

use copycat_trader::{config, db, engine, geyser, tui, types};

use crate::config::Config;

#[derive(Parser, Debug)]
#[command(name = "copycat", version, about)]
struct Cli {
    /// Path to config.toml
    #[arg(short, long, default_value = "config.toml", env = "COPYCAT_CONFIG")]
    config: PathBuf,

    /// Run without executing swaps (decode + log only)
    #[arg(long, env = "COPYCAT_DRY_RUN")]
    dry_run: bool,

    /// Disable TUI (headless operation for systemd/docker)
    #[arg(long, env = "COPYCAT_HEADLESS")]
    headless: bool,
}

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() -> Result<()> {
    // Log to stderr so stdout stays clean for TUI
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,copycat=debug")),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    let cfg = Config::load(&cli.config)
        .with_context(|| format!("loading config from {}", cli.config.display()))?;
    info!(wallets = cfg.wallets.len(), "config loaded");

    let cfg = Arc::new(cfg);

    // Shared shutdown signal
    let (shutdown_tx, _shutdown_rx) = broadcast::channel::<()>(8);
    let shutdown_handle = shutdown_tx.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            info!("SIGINT received — shutting down");
            let _ = shutdown_handle.send(());
        }
    });

    // DB
    let db = db::Db::open(&cfg.db.url, cfg.db.busy_timeout_ms).await?;
    db.migrate().await?;

    // Channels
    let (decoded_tx, decoded_rx) = mpsc::channel::<types::DecodedSwap>(1024);
    let (event_tx, event_rx) = mpsc::channel::<types::UiEvent>(1024);

    // Spawn Geyser + decoder pipeline
    let geyser_handle = {
        let cfg = cfg.clone();
        let shutdown_rx = shutdown_tx.subscribe();
        tokio::spawn(async move {
            if let Err(e) = geyser::run(cfg, decoded_tx, shutdown_rx).await {
                error!(error = %e, "geyser task exited");
            }
        })
    };

    // Spawn engine
    let engine_handle = {
        let cfg = cfg.clone();
        let db = db.clone();
        let event_tx = event_tx.clone();
        let shutdown_rx = shutdown_tx.subscribe();
        let dry_run = cli.dry_run;
        tokio::spawn(async move {
            if let Err(e) = engine::run(cfg, db, decoded_rx, event_tx, shutdown_rx, dry_run).await {
                error!(error = %e, "engine task exited");
            }
        })
    };

    // TUI (foreground)
    if cfg.tui.enabled && !cli.headless {
        tui::run(cfg.clone(), db.clone(), event_rx, cfg.tui.refresh_hz).await?;
        let _ = shutdown_tx.send(());
    } else {
        // Headless: drain events to a log and wait for shutdown
        let mut event_rx = event_rx;
        let mut shutdown_rx = shutdown_tx.subscribe();
        loop {
            tokio::select! {
                Some(ev) = event_rx.recv() => {
                    info!(?ev, "ui_event");
                }
                _ = shutdown_rx.recv() => break,
                else => break,
            }
        }
    }

    let _ = tokio::join!(geyser_handle, engine_handle);
    info!("shutdown complete");
    Ok(())
}
