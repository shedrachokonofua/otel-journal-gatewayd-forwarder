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
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::Duration;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

#[cfg(unix)]
use sd_notify::NotifyState;

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
    setup_signals(shutdown.clone())?;

    // Setup metrics if enabled
    let metrics = if let Some(ref addr) = cli.metrics {
        let state = Arc::new(metrics::MetricsState::new());
        metrics::start_server(addr, state.clone())?;
        Some(state)
    } else {
        None
    };

    // Create shared OTLP client
    let otlp = Arc::new(otlp::OtlpClient::new(
        &config.otlp_endpoint,
        config.tls.as_ref(),
        &config.otlp_headers,
    )?);

    // Start collector threads, each with a freshness tick
    let mut source_states = Vec::new();

    for source in config.sources {
        let cursor = cursor::CursorManager::new(&config.cursor_dir, &source.name)?;
        let collector = collector::Collector::new(
            source,
            &config.tls,
            otlp.clone(),
            cursor,
            config.batch_size,
            config.max_field_bytes,
            metrics.clone(),
        )?;

        let shutdown = shutdown.clone();
        let poll_interval = config.poll_interval;
        let once = cli.once;
        let tick = Arc::new(AtomicU64::new(current_unix_ms()));
        let thread_tick = tick.clone();
        let source_name = collector.source_name().to_string();

        let handle = thread::spawn(move || {
            collector::run_loop(collector, poll_interval, shutdown, once, thread_tick);
        });

        source_states.push((source_name, poll_interval, tick, handle));
    }

    // Notify systemd that the service is ready now that all collectors are spawned.
    #[cfg(unix)]
    {
        if let Err(e) = sd_notify::notify(false, &[NotifyState::Ready]) {
            warn!(error = %e, "Failed to send systemd ready notification");
        } else {
            info!("Sent systemd ready notification");
        }
    }

    // Wait for all collectors to finish, optionally pinging the systemd watchdog.
    #[cfg(unix)]
    {
        let mut usec = 0u64;
        let enabled = sd_notify::watchdog_enabled(false, &mut usec);
        if enabled {
            let timeout = Duration::from_micros(usec);
            info!(timeout_ms = timeout.as_millis(), "systemd watchdog enabled");
            wait_with_watchdog(source_states, shutdown.clone(), timeout)?;
        } else {
            info!("systemd watchdog not enabled; joining collectors directly");
            join_collectors(source_states)?;
        }
    }

    #[cfg(not(unix))]
    {
        join_collectors(source_states)?;
    }

    info!("All collectors stopped, exiting");
    Ok(())
}

/// Wait for collectors to finish while periodically pinging the systemd
/// watchdog as long as every source has ticked within its own freshness window.
#[cfg(unix)]
fn wait_with_watchdog(
    source_states: Vec<(String, Duration, Arc<AtomicU64>, thread::JoinHandle<()>)>,
    shutdown: Arc<AtomicBool>,
    timeout: Duration,
) -> Result<(), Box<dyn std::error::Error>> {
    let ping_interval = timeout / 2;
    let now = current_unix_ms;

    loop {
        // Ping only while every source still appears alive.
        let all_fresh = source_states.iter().all(|(_, poll_interval, tick, _)| {
            let window = poll_interval.saturating_mul(5).max(Duration::from_secs(60));
            now().saturating_sub(tick.load(Ordering::Relaxed)) <= window.as_millis() as u64
        });

        if all_fresh {
            if let Err(e) = sd_notify::notify(false, &[NotifyState::Watchdog]) {
                warn!(error = %e, "Failed to send systemd watchdog notification");
            }
        } else {
            warn!("Skipping systemd watchdog ping: one or more sources appear stale");
        }

        // Sleep in short slices so we can detect finished threads promptly.
        let mut remaining = ping_interval;
        while remaining > Duration::ZERO && !shutdown.load(Ordering::Relaxed) {
            let slice = remaining.min(Duration::from_millis(100));
            thread::sleep(slice);
            remaining = remaining.saturating_sub(slice);

            // If any thread finished (e.g. --once), switch to a plain join.
            if source_states
                .iter()
                .any(|(_, _, _, handle)| handle.is_finished())
            {
                return join_collectors(source_states);
            }
        }

        if shutdown.load(Ordering::Relaxed) {
            return join_collectors(source_states);
        }
    }
}

/// Join all collector threads and surface any panics.
fn join_collectors(
    source_states: Vec<(String, Duration, Arc<AtomicU64>, thread::JoinHandle<()>)>,
) -> Result<(), Box<dyn std::error::Error>> {
    for (source_name, _, _, handle) in source_states {
        if let Err(e) = handle.join() {
            warn!(source = %source_name, panic = ?e, "Collector thread panicked");
        }
    }
    Ok(())
}

fn current_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

#[cfg(unix)]
fn setup_signals(shutdown: Arc<AtomicBool>) -> Result<(), Box<dyn std::error::Error>> {
    signal_hook::flag::register(signal_hook::consts::SIGTERM, shutdown.clone())?;
    signal_hook::flag::register(signal_hook::consts::SIGINT, shutdown.clone())?;
    Ok(())
}

#[cfg(not(unix))]
fn setup_signals(_shutdown: Arc<AtomicBool>) -> Result<(), Box<dyn std::error::Error>> {
    Ok(())
}
