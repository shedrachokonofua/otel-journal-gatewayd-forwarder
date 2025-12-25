# otel-journal-gatewayd-forwarder

Pull-based journal log forwarder. Collects logs from remote `systemd-journal-gatewayd` endpoints and forwards them to an OTLP-compatible backend.

## Install

### From source

```bash
cargo build --release
sudo cp target/release/otel-journal-gatewayd-forwarder /usr/local/bin/
```

For static builds:

```bash
rustup target add x86_64-unknown-linux-musl
cargo build --release --target x86_64-unknown-linux-musl
```

## Configure

Copy and edit the example config:

```bash
sudo mkdir -p /etc/otel-journal-gatewayd-forwarder
sudo cp config.example.toml /etc/otel-journal-gatewayd-forwarder/config.toml
sudo mkdir -p /var/lib/otel-journal-gatewayd-forwarder
```

See `config.example.toml` for all options.

Environment variables override config file values:

| Variable | Description |
|----------|-------------|
| `OJGF_OTLP_ENDPOINT` | OTLP HTTP endpoint |
| `OJGF_POLL_INTERVAL` | Poll interval (e.g. `5s`, `1m`) |
| `OJGF_BATCH_SIZE` | Max entries per request |
| `OJGF_CURSOR_DIR` | Cursor storage directory |

## Run

```bash
# Foreground
otel-journal-gatewayd-forwarder -c /etc/otel-journal-gatewayd-forwarder/config.toml

# Validate config
otel-journal-gatewayd-forwarder --validate

# Single collection cycle
otel-journal-gatewayd-forwarder --once

# With metrics endpoint
otel-journal-gatewayd-forwarder --metrics 0.0.0.0:9091
```

See `--help` for all options.

### Systemd

```bash
sudo cp otel-journal-gatewayd-forwarder.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now otel-journal-gatewayd-forwarder
```

## Host setup

Enable journal gateway on hosts you want to collect from:

```bash
# Debian/Ubuntu
sudo apt install systemd-journal-gateway
sudo systemctl enable --now systemd-journal-gatewayd.socket

# Fedora/RHEL
sudo dnf install systemd-journal-gateway
sudo systemctl enable --now systemd-journal-gatewayd.socket

# Amazon Linux 2023
sudo dnf install systemd-journal-remote
sudo systemctl enable --now systemd-journal-gatewayd.socket
```

Default port: 19531

## OTLP output

Logs are sent to `{otlp_endpoint}/v1/logs` as OTLP/HTTP JSON.

### Resource attributes

| Attribute | Source |
|-----------|--------|
| `host.name` | Source name from config |
| `service.name` | `_SYSTEMD_UNIT` field |
| `os.type` | `linux` |
| Custom | `labels` from source config |

### Severity mapping

| Journal PRIORITY | OTLP |
|------------------|------|
| 0-1 (emerg/alert) | FATAL (21) |
| 2-3 (crit/err) | ERROR (17) |
| 4 (warning) | WARN (13) |
| 5-6 (notice/info) | INFO (9) |
| 7 (debug) | DEBUG (5) |

## Cursor management

Cursors are stored as `{cursor_dir}/{source_name}.cursor`. Updated atomically after successful OTLP push.

On invalid cursor (410 Gone), collection resets to current boot.

## License

MIT OR Apache-2.0
