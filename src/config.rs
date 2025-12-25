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
    cursor_dir: Option<PathBuf>,
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
}

/// Validated application configuration
#[derive(Debug, Clone)]
pub struct Config {
    pub otlp_endpoint: String,
    pub poll_interval: Duration,
    pub batch_size: usize,
    pub cursor_dir: PathBuf,
    pub sources: Vec<Source>,
}

/// Validated source configuration
#[derive(Debug, Clone)]
pub struct Source {
    pub name: String,
    pub url: String,
    pub units: Vec<String>,
    pub labels: HashMap<String, String>,
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

        let cursor_dir = std::env::var("OJGF_CURSOR_DIR")
            .ok()
            .map(PathBuf::from)
            .or(toml_config.cursor_dir)
            .unwrap_or_else(|| PathBuf::from(DEFAULT_CURSOR_DIR));

        let sources: Vec<Source> = toml_config
            .sources
            .into_iter()
            .map(|s| Source {
                name: s.name,
                url: s.url,
                units: s.units,
                labels: s.labels,
            })
            .collect();

        if sources.is_empty() {
            return Err(ConfigError::NoSources);
        }

        Ok(Config {
            otlp_endpoint,
            poll_interval,
            batch_size,
            cursor_dir,
            sources,
        })
    }

    /// Validate the configuration
    pub fn validate(&self) -> Result<(), ConfigError> {
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
}
