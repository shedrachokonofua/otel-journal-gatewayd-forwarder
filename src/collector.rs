//! Source collector - fetches logs from a single source and forwards them.
//!
//! Each source runs its own collector thread.

use crate::config::{Source, TlsConfig};
use crate::cursor::CursorManager;
use crate::journal::{JournalClient, JournalError};
use crate::metrics::MetricsState;
use crate::otlp::{OtlpClient, OtlpError};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;
use thiserror::Error;
use tracing::{debug, error, info, warn};

#[derive(Error, Debug)]
pub enum CollectorError {
    #[error("Journal error: {0}")]
    Journal(#[from] JournalError),
    #[error("OTLP error: {0}")]
    Otlp(#[from] OtlpError),
    #[error("Cursor error: {0}")]
    Cursor(#[from] crate::cursor::CursorError),
}

/// Collector for a single source
pub struct Collector {
    source: Source,
    journal: JournalClient,
    otlp: Arc<OtlpClient>,
    cursor: CursorManager,
    batch_size: usize,
    metrics: Option<Arc<MetricsState>>,
}

impl Collector {
    /// Create a new collector for a source
    pub fn new(
        source: Source,
        global_tls: &Option<TlsConfig>,
        otlp: Arc<OtlpClient>,
        cursor: CursorManager,
        batch_size: usize,
        max_field_bytes: usize,
        metrics: Option<Arc<MetricsState>>,
    ) -> Result<Self, CollectorError> {
        let tls = source.effective_tls(global_tls);
        let journal = JournalClient::new(
            &source.url,
            source.units.clone(),
            tls.as_ref(),
            &source.headers,
            max_field_bytes,
        )?;

        Ok(Self {
            source,
            journal,
            otlp,
            cursor,
            batch_size,
            metrics,
        })
    }

    /// Run a single poll cycle
    pub fn poll(&mut self) -> Result<usize, CollectorError> {
        let start = std::time::Instant::now();
        let current_cursor = self.cursor.load();

        debug!(
            source = %self.source.name,
            cursor = ?current_cursor,
            "Starting poll"
        );

        // Fetch entries from journal
        let entries = match self
            .journal
            .fetch(current_cursor.as_deref(), self.batch_size)
        {
            Ok(entries) => entries,
            Err(JournalError::InvalidCursor) => {
                warn!(
                    source = %self.source.name,
                    "Cursor invalid (410 Gone), resetting to current boot"
                );
                self.cursor.reset()?;

                if let Some(metrics) = &self.metrics {
                    metrics.record_error(&self.source.name, "invalid_cursor");
                }

                // Retry with no cursor (current boot)
                self.journal.fetch(None, self.batch_size)?
            }
            Err(e) => {
                if let Some(metrics) = &self.metrics {
                    let error_type = match &e {
                        JournalError::Http(_) => "http",
                        JournalError::Json(_) => "parse",
                        JournalError::ServerError { .. } => "server",
                        JournalError::InvalidCursor => "invalid_cursor",
                        JournalError::Config(_) => "config",
                    };
                    metrics.record_error(&self.source.name, error_type);
                }
                return Err(e.into());
            }
        };

        if entries.is_empty() {
            debug!(source = %self.source.name, "No new entries");
            if let Some(metrics) = &self.metrics {
                metrics.record_poll(&self.source.name, start.elapsed());
            }
            return Ok(0);
        }

        let count = entries.len();
        let last_cursor = entries.last().map(|e| e.cursor.clone());

        debug!(
            source = %self.source.name,
            count = count,
            "Fetched entries, forwarding to OTLP"
        );

        // Forward to OTLP
        match self
            .otlp
            .send(&self.source.name, &entries, &self.source.labels)
        {
            Ok(()) => {
                // Only advance cursor after successful OTLP push
                if let Some(cursor) = last_cursor {
                    self.cursor.save(&cursor)?;
                }

                let last_entry_realtime = entries.last().map(|e| e.realtime_timestamp);
                if let Some(metrics) = &self.metrics {
                    metrics.record_forwarded(&self.source.name, count as u64);
                    metrics.record_poll(&self.source.name, start.elapsed());
                    metrics.record_last_entry(&self.source.name, last_entry_realtime);
                }

                info!(
                    source = %self.source.name,
                    count = count,
                    duration_ms = start.elapsed().as_millis(),
                    "Forwarded entries"
                );

                Ok(count)
            }
            Err(e) => {
                // Do NOT advance cursor on OTLP failure
                error!(
                    source = %self.source.name,
                    error = %e,
                    "Failed to forward to OTLP, cursor not advanced"
                );

                if let Some(metrics) = &self.metrics {
                    metrics.record_error(&self.source.name, "otlp");
                }

                Err(e.into())
            }
        }
    }

    /// Get source name
    pub fn source_name(&self) -> &str {
        &self.source.name
    }
}

const MAX_DRAIN_BATCHES: u32 = 100;
const MAX_BACKOFF: Duration = Duration::from_secs(300);

/// Compute the next sleep duration after `consecutive_failures` failures.
fn backoff_delay(base: Duration, failures: u32) -> Duration {
    if failures == 0 {
        return base;
    }
    let factor = 2u32.saturating_pow(failures.min(8));
    base.saturating_mul(factor).min(MAX_BACKOFF)
}

/// Run collector in a loop until shutdown signal.
///
/// In `--once` mode, drain cycles repeat without sleeping until a short batch
/// is reached (source caught up), so the per-cycle cap never leaves data behind.
pub fn run_loop(
    mut collector: Collector,
    poll_interval: Duration,
    shutdown: Arc<AtomicBool>,
    once: bool,
    tick: Arc<AtomicU64>,
) {
    let source_name = collector.source_name().to_string();
    info!(source = %source_name, "Collector started");

    let mut consecutive_failures: u32 = 0;

    loop {
        if shutdown.load(Ordering::Relaxed) {
            info!(source = %source_name, "Collector shutting down");
            break;
        }

        let result = drain_cycle(&mut collector, MAX_DRAIN_BATCHES, shutdown.clone());
        match &result {
            Ok(0) => {
                consecutive_failures = 0;
                debug!(source = %source_name, "No new entries");
            }
            Ok(n) => {
                consecutive_failures = 0;
                debug!(source = %source_name, count = n, "Drain cycle completed");
            }
            Err(e) => {
                consecutive_failures = consecutive_failures.saturating_add(1);
                warn!(source = %source_name, error = %e, "Poll failed, will retry");
            }
        }

        tick.store(current_unix_ms(), Ordering::Relaxed);

        if once {
            match &result {
                Ok(n) if *n > 0 && !shutdown.load(Ordering::Relaxed) => {
                    // The per-cycle cap may leave data behind; keep draining.
                    debug!(
                        source = %source_name,
                        count = n,
                        "Once mode: drain cycle hit cap, continuing"
                    );
                    continue;
                }
                _ => {
                    debug!(source = %source_name, "Once mode, exiting");
                    break;
                }
            }
        }

        let delay = backoff_delay(poll_interval, consecutive_failures);
        let mut remaining = delay;
        while remaining > Duration::ZERO && !shutdown.load(Ordering::Relaxed) {
            let sleep = remaining.min(Duration::from_millis(100));
            std::thread::sleep(sleep);
            remaining = remaining.saturating_sub(sleep);
        }
    }
}

fn current_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

/// Fetch and forward up to `max_batches` in one cycle, stopping early if a batch
/// is short (likely caught up) or if shutdown is requested.
fn drain_cycle(
    collector: &mut Collector,
    max_batches: u32,
    shutdown: Arc<AtomicBool>,
) -> Result<usize, CollectorError> {
    let mut total = 0usize;
    for i in 0..max_batches {
        if shutdown.load(Ordering::Relaxed) {
            break;
        }
        let count = collector.poll()?;
        total += count;
        if count == 0 || count < collector.batch_size {
            // Short batch means we're caught up (or empty); don't burn cycles.
            break;
        }
        // If we returned a full batch, there may be more; keep draining.
        debug!(
            batch = i + 1,
            count = count,
            "Fetched full batch, continuing drain"
        );
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_backoff_delay() {
        let base = Duration::from_secs(5);
        assert_eq!(backoff_delay(base, 0), base);
        assert_eq!(backoff_delay(base, 1), Duration::from_secs(10));
        assert_eq!(backoff_delay(base, 2), Duration::from_secs(20));
        assert_eq!(
            backoff_delay(base, 8),
            Duration::from_secs(300).min(Duration::from_secs(1280))
        );
        assert_eq!(backoff_delay(base, 100), Duration::from_secs(300));
    }

    #[test]
    fn test_backoff_delay_min() {
        let base = Duration::from_millis(100);
        assert_eq!(backoff_delay(base, 1), Duration::from_millis(200));
    }
}
