//! Source collector - fetches logs from a single source and forwards them.
//!
//! Each source runs its own collector thread.

use crate::config::Source;
use crate::cursor::CursorManager;
use crate::journal::{JournalClient, JournalError};
use crate::metrics::MetricsState;
use crate::otlp::{OtlpClient, OtlpError};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
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
        otlp: Arc<OtlpClient>,
        cursor: CursorManager,
        batch_size: usize,
        metrics: Option<Arc<MetricsState>>,
    ) -> Result<Self, CollectorError> {
        let journal = JournalClient::new(&source.url, source.units.clone())?;

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
        let entries = match self.journal.fetch(current_cursor.as_deref(), self.batch_size) {
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
        match self.otlp.send(&self.source.name, &entries, &self.source.labels) {
            Ok(()) => {
                // Only advance cursor after successful OTLP push
                if let Some(cursor) = last_cursor {
                    self.cursor.save(&cursor)?;
                }

                if let Some(metrics) = &self.metrics {
                    metrics.record_forwarded(&self.source.name, count as u64);
                    metrics.record_poll(&self.source.name, start.elapsed());
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

/// Run collector in a loop until shutdown signal
pub fn run_loop(
    mut collector: Collector,
    poll_interval: Duration,
    shutdown: Arc<AtomicBool>,
    once: bool,
) {
    let source_name = collector.source_name().to_string();
    info!(source = %source_name, "Collector started");

    loop {
        // Check shutdown flag
        if shutdown.load(Ordering::Relaxed) {
            info!(source = %source_name, "Collector shutting down");
            break;
        }

        // Poll
        match collector.poll() {
            Ok(count) => {
                debug!(source = %source_name, count = count, "Poll completed");
            }
            Err(e) => {
                warn!(source = %source_name, error = %e, "Poll failed, will retry");
            }
        }

        // Exit if --once mode
        if once {
            debug!(source = %source_name, "Once mode, exiting");
            break;
        }

        // Wait for next poll interval (check shutdown every 100ms)
        let mut remaining = poll_interval;
        while remaining > Duration::ZERO && !shutdown.load(Ordering::Relaxed) {
            let sleep = remaining.min(Duration::from_millis(100));
            std::thread::sleep(sleep);
            remaining = remaining.saturating_sub(sleep);
        }
    }
}

#[cfg(test)]
mod tests {
    // Integration tests would require mocking the HTTP endpoints
    // Use wiremock for proper testing when available
}

