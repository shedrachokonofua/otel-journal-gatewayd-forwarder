//! otel-journal-gatewayd-forwarder
//!
//! Pull-based journal log forwarder. Collects logs from remote
//! systemd-journal-gatewayd endpoints and forwards them to an OTLP-compatible backend.

mod collector;
mod config;
mod cursor;
mod journal;
mod metrics;
mod otlp;

use clap::Parser;
use config::{Cli, Config};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

fn main() -> ExitCode {
    let cli = Cli::parse();

    // Setup logging
    setup_logging(&cli);

    // Load configuration
    let config = match Config::load(&cli.config) {
        Ok(config) => config,
        Err(e) => {
            error!(error = %e, "Failed to load configuration");
            return ExitCode::from(1);
        }
    };

    // Validate configuration
    if let Err(e) = config.validate() {
        error!(error = %e, "Configuration validation failed");
        return ExitCode::from(1);
    }

    // --validate mode: exit after validation
    if cli.validate {
        info!("Configuration is valid");
        println!("Configuration validated successfully:");
        println!("  OTLP endpoint: {}", config.otlp_endpoint);
        println!("  Poll interval: {:?}", config.poll_interval);
        println!("  Batch size: {}", config.batch_size);
        println!("  Cursor dir: {}", config.cursor_dir.display());
        println!("  Sources: {}", config.sources.len());
        for source in &config.sources {
            println!("    - {} ({})", source.name, source.url);
        }
        return ExitCode::SUCCESS;
    }

    // Run the forwarder
    if let Err(e) = run(config, &cli) {
        error!(error = %e, "Fatal error");
        return ExitCode::from(1);
    }

    ExitCode::SUCCESS
}

fn setup_logging(cli: &Cli) {
    let filter = if cli.quiet {
        "error"
    } else {
        match cli.verbose {
            0 => "warn,otel_journal_gatewayd_forwarder=info",
            1 => "info",
            2 => "debug",
            _ => "trace",
        }
    };

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(filter));

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();
}

fn run(config: Config, cli: &Cli) -> Result<(), Box<dyn std::error::Error>> {
    info!(
        otlp_endpoint = %config.otlp_endpoint,
        sources = config.sources.len(),
        "Starting forwarder"
    );

    // Shared shutdown flag
    let shutdown = Arc::new(AtomicBool::new(false));

    // Setup signal handlers
    let shutdown_clone = shutdown.clone();
    ctrlc_setup(shutdown_clone);

    // Setup metrics if enabled
    let metrics = if let Some(ref addr) = cli.metrics {
        let state = Arc::new(metrics::MetricsState::new());
        metrics::start_server(addr, state.clone())?;
        Some(state)
    } else {
        None
    };

    // Create shared OTLP client
    let otlp = Arc::new(otlp::OtlpClient::new(&config.otlp_endpoint)?);

    // Start collector threads
    let mut handles = Vec::new();

    for source in config.sources {
        let cursor = cursor::CursorManager::new(&config.cursor_dir, &source.name)?;
        let collector = collector::Collector::new(
            source,
            otlp.clone(),
            cursor,
            config.batch_size,
            metrics.clone(),
        )?;

        let shutdown = shutdown.clone();
        let poll_interval = config.poll_interval;
        let once = cli.once;

        let handle = thread::spawn(move || {
            collector::run_loop(collector, poll_interval, shutdown, once);
        });

        handles.push(handle);
    }

    // Wait for all collectors to finish
    for handle in handles {
        if let Err(e) = handle.join() {
            warn!("Collector thread panicked: {:?}", e);
        }
    }

    info!("All collectors stopped, exiting");
    Ok(())
}

/// Setup Ctrl+C handler for graceful shutdown
fn ctrlc_setup(shutdown: Arc<AtomicBool>) {
    // Register signal handler using libc
    #[cfg(unix)]
    {
        // Store shutdown flag in global static for signal handler
        SHUTDOWN_FLAG
            .set(shutdown)
            .expect("Shutdown flag already set");

        // Register signal handlers
        unsafe {
            libc::signal(libc::SIGINT, handle_signal as libc::sighandler_t);
            libc::signal(libc::SIGTERM, handle_signal as libc::sighandler_t);
        }
    }

    #[cfg(not(unix))]
    {
        // On non-Unix platforms, just drop the shutdown flag
        // Graceful shutdown won't work but the program will still run
        let _ = shutdown;
    }
}

#[cfg(unix)]
static SHUTDOWN_FLAG: std::sync::OnceLock<Arc<AtomicBool>> = std::sync::OnceLock::new();

#[cfg(unix)]
extern "C" fn handle_signal(_: libc::c_int) {
    if let Some(flag) = SHUTDOWN_FLAG.get() {
        flag.store(true, Ordering::Relaxed);
    }
}

