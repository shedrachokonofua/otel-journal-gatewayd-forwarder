//! Prometheus metrics endpoint.
//!
//! Exposes metrics at the configured address when `--metrics` is enabled.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::Duration;
use thiserror::Error;
use tracing::{debug, error, info, warn};

#[derive(Error, Debug)]
pub enum MetricsError {
    #[error("Failed to bind to address: {0}")]
    Bind(std::io::Error),
}

/// Metrics for a single source
#[derive(Debug, Clone, Default)]
pub struct SourceMetrics {
    pub entries_forwarded: u64,
    pub poll_errors: HashMap<String, u64>,
    pub last_poll_timestamp: Option<f64>,
    pub last_poll_duration: Option<Duration>,
}

/// Shared metrics state
#[derive(Debug, Default)]
pub struct MetricsState {
    sources: RwLock<HashMap<String, SourceMetrics>>,
}

impl MetricsState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record forwarded entries
    pub fn record_forwarded(&self, source: &str, count: u64) {
        let mut sources = self.sources.write().unwrap();
        let metrics = sources.entry(source.to_string()).or_default();
        metrics.entries_forwarded += count;
    }

    /// Record a poll error
    pub fn record_error(&self, source: &str, error_type: &str) {
        let mut sources = self.sources.write().unwrap();
        let metrics = sources.entry(source.to_string()).or_default();
        *metrics
            .poll_errors
            .entry(error_type.to_string())
            .or_default() += 1;
    }

    /// Record successful poll
    pub fn record_poll(&self, source: &str, duration: Duration) {
        let mut sources = self.sources.write().unwrap();
        let metrics = sources.entry(source.to_string()).or_default();
        metrics.last_poll_timestamp = Some(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs_f64(),
        );
        metrics.last_poll_duration = Some(duration);
    }

    /// Generate Prometheus metrics output
    pub fn render(&self) -> String {
        let sources = self.sources.read().unwrap();
        let mut output = String::new();

        // Entries forwarded
        output.push_str("# HELP ojgf_entries_forwarded_total Total journal entries forwarded\n");
        output.push_str("# TYPE ojgf_entries_forwarded_total counter\n");
        for (source, metrics) in sources.iter() {
            output.push_str(&format!(
                "ojgf_entries_forwarded_total{{source=\"{}\"}} {}\n",
                escape_label(source),
                metrics.entries_forwarded
            ));
        }

        // Poll errors
        output.push_str("# HELP ojgf_poll_errors_total Total poll errors\n");
        output.push_str("# TYPE ojgf_poll_errors_total counter\n");
        for (source, metrics) in sources.iter() {
            for (error_type, count) in &metrics.poll_errors {
                output.push_str(&format!(
                    "ojgf_poll_errors_total{{source=\"{}\",error=\"{}\"}} {}\n",
                    escape_label(source),
                    escape_label(error_type),
                    count
                ));
            }
        }

        // Last poll timestamp
        output.push_str(
            "# HELP ojgf_last_poll_timestamp_seconds Timestamp of last successful poll\n",
        );
        output.push_str("# TYPE ojgf_last_poll_timestamp_seconds gauge\n");
        for (source, metrics) in sources.iter() {
            if let Some(ts) = metrics.last_poll_timestamp {
                output.push_str(&format!(
                    "ojgf_last_poll_timestamp_seconds{{source=\"{}\"}} {:.3}\n",
                    escape_label(source),
                    ts
                ));
            }
        }

        // Poll duration
        output.push_str("# HELP ojgf_poll_duration_seconds Duration of last poll cycle\n");
        output.push_str("# TYPE ojgf_poll_duration_seconds gauge\n");
        for (source, metrics) in sources.iter() {
            if let Some(duration) = metrics.last_poll_duration {
                output.push_str(&format!(
                    "ojgf_poll_duration_seconds{{source=\"{}\"}} {:.3}\n",
                    escape_label(source),
                    duration.as_secs_f64()
                ));
            }
        }

        output
    }
}

/// Escape special characters in label values
fn escape_label(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

/// Start the metrics HTTP server
pub fn start_server(addr: &str, state: Arc<MetricsState>) -> Result<(), MetricsError> {
    let listener = TcpListener::bind(addr).map_err(MetricsError::Bind)?;
    info!(addr = %addr, "Metrics server listening");

    thread::spawn(move || {
        for stream in listener.incoming() {
            match stream {
                Ok(stream) => {
                    let state = state.clone();
                    thread::spawn(move || {
                        if let Err(e) = handle_request(stream, &state) {
                            debug!(error = %e, "Error handling metrics request");
                        }
                    });
                }
                Err(e) => {
                    warn!(error = %e, "Error accepting connection");
                }
            }
        }
    });

    Ok(())
}

fn handle_request(mut stream: TcpStream, state: &MetricsState) -> std::io::Result<()> {
    let mut buf = [0u8; 1024];
    stream.read(&mut buf)?;

    // Simple HTTP parsing - just check for GET /metrics
    let request = String::from_utf8_lossy(&buf);
    let is_metrics_request = request.starts_with("GET /metrics") || request.starts_with("GET / ");

    if is_metrics_request {
        let body = state.render();
        let response = format!(
            "HTTP/1.1 200 OK\r\n\
             Content-Type: text/plain; version=0.0.4; charset=utf-8\r\n\
             Content-Length: {}\r\n\
             Connection: close\r\n\
             \r\n\
             {}",
            body.len(),
            body
        );
        stream.write_all(response.as_bytes())?;
    } else {
        let response = "HTTP/1.1 404 Not Found\r\nConnection: close\r\n\r\n";
        stream.write_all(response.as_bytes())?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_metrics_render() {
        let state = MetricsState::new();
        state.record_forwarded("host-01", 100);
        state.record_error("host-01", "timeout");
        state.record_poll("host-01", Duration::from_millis(234));

        let output = state.render();
        assert!(output.contains("ojgf_entries_forwarded_total{source=\"host-01\"} 100"));
        assert!(output.contains("ojgf_poll_errors_total{source=\"host-01\",error=\"timeout\"} 1"));
    }

    #[test]
    fn test_escape_label() {
        assert_eq!(escape_label("simple"), "simple");
        assert_eq!(escape_label("with\"quote"), "with\\\"quote");
        assert_eq!(escape_label("with\\backslash"), "with\\\\backslash");
    }
}
