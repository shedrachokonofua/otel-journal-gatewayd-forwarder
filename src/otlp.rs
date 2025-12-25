//! OTLP/HTTP client for forwarding logs.
//!
//! Sends logs to OTLP-compatible backends via HTTP/JSON.
//! Endpoint: `{otlp_endpoint}/v1/logs`

use crate::journal::JournalEntry;
use reqwest::blocking::Client;
use reqwest::StatusCode;
use serde::Serialize;
use std::collections::HashMap;
use std::time::Duration;
use thiserror::Error;
use tracing::{debug, trace, warn};

/// HTTP timeout for OTLP requests
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Error, Debug)]
pub enum OtlpError {
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("Server rejected request: {status} - {body}")]
    ServerError { status: StatusCode, body: String },
}

/// OTLP client for sending logs
pub struct OtlpClient {
    client: Client,
    endpoint: String,
}

impl OtlpClient {
    /// Create a new OTLP client
    pub fn new(endpoint: &str) -> Result<Self, OtlpError> {
        let client = Client::builder().timeout(REQUEST_TIMEOUT).build()?;

        // Normalize endpoint
        let endpoint = endpoint.trim_end_matches('/').to_string();
        let endpoint = format!("{}/v1/logs", endpoint);

        Ok(Self { client, endpoint })
    }

    /// Send log records to the OTLP endpoint
    pub fn send(
        &self,
        source_name: &str,
        entries: &[JournalEntry],
        labels: &HashMap<String, String>,
    ) -> Result<(), OtlpError> {
        if entries.is_empty() {
            return Ok(());
        }

        let payload = build_otlp_payload(source_name, entries, labels);
        let json = serde_json::to_string(&payload).expect("Failed to serialize OTLP payload");

        trace!(endpoint = %self.endpoint, records = entries.len(), "Sending OTLP logs");

        let response = self
            .client
            .post(&self.endpoint)
            .header("Content-Type", "application/json")
            .body(json)
            .send()?;

        let status = response.status();

        if status.is_success() {
            debug!(
                records = entries.len(),
                "Successfully sent logs to OTLP endpoint"
            );
            Ok(())
        } else {
            let body = response.text().unwrap_or_default();
            warn!(status = %status, body = %body, "OTLP endpoint rejected request");
            Err(OtlpError::ServerError { status, body })
        }
    }
}

// ============================================================================
// OTLP Protocol Structures
// ============================================================================

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ExportLogsServiceRequest {
    resource_logs: Vec<ResourceLogs>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ResourceLogs {
    resource: Resource,
    scope_logs: Vec<ScopeLogs>,
}

#[derive(Serialize)]
struct Resource {
    attributes: Vec<KeyValue>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ScopeLogs {
    scope: Scope,
    log_records: Vec<LogRecord>,
}

#[derive(Serialize)]
struct Scope {
    name: String,
    version: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct LogRecord {
    time_unix_nano: String,
    observed_time_unix_nano: String,
    severity_number: u8,
    severity_text: String,
    body: AnyValue,
    attributes: Vec<KeyValue>,
}

#[derive(Serialize)]
struct KeyValue {
    key: String,
    value: AttributeValue,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AttributeValue {
    #[serde(skip_serializing_if = "Option::is_none")]
    string_value: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    int_value: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AnyValue {
    string_value: String,
}

// ============================================================================
// Payload Building
// ============================================================================

fn build_otlp_payload(
    source_name: &str,
    entries: &[JournalEntry],
    labels: &HashMap<String, String>,
) -> ExportLogsServiceRequest {
    // Group entries by service (systemd unit)
    let mut by_service: HashMap<String, Vec<&JournalEntry>> = HashMap::new();
    for entry in entries {
        let service = entry
            .systemd_unit
            .clone()
            .unwrap_or_else(|| "unknown".to_string());
        by_service.entry(service).or_default().push(entry);
    }

    let mut resource_logs = Vec::new();

    for (service, service_entries) in by_service {
        // Build resource attributes
        let mut resource_attrs = vec![
            KeyValue {
                key: "host.name".to_string(),
                value: AttributeValue {
                    string_value: Some(source_name.to_string()),
                    int_value: None,
                },
            },
            KeyValue {
                key: "service.name".to_string(),
                value: AttributeValue {
                    string_value: Some(service),
                    int_value: None,
                },
            },
            KeyValue {
                key: "os.type".to_string(),
                value: AttributeValue {
                    string_value: Some("linux".to_string()),
                    int_value: None,
                },
            },
        ];

        // Add custom labels
        for (key, value) in labels {
            resource_attrs.push(KeyValue {
                key: key.clone(),
                value: AttributeValue {
                    string_value: Some(value.clone()),
                    int_value: None,
                },
            });
        }

        // Build log records
        let log_records: Vec<LogRecord> = service_entries
            .into_iter()
            .map(|entry| build_log_record(entry))
            .collect();

        resource_logs.push(ResourceLogs {
            resource: Resource {
                attributes: resource_attrs,
            },
            scope_logs: vec![ScopeLogs {
                scope: Scope {
                    name: "otel-journal-gatewayd-forwarder".to_string(),
                    version: env!("CARGO_PKG_VERSION").to_string(),
                },
                log_records,
            }],
        });
    }

    ExportLogsServiceRequest { resource_logs }
}

fn build_log_record(entry: &JournalEntry) -> LogRecord {
    // Convert microseconds to nanoseconds
    let time_unix_nano = entry.realtime_timestamp * 1000;
    let now_ns = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64;

    let (severity_number, severity_text) = map_priority(entry.priority);

    // Build attributes from journal fields
    let mut attributes = Vec::new();

    if let Some(ref pid) = entry.pid {
        attributes.push(KeyValue {
            key: "process.pid".to_string(),
            value: AttributeValue {
                string_value: Some(pid.clone()),
                int_value: None,
            },
        });
    }

    if let Some(ref uid) = entry.uid {
        attributes.push(KeyValue {
            key: "process.owner".to_string(),
            value: AttributeValue {
                string_value: Some(uid.clone()),
                int_value: None,
            },
        });
    }

    if let Some(ref comm) = entry.comm {
        attributes.push(KeyValue {
            key: "process.command".to_string(),
            value: AttributeValue {
                string_value: Some(comm.clone()),
                int_value: None,
            },
        });
    }

    if let Some(ref exe) = entry.exe {
        attributes.push(KeyValue {
            key: "process.executable.path".to_string(),
            value: AttributeValue {
                string_value: Some(exe.clone()),
                int_value: None,
            },
        });
    }

    if let Some(ref syslog_id) = entry.syslog_identifier {
        attributes.push(KeyValue {
            key: "syslog.identifier".to_string(),
            value: AttributeValue {
                string_value: Some(syslog_id.clone()),
                int_value: None,
            },
        });
    }

    if let Some(ref boot_id) = entry.boot_id {
        attributes.push(KeyValue {
            key: "systemd.boot_id".to_string(),
            value: AttributeValue {
                string_value: Some(boot_id.clone()),
                int_value: None,
            },
        });
    }

    // Add journal cursor as attribute (useful for debugging)
    attributes.push(KeyValue {
        key: "systemd.cursor".to_string(),
        value: AttributeValue {
            string_value: Some(entry.cursor.clone()),
            int_value: None,
        },
    });

    // Add extra fields
    for (key, value) in &entry.extra_fields {
        // Convert journal field names to something more reasonable
        let attr_key = key.to_lowercase().replace('_', ".");
        attributes.push(KeyValue {
            key: attr_key,
            value: AttributeValue {
                string_value: Some(value.clone()),
                int_value: None,
            },
        });
    }

    LogRecord {
        time_unix_nano: time_unix_nano.to_string(),
        observed_time_unix_nano: now_ns.to_string(),
        severity_number,
        severity_text: severity_text.to_string(),
        body: AnyValue {
            string_value: entry.message.clone(),
        },
        attributes,
    }
}

/// Map journal PRIORITY to OTLP severity
///
/// | Journal PRIORITY | OTLP Severity |
/// |------------------|---------------|
/// | 0 (emerg)        | 21 (FATAL)    |
/// | 1 (alert)        | 21 (FATAL)    |
/// | 2 (crit)         | 17 (ERROR)    |
/// | 3 (err)          | 17 (ERROR)    |
/// | 4 (warning)      | 13 (WARN)     |
/// | 5 (notice)       | 9 (INFO)      |
/// | 6 (info)         | 9 (INFO)      |
/// | 7 (debug)        | 5 (DEBUG)     |
fn map_priority(priority: Option<u8>) -> (u8, &'static str) {
    match priority {
        Some(0) | Some(1) => (21, "FATAL"),
        Some(2) | Some(3) => (17, "ERROR"),
        Some(4) => (13, "WARN"),
        Some(5) | Some(6) => (9, "INFO"),
        Some(7) => (5, "DEBUG"),
        None => (0, "UNSPECIFIED"),
        _ => (0, "UNSPECIFIED"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_priority_mapping() {
        assert_eq!(map_priority(Some(0)), (21, "FATAL"));
        assert_eq!(map_priority(Some(3)), (17, "ERROR"));
        assert_eq!(map_priority(Some(4)), (13, "WARN"));
        assert_eq!(map_priority(Some(6)), (9, "INFO"));
        assert_eq!(map_priority(Some(7)), (5, "DEBUG"));
        assert_eq!(map_priority(None), (0, "UNSPECIFIED"));
    }

    #[test]
    fn test_build_payload() {
        let entries = vec![JournalEntry {
            cursor: "s=abc;i=1".to_string(),
            realtime_timestamp: 1703456789000000,
            monotonic_timestamp: None,
            boot_id: Some("boot123".to_string()),
            message: "Test message".to_string(),
            priority: Some(6),
            systemd_unit: Some("test.service".to_string()),
            syslog_identifier: None,
            pid: Some("1234".to_string()),
            uid: None,
            gid: None,
            comm: None,
            exe: None,
            machine_id: None,
            hostname: None,
            extra_fields: HashMap::new(),
        }];

        let labels = HashMap::from([("env".to_string(), "test".to_string())]);
        let payload = build_otlp_payload("test-host", &entries, &labels);

        assert_eq!(payload.resource_logs.len(), 1);
        let resource = &payload.resource_logs[0];
        assert_eq!(resource.scope_logs.len(), 1);
        assert_eq!(resource.scope_logs[0].log_records.len(), 1);

        let record = &resource.scope_logs[0].log_records[0];
        assert_eq!(record.body.string_value, "Test message");
        assert_eq!(record.severity_number, 9);
        assert_eq!(record.severity_text, "INFO");
    }
}
