//! Journal gatewayd HTTP API client.
//!
//! Fetches journal entries from systemd-journal-gatewayd endpoints.
//! See: https://www.freedesktop.org/software/systemd/man/latest/systemd-journal-gatewayd.service.html

use reqwest::blocking::Client;
use reqwest::StatusCode;
use serde::Deserialize;
use std::collections::HashMap;
use std::time::Duration;
use thiserror::Error;
use tracing::{debug, trace, warn};

/// HTTP timeout for gatewayd requests
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Error, Debug)]
pub enum JournalError {
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("Failed to parse JSON response: {0}")]
    Json(#[from] serde_json::Error),
    #[error("Invalid cursor (410 Gone)")]
    InvalidCursor,
    #[error("Server error: {status}")]
    ServerError { status: StatusCode },
}

/// A journal entry from gatewayd
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct JournalEntry {
    /// The cursor string for this entry
    pub cursor: String,
    /// Realtime timestamp in microseconds
    pub realtime_timestamp: u64,
    /// Monotonic timestamp in microseconds
    pub monotonic_timestamp: Option<u64>,
    /// Boot ID
    pub boot_id: Option<String>,
    /// Log message
    pub message: String,
    /// Priority (0-7)
    pub priority: Option<u8>,
    /// Systemd unit name
    pub systemd_unit: Option<String>,
    /// Syslog identifier
    pub syslog_identifier: Option<String>,
    /// Process ID
    pub pid: Option<String>,
    /// User ID
    pub uid: Option<String>,
    /// Group ID
    pub gid: Option<String>,
    /// Command name
    pub comm: Option<String>,
    /// Executable path
    pub exe: Option<String>,
    /// Machine ID
    pub machine_id: Option<String>,
    /// Hostname
    pub hostname: Option<String>,
    /// All other fields
    pub extra_fields: HashMap<String, String>,
}

/// Raw journal entry as returned by gatewayd
#[derive(Debug, Deserialize)]
struct RawJournalEntry {
    #[serde(rename = "__CURSOR")]
    cursor: String,
    #[serde(rename = "__REALTIME_TIMESTAMP")]
    realtime_timestamp: String,
    #[serde(rename = "__MONOTONIC_TIMESTAMP")]
    monotonic_timestamp: Option<String>,
    #[serde(rename = "_BOOT_ID")]
    boot_id: Option<String>,
    #[serde(rename = "MESSAGE")]
    message: Option<serde_json::Value>,
    #[serde(rename = "PRIORITY")]
    priority: Option<String>,
    #[serde(rename = "_SYSTEMD_UNIT")]
    systemd_unit: Option<String>,
    #[serde(rename = "SYSLOG_IDENTIFIER")]
    syslog_identifier: Option<String>,
    #[serde(rename = "_PID")]
    pid: Option<String>,
    #[serde(rename = "_UID")]
    uid: Option<String>,
    #[serde(rename = "_GID")]
    gid: Option<String>,
    #[serde(rename = "_COMM")]
    comm: Option<String>,
    #[serde(rename = "_EXE")]
    exe: Option<String>,
    #[serde(rename = "_MACHINE_ID")]
    machine_id: Option<String>,
    #[serde(rename = "_HOSTNAME")]
    hostname: Option<String>,
    #[serde(flatten)]
    extra: HashMap<String, serde_json::Value>,
}

impl From<RawJournalEntry> for JournalEntry {
    fn from(raw: RawJournalEntry) -> Self {
        // Parse message - can be a string or an array of bytes
        let message = match raw.message {
            Some(serde_json::Value::String(s)) => s,
            Some(serde_json::Value::Array(arr)) => {
                // Array of byte values - convert to string
                let bytes: Vec<u8> = arr
                    .iter()
                    .filter_map(|v| v.as_u64().map(|n| n as u8))
                    .collect();
                String::from_utf8_lossy(&bytes).to_string()
            }
            _ => String::new(),
        };

        // Convert extra fields to strings
        let extra_fields: HashMap<String, String> = raw
            .extra
            .into_iter()
            .filter_map(|(k, v)| {
                // Skip internal fields we've already processed
                if k.starts_with("__") || k == "MESSAGE" || k == "PRIORITY" {
                    return None;
                }
                let value = match v {
                    serde_json::Value::String(s) => s,
                    serde_json::Value::Array(arr) => {
                        let bytes: Vec<u8> = arr
                            .iter()
                            .filter_map(|v| v.as_u64().map(|n| n as u8))
                            .collect();
                        String::from_utf8_lossy(&bytes).to_string()
                    }
                    other => other.to_string(),
                };
                Some((k, value))
            })
            .collect();

        JournalEntry {
            cursor: raw.cursor,
            realtime_timestamp: raw.realtime_timestamp.parse().unwrap_or(0),
            monotonic_timestamp: raw
                .monotonic_timestamp
                .as_ref()
                .and_then(|s| s.parse().ok()),
            boot_id: raw.boot_id,
            message,
            priority: raw.priority.as_ref().and_then(|s| s.parse().ok()),
            systemd_unit: raw.systemd_unit,
            syslog_identifier: raw.syslog_identifier,
            pid: raw.pid,
            uid: raw.uid,
            gid: raw.gid,
            comm: raw.comm,
            exe: raw.exe,
            machine_id: raw.machine_id,
            hostname: raw.hostname,
            extra_fields,
        }
    }
}

/// Journal gatewayd client
pub struct JournalClient {
    client: Client,
    base_url: String,
    units: Vec<String>,
}

impl JournalClient {
    /// Create a new journal client
    pub fn new(base_url: &str, units: Vec<String>) -> Result<Self, JournalError> {
        let client = Client::builder().timeout(REQUEST_TIMEOUT).build()?;

        // Normalize URL (remove trailing slash)
        let base_url = base_url.trim_end_matches('/').to_string();

        Ok(Self {
            client,
            base_url,
            units,
        })
    }

    /// Fetch journal entries
    ///
    /// If cursor is Some, fetch entries after that cursor.
    /// If cursor is None, fetch entries from current boot.
    pub fn fetch(
        &self,
        cursor: Option<&str>,
        batch_size: usize,
    ) -> Result<Vec<JournalEntry>, JournalError> {
        let mut url = format!("{}/entries", self.base_url);
        let mut query_parts = Vec::new();

        // Add cursor or boot filter
        if let Some(c) = cursor {
            query_parts.push(format!("cursor={}", urlencoding::encode(c)));
            query_parts.push("skip=1".to_string()); // Skip the cursor entry itself
        } else {
            query_parts.push("boot".to_string());
        }

        // Add unit filters
        for unit in &self.units {
            query_parts.push(format!("_SYSTEMD_UNIT={}", urlencoding::encode(unit)));
        }

        if !query_parts.is_empty() {
            url = format!("{}?{}", url, query_parts.join("&"));
        }

        debug!(url = %url, "Fetching journal entries");

        let response = self
            .client
            .get(&url)
            .header("Accept", "application/json")
            .header("Range", format!("entries=:{}", batch_size))
            .send()?;

        let status = response.status();
        trace!(status = %status, "Got response");

        match status {
            StatusCode::OK => {
                // Parse newline-delimited JSON
                let body = response.text()?;
                self.parse_entries(&body)
            }
            StatusCode::NO_CONTENT => {
                debug!("No new entries");
                Ok(Vec::new())
            }
            StatusCode::GONE => {
                warn!("Cursor is no longer valid (410 Gone)");
                Err(JournalError::InvalidCursor)
            }
            _ => Err(JournalError::ServerError { status }),
        }
    }

    /// Parse newline-delimited JSON entries
    fn parse_entries(&self, body: &str) -> Result<Vec<JournalEntry>, JournalError> {
        let mut entries = Vec::new();

        for line in body.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }

            match serde_json::from_str::<RawJournalEntry>(line) {
                Ok(raw) => {
                    entries.push(JournalEntry::from(raw));
                }
                Err(e) => {
                    warn!(error = %e, line = %line.chars().take(100).collect::<String>(), "Failed to parse journal entry, skipping");
                }
            }
        }

        debug!(count = entries.len(), "Parsed journal entries");
        Ok(entries)
    }
}

// URL encoding helper
mod urlencoding {
    pub fn encode(s: &str) -> String {
        let mut result = String::new();
        for c in s.chars() {
            match c {
                'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' => result.push(c),
                _ => {
                    for b in c.to_string().as_bytes() {
                        result.push_str(&format!("%{:02X}", b));
                    }
                }
            }
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_entry() {
        let json = r#"{"__CURSOR":"s=abc;i=1","__REALTIME_TIMESTAMP":"1703456789000000","MESSAGE":"Hello world","PRIORITY":"6","_SYSTEMD_UNIT":"test.service"}"#;
        let raw: RawJournalEntry = serde_json::from_str(json).unwrap();
        let entry = JournalEntry::from(raw);

        assert_eq!(entry.cursor, "s=abc;i=1");
        assert_eq!(entry.realtime_timestamp, 1703456789000000);
        assert_eq!(entry.message, "Hello world");
        assert_eq!(entry.priority, Some(6));
        assert_eq!(entry.systemd_unit, Some("test.service".to_string()));
    }

    #[test]
    fn test_parse_binary_message() {
        let json = r#"{"__CURSOR":"s=abc;i=1","__REALTIME_TIMESTAMP":"1703456789000000","MESSAGE":[72,101,108,108,111]}"#;
        let raw: RawJournalEntry = serde_json::from_str(json).unwrap();
        let entry = JournalEntry::from(raw);

        assert_eq!(entry.message, "Hello");
    }

    #[test]
    fn test_url_encoding() {
        assert_eq!(urlencoding::encode("hello world"), "hello%20world");
        assert_eq!(urlencoding::encode("s=abc;i=1"), "s%3Dabc%3Bi%3D1");
    }
}
