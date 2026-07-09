//! Configuration handling for the forwarder.
//!
//! Supports:
//! - TOML config file
//! - Environment variables (OJGF_* prefix)
//! - CLI arguments

use clap::Parser;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;
use thiserror::Error;

/// Default config file path
pub const DEFAULT_CONFIG_PATH: &str = "/etc/otel-journal-gatewayd-forwarder/config.toml";
/// Default cursor storage directory
pub const DEFAULT_CURSOR_DIR: &str = "/var/lib/otel-journal-gatewayd-forwarder";
/// Default poll interval
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(5);
/// Default batch size
pub const DEFAULT_BATCH_SIZE: usize = 500;
/// Default per-field byte cap for journal extra fields.
pub const DEFAULT_MAX_FIELD_BYTES: usize = 8 * 1024;

/// Resolve the cursor directory from env/config precedence:
/// `OJGF_CURSOR_DIR` > config `cursor_dir` > `STATE_DIRECTORY` > default.
fn resolve_cursor_dir(
    ojgf_cursor_dir: Option<String>,
    toml_cursor_dir: Option<PathBuf>,
    state_directory: Option<String>,
) -> PathBuf {
    ojgf_cursor_dir
        .map(PathBuf::from)
        .or(toml_cursor_dir)
        .or_else(|| state_directory.map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from(DEFAULT_CURSOR_DIR))
}

#[derive(Error, Debug)]
pub enum ConfigError {
    #[error("Failed to read config file: {0}")]
    ReadFile(#[from] std::io::Error),
    #[error("Failed to parse config file: {0}")]
    ParseToml(#[from] toml::de::Error),
    #[error("Missing required field: {0}")]
    MissingField(&'static str),
    #[error("Invalid value for {field}: {message}")]
    InvalidValue {
        field: &'static str,
        message: String,
    },
    #[error("No sources configured")]
    NoSources,
}

/// CLI arguments
#[derive(Parser, Debug)]
#[command(name = "otel-journal-gatewayd-forwarder")]
#[command(about = "Pull-based journal log forwarder for systemd-journal-gatewayd")]
#[command(version)]
pub struct Cli {
    /// Config file path
    #[arg(short, long, default_value = DEFAULT_CONFIG_PATH)]
    pub config: PathBuf,

    /// Increase log verbosity (-v info, -vv debug, -vvv trace)
    #[arg(short, long, action = clap::ArgAction::Count)]
    pub verbose: u8,

    /// Suppress all output except errors
    #[arg(short, long)]
    pub quiet: bool,

    /// Validate config and exit
    #[arg(long)]
    pub validate: bool,

    /// Run one collection cycle and exit
    #[arg(long)]
    pub once: bool,

    /// Enable Prometheus metrics endpoint
    #[arg(long, value_name = "ADDR")]
    pub metrics: Option<String>,
}

/// TOML config file structure
#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct TomlConfig {
    otlp_endpoint: Option<String>,
    poll_interval: Option<String>,
    batch_size: Option<usize>,
    max_field_bytes: Option<usize>,
    cursor_dir: Option<PathBuf>,
    tls: Option<TlsConfig>,
    otlp_headers: HashMap<String, String>,
    sources: Vec<TomlSource>,
}

/// Source configuration from TOML
#[derive(Debug, Deserialize, Clone)]
struct TomlSource {
    name: String,
    url: String,
    #[serde(default)]
    units: Vec<String>,
    #[serde(default)]
    labels: HashMap<String, String>,
    tls: Option<TlsConfig>,
    #[serde(default)]
    headers: HashMap<String, String>,
}

/// Validated application configuration
#[derive(Debug, Clone)]
pub struct Config {
    pub otlp_endpoint: String,
    pub poll_interval: Duration,
    pub batch_size: usize,
    pub max_field_bytes: usize,
    pub cursor_dir: PathBuf,
    pub tls: Option<TlsConfig>,
    pub otlp_headers: HashMap<String, String>,
    pub sources: Vec<Source>,
}

/// Validated source configuration
#[derive(Debug, Clone)]
pub struct Source {
    pub name: String,
    pub url: String,
    pub units: Vec<String>,
    pub labels: HashMap<String, String>,
    pub tls: Option<TlsConfig>,
    pub headers: HashMap<String, String>,
}

impl Source {
    /// Return source-specific TLS config, falling back to the global default.
    pub fn effective_tls(&self, global: &Option<TlsConfig>) -> Option<TlsConfig> {
        self.tls.clone().or_else(|| global.clone())
    }
}

/// TLS configuration for a source or the global default.
#[derive(Debug, Deserialize, Clone, Default)]
#[serde(default)]
pub struct TlsConfig {
    pub ca_cert: Option<PathBuf>,
    pub client_cert: Option<PathBuf>,
    pub client_key: Option<PathBuf>,
}

/// Build a reqwest blocking client with TLS, identity, and default headers.
pub fn build_http_client(
    tls: Option<&TlsConfig>,
    headers: &HashMap<String, String>,
    timeout: Duration,
) -> Result<reqwest::blocking::Client, ConfigError> {
    let mut builder = reqwest::blocking::Client::builder()
        .timeout(timeout)
        .gzip(true);

    if let Some(t) = tls {
        if let Some(ca) = &t.ca_cert {
            let pem = std::fs::read_to_string(ca).map_err(|e| ConfigError::InvalidValue {
                field: "tls.ca_cert",
                message: format!("{}: {}", ca.display(), e),
            })?;
            let cert = reqwest::Certificate::from_pem(pem.as_bytes()).map_err(|e| {
                ConfigError::InvalidValue {
                    field: "tls.ca_cert",
                    message: e.to_string(),
                }
            })?;
            builder = builder.add_root_certificate(cert);
        }

        if let (Some(cert_path), Some(key_path)) = (&t.client_cert, &t.client_key) {
            let mut pem =
                std::fs::read_to_string(cert_path).map_err(|e| ConfigError::InvalidValue {
                    field: "tls.client_cert",
                    message: format!("{}: {}", cert_path.display(), e),
                })?;
            let key = std::fs::read_to_string(key_path).map_err(|e| ConfigError::InvalidValue {
                field: "tls.client_key",
                message: format!("{}: {}", key_path.display(), e),
            })?;
            pem.push_str(&key);
            let identity = reqwest::Identity::from_pem(pem.as_bytes()).map_err(|e| {
                ConfigError::InvalidValue {
                    field: "tls.client_cert",
                    message: e.to_string(),
                }
            })?;
            builder = builder.identity(identity);
        }
    }

    let mut headers_map = reqwest::header::HeaderMap::new();
    for (k, v) in headers {
        let name = reqwest::header::HeaderName::from_bytes(k.as_bytes()).map_err(|e| {
            ConfigError::InvalidValue {
                field: "headers",
                message: format!("invalid header name '{}': {}", k, e),
            }
        })?;
        let value =
            reqwest::header::HeaderValue::from_str(v).map_err(|e| ConfigError::InvalidValue {
                field: "headers",
                message: format!("invalid header value '{}': {}", v, e),
            })?;
        headers_map.insert(name, value);
    }
    if !headers_map.is_empty() {
        builder = builder.default_headers(headers_map);
    }

    builder.build().map_err(|e| ConfigError::InvalidValue {
        field: "http_client",
        message: e.to_string(),
    })
}

impl Config {
    /// Load configuration from file and environment variables
    pub fn load(path: &PathBuf) -> Result<Self, ConfigError> {
        // Try to read config file
        let toml_config = if path.exists() {
            let contents = std::fs::read_to_string(path)?;
            toml::from_str::<TomlConfig>(&contents)?
        } else if path.as_os_str() == DEFAULT_CONFIG_PATH {
            // Default path doesn't exist, use defaults
            TomlConfig::default()
        } else {
            // Explicit path doesn't exist
            return Err(ConfigError::ReadFile(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("Config file not found: {}", path.display()),
            )));
        };

        // Merge with environment variables
        let otlp_endpoint = std::env::var("OJGF_OTLP_ENDPOINT")
            .ok()
            .or(toml_config.otlp_endpoint)
            .ok_or(ConfigError::MissingField("otlp_endpoint"))?;

        let poll_interval = std::env::var("OJGF_POLL_INTERVAL")
            .ok()
            .or(toml_config.poll_interval)
            .map(|s| parse_duration(&s))
            .transpose()?
            .unwrap_or(DEFAULT_POLL_INTERVAL);

        let batch_size = std::env::var("OJGF_BATCH_SIZE")
            .ok()
            .map(|s| {
                s.parse::<usize>().map_err(|_| ConfigError::InvalidValue {
                    field: "batch_size",
                    message: "must be a positive integer".to_string(),
                })
            })
            .transpose()?
            .or(toml_config.batch_size)
            .unwrap_or(DEFAULT_BATCH_SIZE);

        let max_field_bytes = std::env::var("OJGF_MAX_FIELD_BYTES")
            .ok()
            .map(|s| {
                s.parse::<usize>().map_err(|_| ConfigError::InvalidValue {
                    field: "max_field_bytes",
                    message: "must be a positive integer".to_string(),
                })
            })
            .transpose()?
            .or(toml_config.max_field_bytes)
            .unwrap_or(DEFAULT_MAX_FIELD_BYTES);

        let cursor_dir = resolve_cursor_dir(
            std::env::var("OJGF_CURSOR_DIR").ok(),
            toml_config.cursor_dir,
            std::env::var("STATE_DIRECTORY").ok(),
        );

        let sources: Vec<Source> = toml_config
            .sources
            .into_iter()
            .map(|s| Source {
                name: s.name,
                url: s.url,
                units: s.units,
                labels: s.labels,
                tls: s.tls,
                headers: s.headers,
            })
            .collect();

        if sources.is_empty() {
            return Err(ConfigError::NoSources);
        }

        Ok(Config {
            otlp_endpoint,
            poll_interval,
            batch_size,
            max_field_bytes,
            cursor_dir,
            tls: toml_config.tls,
            otlp_headers: toml_config.otlp_headers,
            sources,
        })
    }

    fn validate_tls(tls: &Option<TlsConfig>) -> Result<(), ConfigError> {
        if let Some(t) = tls {
            let has_cert = t.client_cert.is_some();
            let has_key = t.client_key.is_some();
            if has_cert != has_key {
                return Err(ConfigError::InvalidValue {
                    field: "tls.client_cert / tls.client_key",
                    message: "client_cert and client_key must both be set or both omitted"
                        .to_string(),
                });
            }
        }
        Ok(())
    }

    /// Validate the configuration
    pub fn validate(&self) -> Result<(), ConfigError> {
        Self::validate_tls(&self.tls)?;

        // Check OTLP endpoint is valid URL
        if !self.otlp_endpoint.starts_with("http://") && !self.otlp_endpoint.starts_with("https://")
        {
            return Err(ConfigError::InvalidValue {
                field: "otlp_endpoint",
                message: "must be a valid HTTP(S) URL".to_string(),
            });
        }

        // Check sources
        for source in &self.sources {
            if source.name.is_empty() {
                return Err(ConfigError::InvalidValue {
                    field: "source.name",
                    message: "cannot be empty".to_string(),
                });
            }
            if !source.url.starts_with("http://") && !source.url.starts_with("https://") {
                return Err(ConfigError::InvalidValue {
                    field: "source.url",
                    message: format!("invalid URL for source '{}': must be HTTP(S)", source.name),
                });
            }

            Self::validate_tls(&source.tls)?;
        }

        Ok(())
    }
}

/// Parse a duration string like "5s", "10m", "1h"
fn parse_duration(s: &str) -> Result<Duration, ConfigError> {
    humantime::parse_duration(s).map_err(|e| ConfigError::InvalidValue {
        field: "poll_interval",
        message: e.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    #[test]
    fn test_parse_duration() {
        assert_eq!(parse_duration("5s").unwrap(), Duration::from_secs(5));
        assert_eq!(parse_duration("10m").unwrap(), Duration::from_secs(600));
        assert_eq!(parse_duration("1h").unwrap(), Duration::from_secs(3600));
    }

    #[test]
    fn test_load_config() {
        let config_content = r#"
otlp_endpoint = "http://localhost:4318"
poll_interval = "10s"
batch_size = 1000

[[sources]]
name = "test-host"
url = "http://localhost:19531"
"#;
        let file = NamedTempFile::new().unwrap();
        std::fs::write(file.path(), config_content).unwrap();

        let config = Config::load(&file.path().to_path_buf()).unwrap();
        assert_eq!(config.otlp_endpoint, "http://localhost:4318");
        assert_eq!(config.poll_interval, Duration::from_secs(10));
        assert_eq!(config.batch_size, 1000);
        assert_eq!(config.sources.len(), 1);
        assert_eq!(config.sources[0].name, "test-host");
    }

    #[test]
    fn test_headers_and_tls_parse() {
        let config_content = r#"
otlp_endpoint = "http://localhost:4318"
otlp_headers = { Authorization = "Bearer token" }

[tls]
ca_cert = "/etc/ca.pem"

[[sources]]
name = "secure-host"
url = "https://localhost:19531"
headers = { Authorization = "Basic dXNlcjpwYXNz" }
tls = { client_cert = "/etc/client.pem", client_key = "/etc/client.key" }
"#;
        let file = NamedTempFile::new().unwrap();
        std::fs::write(file.path(), config_content).unwrap();

        let config = Config::load(&file.path().to_path_buf()).unwrap();
        assert_eq!(
            config.otlp_headers.get("Authorization"),
            Some(&"Bearer token".to_string())
        );
        assert_eq!(
            config.tls.as_ref().unwrap().ca_cert,
            Some(PathBuf::from("/etc/ca.pem"))
        );

        let source = &config.sources[0];
        assert_eq!(
            source.headers.get("Authorization"),
            Some(&"Basic dXNlcjpwYXNz".to_string())
        );
        let tls = source.effective_tls(&config.tls).unwrap();
        assert_eq!(tls.client_cert, Some(PathBuf::from("/etc/client.pem")));
        assert_eq!(tls.client_key, Some(PathBuf::from("/etc/client.key")));
    }

    #[test]
    fn test_tls_cert_without_key_rejected() {
        let config_content = r#"
otlp_endpoint = "http://localhost:4318"

[[sources]]
name = "bad-host"
url = "https://localhost:19531"
tls = { client_cert = "/etc/client.pem" }
"#;
        let file = NamedTempFile::new().unwrap();
        std::fs::write(file.path(), config_content).unwrap();

        let config = Config::load(&file.path().to_path_buf()).unwrap();
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_resolve_cursor_dir_precedence() {
        let default = PathBuf::from(DEFAULT_CURSOR_DIR);

        assert_eq!(
            resolve_cursor_dir(
                Some("/env/cursors".to_string()),
                Some(PathBuf::from("/toml/cursors")),
                Some("/state/cursors".to_string()),
            ),
            PathBuf::from("/env/cursors")
        );

        assert_eq!(
            resolve_cursor_dir(
                None,
                Some(PathBuf::from("/toml/cursors")),
                Some("/state/cursors".to_string()),
            ),
            PathBuf::from("/toml/cursors")
        );

        assert_eq!(
            resolve_cursor_dir(None, None, Some("/state/cursors".to_string())),
            PathBuf::from("/state/cursors")
        );

        assert_eq!(resolve_cursor_dir(None, None, None), default);
    }
}
