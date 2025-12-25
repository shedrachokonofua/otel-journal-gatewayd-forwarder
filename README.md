# otel-journal-gatewayd-forwarder

Pull-based journal log forwarder. Collects logs from remote `systemd-journal-gatewayd` endpoints and forwards them to an OTLP-compatible backend.

## Why

Infrastructure hosts (hypervisors, bare-metal servers) should be passive. They expose data; they don't push it. This tool enables pull-based log collection that matches the Prometheus model for metrics:

```
┌──────────────────┐         ┌─────────────────────┐
│  Monitoring VM   │         │   Infrastructure    │
│                  │ pulls   │                     │
│  forwarder ──────┼────────▶│ :19531 (gatewayd)   │
│      │           │         │ :9100  (node_exp)   │
│      ▼           │         │ :9633  (smart_exp)  │
│  OTLP endpoint   │         │                     │
└──────────────────┘         └─────────────────────┘
```

No agents on hosts. Hosts just serve journal entries over HTTP when asked.

## Features

- **Pull-based** — Forwarder reaches out to hosts, not the other way around
- **Cursor management** — Crash-safe resume, no duplicates, no gaps
- **Multi-source** — Collect from many hosts concurrently
- **OTLP native** — Forwards to any OTLP-compatible backend (Loki, Grafana Cloud, Datadog, etc.)
- **Single binary** — Static build, no runtime dependencies
- **Minimal** — ~3MB binary, ~5MB RSS

## Installation

### Binary releases

```bash
curl -LO https://github.com/yourname/otel-journal-gatewayd-forwarder/releases/latest/download/otel-journal-gatewayd-forwarder-linux-amd64
chmod +x otel-journal-gatewayd-forwarder-linux-amd64
sudo mv otel-journal-gatewayd-forwarder-linux-amd64 /usr/local/bin/otel-journal-gatewayd-forwarder
```

### From source

```bash
cargo build --release
# For static musl build:
cargo build --release --target x86_64-unknown-linux-musl
```

## Configuration

### Config file

```toml
# /etc/otel-journal-gatewayd-forwarder/config.toml

# OTLP endpoint (required)
# Supports OTLP/HTTP with JSON encoding
otlp_endpoint = "http://localhost:4318"

# Poll interval (default: 5s)
poll_interval = "5s"

# Max entries per request (default: 500)
batch_size = 500

# Cursor storage directory (default: /var/lib/otel-journal-gatewayd-forwarder)
cursor_dir = "/var/lib/otel-journal-gatewayd-forwarder"

# Sources to collect from
[[sources]]
name = "host-01"
url = "http://192.168.1.10:19531"

[[sources]]
name = "host-02"
url = "http://192.168.1.11:19531"
# Optional: only collect specific units
units = ["sshd.service", "docker.service"]

[[sources]]
name = "host-03"
url = "https://host-03.internal:19531"
# Optional: custom labels added to all logs from this source
labels = { datacenter = "us-east-1", role = "gateway" }
```

### Environment variables

All config options can be set via environment variables:

```bash
OJGF_OTLP_ENDPOINT=http://localhost:4318
OJGF_POLL_INTERVAL=10s
OJGF_BATCH_SIZE=1000
OJGF_CURSOR_DIR=/tmp/cursors
```

## CLI

```
otel-journal-gatewayd-forwarder [OPTIONS]

OPTIONS:
    -c, --config <PATH>      Config file [default: /etc/otel-journal-gatewayd-forwarder/config.toml]
    -v, --verbose            Increase log verbosity (-v info, -vv debug, -vvv trace)
    -q, --quiet              Suppress all output except errors
        --validate           Validate config and exit
        --once               Run one collection cycle and exit
        --metrics <ADDR>     Enable Prometheus metrics endpoint [e.g., 0.0.0.0:9091]
    -h, --help               Print help
    -V, --version            Print version
```

## Systemd service

```ini
# /etc/systemd/system/otel-journal-gatewayd-forwarder.service
[Unit]
Description=OTLP Journal Gatewayd Forwarder
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart=/usr/local/bin/otel-journal-gatewayd-forwarder -c /etc/otel-journal-gatewayd-forwarder/config.toml
Restart=always
RestartSec=5

# Hardening
NoNewPrivileges=yes
ProtectSystem=strict
ProtectHome=yes
ReadWritePaths=/var/lib/otel-journal-gatewayd-forwarder

[Install]
WantedBy=multi-user.target
```

## Host setup

On each host you want to collect from, enable the journal gateway:

### Debian/Ubuntu/Proxmox

```bash
sudo apt install systemd-journal-gateway
sudo systemctl enable --now systemd-journal-gatewayd.socket
```

### Amazon Linux 2023

```bash
sudo dnf install systemd-journal-remote
sudo systemctl enable --now systemd-journal-gatewayd.socket
```

### Fedora/RHEL

```bash
sudo dnf install systemd-journal-gateway
sudo systemctl enable --now systemd-journal-gatewayd.socket
```

The gateway listens on port 19531 by default.

## OTLP output

Logs are forwarded as OTLP LogRecords via HTTP/JSON to `{otlp_endpoint}/v1/logs`.

### Resource attributes

| Attribute | Source |
|-----------|--------|
| `host.name` | Source name from config |
| `service.name` | `_SYSTEMD_UNIT` field |
| `os.type` | `linux` |
| Custom labels | `labels` from source config |

### Log record fields

| OTLP Field | Journal Source |
|------------|----------------|
| `time_unix_nano` | `__REALTIME_TIMESTAMP` × 1000 |
| `body` | `MESSAGE` |
| `severity_number` | Mapped from `PRIORITY` |
| `severity_text` | Mapped from `PRIORITY` |
| `attributes` | Additional journal fields |

### Severity mapping

| Journal PRIORITY | OTLP Severity |
|------------------|---------------|
| 0 (emerg) | 21 (FATAL) |
| 1 (alert) | 21 (FATAL) |
| 2 (crit) | 17 (ERROR) |
| 3 (err) | 17 (ERROR) |
| 4 (warning) | 13 (WARN) |
| 5 (notice) | 9 (INFO) |
| 6 (info) | 9 (INFO) |
| 7 (debug) | 5 (DEBUG) |

## Cursor management

Cursors track the last successfully forwarded entry per source.

- Stored as plain text files: `{cursor_dir}/{source_name}.cursor`
- Updated atomically (write to `.tmp`, rename)
- Only advanced after successful OTLP push
- On missing cursor, collection starts from current boot

### Cursor recovery

If a cursor becomes invalid (host rebooted, journal rotated), the forwarder:

1. Detects 410 Gone or invalid cursor response
2. Logs a warning
3. Resets to current boot (`?boot`)
4. Continues normally

## Metrics

When `--metrics` is enabled, Prometheus metrics are exposed:

```
# HELP ojgf_entries_forwarded_total Total journal entries forwarded
# TYPE ojgf_entries_forwarded_total counter
ojgf_entries_forwarded_total{source="host-01"} 12345

# HELP ojgf_poll_errors_total Total poll errors
# TYPE ojgf_poll_errors_total counter
ojgf_poll_errors_total{source="host-01",error="timeout"} 2

# HELP ojgf_last_poll_timestamp_seconds Timestamp of last successful poll
# TYPE ojgf_last_poll_timestamp_seconds gauge
ojgf_last_poll_timestamp_seconds{source="host-01"} 1703456789

# HELP ojgf_poll_duration_seconds Duration of last poll cycle
# TYPE ojgf_poll_duration_seconds gauge
ojgf_poll_duration_seconds{source="host-01"} 0.234
```

## Error handling

| Scenario | Behavior |
|----------|----------|
| Source unreachable | Log warning, skip source, retry next cycle |
| OTLP push fails | Log error, **do not advance cursor**, retry next cycle |
| Invalid cursor (410) | Log warning, reset to `?boot`, continue |
| Malformed entry | Log warning, skip entry, continue batch |
| Config error | Exit with error message |

## Journal gatewayd reference

This tool consumes the [systemd-journal-gatewayd](https://www.freedesktop.org/software/systemd/man/latest/systemd-journal-gatewayd.service.html) HTTP API:

```bash
# Basic query
curl http://host:19531/entries

# JSON format
curl -H "Accept: application/json" http://host:19531/entries

# With cursor (resume)
curl "http://host:19531/entries?cursor=s=abc...&skip=1"

# Limit entries
curl -H "Range: entries=:500" http://host:19531/entries

# Filter by unit
curl "http://host:19531/entries?_SYSTEMD_UNIT=sshd.service"

# Current boot only
curl "http://host:19531/entries?boot"
```

## Architecture

```
┌─────────────────────────────────────────────────────────────┐
│                         Forwarder                           │
│                                                             │
│  ┌─────────────┐    ┌─────────────┐    ┌─────────────┐     │
│  │  Source 1   │    │  Source 2   │    │  Source N   │     │
│  │  Collector  │    │  Collector  │    │  Collector  │     │
│  └──────┬──────┘    └──────┬──────┘    └──────┬──────┘     │
│         │                  │                  │             │
│         └─────────────┬────┴────┬─────────────┘             │
│                       │         │                           │
│                       ▼         ▼                           │
│               ┌───────────────────────┐                     │
│               │     OTLP Batcher      │                     │
│               └───────────┬───────────┘                     │
│                           │                                 │
│                           ▼                                 │
│               ┌───────────────────────┐                     │
│               │   Cursor Persistence  │                     │
│               └───────────────────────┘                     │
└─────────────────────────────────────────────────────────────┘
```

Each source collector runs on its own thread. Entries are batched per source, forwarded, then cursor is persisted.

## Building

```bash
# Debug build
cargo build

# Release build
cargo build --release

# Static musl build (recommended for deployment)
rustup target add x86_64-unknown-linux-musl
cargo build --release --target x86_64-unknown-linux-musl

# Run tests
cargo test
```

## License

MIT OR Apache-2.0

## Contributing

Issues and PRs welcome. Keep it simple.

---

**Questions?** Open an issue. **Using this in production?** I'd love to hear about it.

