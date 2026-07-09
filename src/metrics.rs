//! Prometheus metrics endpoint.
//!
//! Exposes metrics at the configured address when `--metrics` is enabled.

use parking_lot::RwLock;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use thiserror::Error;
use tracing::{debug, info, warn};

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
    pub last_entry_realtime_us: Option<u64>,
    pub last_success_timestamp: Option<f64>,
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
        let mut sources = self.sources.write();
        let metrics = sources.entry(source.to_string()).or_default();
        metrics.entries_forwarded += count;
    }

    /// Record a poll error
    pub fn record_error(&self, source: &str, error_type: &str) {
        let mut sources = self.sources.write();
        let metrics = sources.entry(source.to_string()).or_default();
        *metrics
            .poll_errors
            .entry(error_type.to_string())
            .or_default() += 1;
    }

    /// Record successful poll
    pub fn record_poll(&self, source: &str, duration: Duration) {
        let mut sources = self.sources.write();
        let metrics = sources.entry(source.to_string()).or_default();
        metrics.last_poll_timestamp = Some(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs_f64(),
        );
        metrics.last_poll_duration = Some(duration);
    }

    /// Record the realtime timestamp of the last forwarded entry for lag calc
    /// and update the last-success timestamp.
    pub fn record_last_entry(&self, source: &str, realtime_us: Option<u64>) {
        let mut sources = self.sources.write();
        let metrics = sources.entry(source.to_string()).or_default();
        metrics.last_entry_realtime_us = realtime_us;
        metrics.last_success_timestamp = Some(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs_f64(),
        );
    }

    /// Generate Prometheus metrics output
    pub fn render(&self) -> String {
        let sources = self.sources.read();
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

        // Source lag (now - last forwarded entry realtime)
        output.push_str(
            "# HELP ojgf_source_lag_seconds Time since the last forwarded entry was emitted\n",
        );
        output.push_str("# TYPE ojgf_source_lag_seconds gauge\n");
        let now_s = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs_f64();
        for (source, metrics) in sources.iter() {
            if let Some(us) = metrics.last_entry_realtime_us {
                let lag = (now_s - (us as f64 / 1_000_000.0)).max(0.0);
                output.push_str(&format!(
                    "ojgf_source_lag_seconds{{source=\"{}\"}} {:.3}\n",
                    escape_label(source),
                    lag
                ));
            }
        }

        // Last successful forward timestamp
        output.push_str(
            "# HELP ojgf_last_success_timestamp_seconds Timestamp of last successful OTLP export\n",
        );
        output.push_str("# TYPE ojgf_last_success_timestamp_seconds gauge\n");
        for (source, metrics) in sources.iter() {
            if let Some(ts) = metrics.last_success_timestamp {
                output.push_str(&format!(
                    "ojgf_last_success_timestamp_seconds{{source=\"{}\"}} {:.3}\n",
                    escape_label(source),
                    ts
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

fn handle_request(mut stream: impl Read + Write, state: &MetricsState) -> std::io::Result<()> {
    let mut buf = [0u8; 1024];
    let _ = stream.read(&mut buf)?;

    // Simple HTTP parsing
    let request = String::from_utf8_lossy(&buf);
    let is_health_request = request.starts_with("GET /healthz");
    let is_metrics_request = request.starts_with("GET /metrics") || request.starts_with("GET / ");

    if is_health_request {
        let body = "ok";
        let response = format!(
            "HTTP/1.1 200 OK\r\n\
             Content-Type: text/plain\r\n\
             Content-Length: {}\r\n\
             Connection: close\r\n\
             \r\n\
             {}",
            body.len(),
            body
        );
        stream.write_all(response.as_bytes())?;
    } else if is_metrics_request {
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
        state.record_last_entry("host-01", Some(1_703_456_789_000_000));

        let output = state.render();
        assert!(output.contains("ojgf_entries_forwarded_total{source=\"host-01\"} 100"));
        assert!(output.contains("ojgf_poll_errors_total{source=\"host-01\",error=\"timeout\"} 1"));
        assert!(output.contains("ojgf_source_lag_seconds{source=\"host-01\"}"));
        assert!(output.contains("ojgf_last_success_timestamp_seconds{source=\"host-01\"}"));
    }

    #[test]
    fn test_healthz_request() {
        let state = MetricsState::new();
        let request = b"GET /healthz HTTP/1.1\r\n\r\n";
        let mut stream = MockStream {
            read_buf: request.to_vec(),
            write_buf: Vec::new(),
        };
        handle_request(&mut stream, &state).unwrap();
        let response = String::from_utf8(stream.write_buf).unwrap();
        assert!(response.starts_with("HTTP/1.1 200 OK"));
        assert!(response.ends_with("\r\n\r\nok"));
    }

    #[test]
    fn test_metrics_request() {
        let state = MetricsState::new();
        state.record_forwarded("host-01", 42);
        let request = b"GET /metrics HTTP/1.1\r\n\r\n";
        let mut stream = MockStream {
            read_buf: request.to_vec(),
            write_buf: Vec::new(),
        };
        handle_request(&mut stream, &state).unwrap();
        let response = String::from_utf8(stream.write_buf).unwrap();
        assert!(response.starts_with("HTTP/1.1 200 OK"));
        assert!(response.contains("ojgf_entries_forwarded_total{source=\"host-01\"} 42"));
    }

    #[test]
    fn test_escape_label() {
        assert_eq!(escape_label("simple"), "simple");
        assert_eq!(escape_label("with\"quote"), "with\\\"quote");
        assert_eq!(escape_label("with\\backslash"), "with\\\\backslash");
    }

    struct MockStream {
        read_buf: Vec<u8>,
        write_buf: Vec<u8>,
    }

    impl Read for MockStream {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = std::cmp::min(buf.len(), self.read_buf.len());
            buf[..n].copy_from_slice(&self.read_buf[..n]);
            self.read_buf.drain(..n);
            Ok(n)
        }
    }

    impl Write for MockStream {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.write_buf.extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
}
