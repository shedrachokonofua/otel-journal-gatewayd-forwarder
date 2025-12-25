#!/bin/bash
# E2E Test Script
# Verifies the forwarder correctly collects journal logs and sends them to OTLP

set -euo pipefail

LOGS_FILE="/test-results/logs.json"
EXPECTED_LOGS=3

echo "=========================================="
echo "E2E Test: otel-journal-gatewayd-forwarder"
echo "=========================================="

# Wait for services to be ready
echo "[test] Waiting for services to start..."
sleep 3

# Check journal-gatewayd is responding (socket-activated via systemd)
echo "[test] Checking journal-gatewayd on port 19531..."
for i in {1..30}; do
    # Try to connect and get at least one entry
    RESP=$(curl -sf "http://127.0.0.1:19531/entries?boot" -H "Accept: application/json" -H "Range: entries=:1" 2>/dev/null || echo "")
    if [ -n "$RESP" ] && [ "$RESP" != "Failed to parse Range header." ]; then
        echo "[test] journal-gatewayd is ready"
        break
    fi
    if [ $i -eq 30 ]; then
        echo "[test] FAIL: journal-gatewayd not responding after 30 attempts"
        echo "[test] Last response: $RESP"
        systemctl status systemd-journal-gatewayd.socket || true
        systemctl status systemd-journal-gatewayd.service || true
        ss -tlnp | grep 19531 || echo "Port 19531 not listening"
        exit 1
    fi
    echo "[test] Waiting for gatewayd... ($i/30)"
    sleep 1
done

# Check otel-collector is responding
echo "[test] Checking otel-collector..."
for i in {1..20}; do
    if curl -sf -X POST "http://127.0.0.1:4318/v1/logs" \
        -H "Content-Type: application/json" \
        -d '{"resourceLogs":[]}' >/dev/null 2>&1; then
        echo "[test] otel-collector is ready"
        break
    fi
    if [ $i -eq 20 ]; then
        echo "[test] FAIL: otel-collector not responding"
        systemctl status otel-collector.service || true
        exit 1
    fi
    echo "[test] Waiting for otel-collector... ($i/20)"
    sleep 1
done

# Let the log generator create some entries
echo "[test] Waiting for test logs to be generated..."
sleep 5

# Show some journal entries for debugging
echo "[test] Sample journal entries from gatewayd:"
curl -s "http://127.0.0.1:19531/entries?boot" -H "Accept: application/json" -H "Range: entries=:3" | jq -r '.MESSAGE // empty' 2>/dev/null | head -5 || true
echo ""

# Run the forwarder once
echo "[test] Running forwarder..."
/usr/local/bin/otel-journal-gatewayd-forwarder \
    -c /app/e2e/config.toml \
    --once \
    -vv || {
        echo "[test] Forwarder failed with exit code $?"
        exit 1
    }

# Give collector time to flush to file
echo "[test] Waiting for collector to flush..."
sleep 3

# Verify logs were received
echo "[test] Verifying received logs..."

if [ ! -f "$LOGS_FILE" ]; then
    echo "[test] FAIL: No logs file created at $LOGS_FILE"
    ls -la /test-results/ || true
    exit 1
fi

echo "[test] Logs file exists, size: $(stat -c%s "$LOGS_FILE") bytes"

# Count log records in the file (each line is a separate OTLP export)
LOG_COUNT=$(jq -s '[.[].resourceLogs[]?.scopeLogs[]?.logRecords[]?] | length' "$LOGS_FILE" 2>/dev/null || echo "0")

echo "[test] Received $LOG_COUNT log records"

if [ "$LOG_COUNT" -lt "$EXPECTED_LOGS" ]; then
    echo "[test] FAIL: Expected at least $EXPECTED_LOGS logs, got $LOG_COUNT"
    echo "[test] File contents:"
    head -c 2000 "$LOGS_FILE"
    exit 1
fi

# Verify log content structure
echo "[test] Verifying log structure..."

# Check for expected attributes
HAS_HOST=$(jq -s 'any(.[].resourceLogs[]?.resource?.attributes[]?; .key == "host.name")' "$LOGS_FILE" 2>/dev/null || echo "false")
HAS_SERVICE=$(jq -s 'any(.[].resourceLogs[]?.resource?.attributes[]?; .key == "service.name")' "$LOGS_FILE" 2>/dev/null || echo "false")

if [ "$HAS_HOST" != "true" ]; then
    echo "[test] FAIL: Missing host.name attribute"
    exit 1
fi

if [ "$HAS_SERVICE" != "true" ]; then
    echo "[test] FAIL: Missing service.name attribute"
    exit 1
fi

echo "=========================================="
echo "[test] PASS: All tests passed!"
echo "  - Received $LOG_COUNT log records"
echo "  - Log structure validated"
echo "=========================================="

exit 0
