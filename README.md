# otel-journal-gatewayd-forwarder

Pull-based journal log forwarder. Collects logs from remote `systemd-journal-gatewayd` endpoints and forwards them to an OTLP-compatible backend.

## Install

### From GitHub Releases

Download the latest binary from the [GitHub Releases](https://github.com/shedrachokonofua/otel-journal-gatewayd-forwarder/releases) page.

```bash
# Example for Linux AMD64
curl -L -O https://github.com/shedrachokonofua/otel-journal-gatewayd-forwarder/releases/latest/download/otel-journal-gatewayd-forwarder-linux-amd64
chmod +x otel-journal-gatewayd-forwarder-linux-amd64
sudo mv otel-journal-gatewayd-forwarder-linux-amd64 /usr/local/bin/otel-journal-gatewayd-forwarder
```

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

| Variable             | Description                     |
| -------------------- | ------------------------------- |
| `OJGF_OTLP_ENDPOINT` | OTLP HTTP endpoint              |
| `OJGF_POLL_INTERVAL` | Poll interval (e.g. `5s`, `1m`) |
| `OJGF_BATCH_SIZE`    | Max entries per request         |
| `OJGF_CURSOR_DIR`    | Cursor storage directory        |

### Configuration File

The configuration file (`config.toml`) uses TOML format.

**Global Options:**

- `otlp_endpoint`: OTLP/HTTP receiver URL (required).
- `poll_interval`: Time between collection cycles (default: `5s`).
- `batch_size`: Max entries per request (default: `500`).
- `cursor_dir`: Directory for cursor state (default: `/var/lib/otel-journal-gatewayd-forwarder`).

**Sources:**
Define one or more `[[sources]]` blocks:

- `name`: Source identifier (sets `host.name`).
- `url`: `systemd-journal-gatewayd` endpoint URL.
- `units`: (Optional) List of systemd units to collect.
- `labels`: (Optional) Custom resource attributes.

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

Create a systemd service file at `/etc/systemd/system/otel-journal-gatewayd-forwarder.service`:

```ini
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

Then enable and start the service:

```bash
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

| Attribute      | Source                      |
| -------------- | --------------------------- |
| `host.name`    | Source name from config     |
| `service.name` | `_SYSTEMD_UNIT` field       |
| `os.type`      | `linux`                     |
| Custom         | `labels` from source config |

### Severity mapping

| Journal PRIORITY  | OTLP       |
| ----------------- | ---------- |
| 0-1 (emerg/alert) | FATAL (21) |
| 2-3 (crit/err)    | ERROR (17) |
| 4 (warning)       | WARN (13)  |
| 5-6 (notice/info) | INFO (9)   |
| 7 (debug)         | DEBUG (5)  |

## Cursor management

Cursors are stored as `{cursor_dir}/{source_name}.cursor`. Updated atomically after successful OTLP push.

On invalid cursor (410 Gone), collection resets to current boot.

## E2E Testing

The project includes an end-to-end testing suite that runs in a containerized environment.

### Strategy

The E2E test validates the full pipeline:

1. **Environment**: A systemd-enabled container (Podman) starts `systemd-journal-gatewayd`, an `otel-collector` (writing to file), and a log generator.
2. **Execution**: The forwarder runs in one-shot mode (`--once`) against the local gateway.
3. **Verification**: The test script asserts that:
   - Logs are successfully collected from the gateway.
   - Logs are successfully sent to the collector.
   - The received logs contain expected resource attributes (`host.name`, `service.name`).

### Running Tests

```bash
# Run locally (requires Podman)
./e2e/run-in-container.sh
```

## License

MIT OR Apache-2.0
