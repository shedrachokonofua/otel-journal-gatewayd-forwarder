#!/bin/bash
# E2E Test Script
# Verifies the forwarder correctly collects journal logs and sends them to OTLP.
#
# Two phases:
#   1. One-shot: forwarder runs --once against systemd-managed gatewayd +
#      otel-collector, verifies basic log forwarding and structure.
#   2. Continuation: synthetic journal files served via socket-activated
#      gatewayd on a separate port, forwarder runs continuously across
#      multiple cursor cycles and a process restart to verify cursor
#      continuation, drain loop, and duplicate suppression.

set -euo pipefail

LOGS_FILE="/test-results/logs.json"
EXPECTED_LOGS=3

BIN=/usr/local/bin/otel-journal-gatewayd-forwarder
E2E_DIR=/app/e2e

echo "=========================================="
echo "E2E Test: otel-journal-gatewayd-forwarder"
echo "=========================================="

# -------------------------------------------------------------------------
# Phase 1: One-shot basic forwarding
# -------------------------------------------------------------------------

echo ""
echo "=== Phase 1: One-shot forwarding ==="

# Wait for services to be ready
echo "[phase1] Waiting for services to start..."
sleep 3

# Check journal-gatewayd is responding (socket-activated via systemd)
echo "[phase1] Checking journal-gatewayd on port 19531..."
for i in $(seq 1 30); do
    RESP=$(curl -sf "http://127.0.0.1:19531/entries?boot" -H "Accept: application/json" -H "Range: entries=:1" 2>/dev/null || echo "")
    if [ -n "$RESP" ] && [ "$RESP" != "Failed to parse Range header." ]; then
        echo "[phase1] journal-gatewayd is ready"
        break
    fi
    if [ "$i" -eq 30 ]; then
        echo "[phase1] FAIL: journal-gatewayd not responding after 30 attempts"
        echo "[phase1] Last response: $RESP"
        systemctl status systemd-journal-gatewayd.socket || true
        systemctl status systemd-journal-gatewayd.service || true
        ss -tlnp | grep 19531 || echo "Port 19531 not listening"
        exit 1
    fi
    echo "[phase1] Waiting for gatewayd... ($i/30)"
    sleep 1
done

# Check otel-collector is responding
echo "[phase1] Checking otel-collector..."
for i in $(seq 1 20); do
    if curl -sf -X POST "http://127.0.0.1:4318/v1/logs" \
        -H "Content-Type: application/json" \
        -d '{"resourceLogs":[]}' >/dev/null 2>&1; then
        echo "[phase1] otel-collector is ready"
        break
    fi
    if [ "$i" -eq 20 ]; then
        echo "[phase1] FAIL: otel-collector not responding"
        systemctl status otel-collector.service || true
        exit 1
    fi
    echo "[phase1] Waiting for otel-collector... ($i/20)"
    sleep 1
done

# Let the log generator create some entries
echo "[phase1] Waiting for test logs to be generated..."
sleep 5

# Show some journal entries for debugging
echo "[phase1] Sample journal entries from gatewayd:"
curl -s "http://127.0.0.1:19531/entries?boot" -H "Accept: application/json" -H "Range: entries=:3" | jq -r '.MESSAGE // empty' 2>/dev/null | head -5 || true
echo ""

# Run the forwarder once.
# Output goes to a file, not stdout: via e2e-test.service stdout is journald,
# and anything we print lands back in the journal the forwarder is draining
# (feedback loop -> --once never exits). It also keeps the CI job log small.
# The 120s timeout turns any future drain runaway into a fast failure instead
# of a 1h job timeout.
echo "[phase1] Running forwarder..."
PHASE1_LOG=/tmp/phase1-forwarder.log
timeout 120 "$BIN" -c "$E2E_DIR/config.toml" --once -vv >"$PHASE1_LOG" 2>&1 || {
    RC=$?
    echo "[phase1] FAIL: Forwarder failed with exit code $RC (124 = drain did not finish in 120s)"
    tail -n 100 "$PHASE1_LOG"
    exit 1
}

# Give collector time to flush to file
echo "[phase1] Waiting for collector to flush..."
sleep 3

# Verify logs were received
echo "[phase1] Verifying received logs..."

if [ ! -f "$LOGS_FILE" ]; then
    echo "[phase1] FAIL: No logs file created at $LOGS_FILE"
    ls -la /test-results/ || true
    exit 1
fi

echo "[phase1] Logs file exists, size: $(stat -c%s "$LOGS_FILE") bytes"

# Count log records in the file (each line is a separate OTLP export)
LOG_COUNT=$(jq -s '[.[].resourceLogs[]?.scopeLogs[]?.logRecords[]?] | length' "$LOGS_FILE" 2>/dev/null || echo "0")

echo "[phase1] Received $LOG_COUNT log records"

if [ "$LOG_COUNT" -lt "$EXPECTED_LOGS" ]; then
    echo "[phase1] FAIL: Expected at least $EXPECTED_LOGS logs, got $LOG_COUNT"
    echo "[phase1] File contents:"
    head -c 2000 "$LOGS_FILE"
    exit 1
fi

# Verify log content structure
echo "[phase1] Verifying log structure..."

HAS_HOST=$(jq -s 'any(.[].resourceLogs[]?.resource?.attributes[]?; .key == "host.name")' "$LOGS_FILE" 2>/dev/null || echo "false")
HAS_SERVICE=$(jq -s 'any(.[].resourceLogs[]?.resource?.attributes[]?; .key == "service.name")' "$LOGS_FILE" 2>/dev/null || echo "false")

if [ "$HAS_HOST" != "true" ]; then
    echo "[phase1] FAIL: Missing host.name attribute"
    exit 1
fi

if [ "$HAS_SERVICE" != "true" ]; then
    echo "[phase1] FAIL: Missing service.name attribute"
    exit 1
fi

echo "[phase1] PASS: Basic forwarding verified ($LOG_COUNT records)"

# -------------------------------------------------------------------------
# Phase 2: Continuation (cursor + drain + restart)
# -------------------------------------------------------------------------

echo ""
echo "=== Phase 2: Continuation (cursor, drain, restart) ==="

# Use separate ports to avoid conflicting with phase 1 services.
GW_PORT=19532
SINK_PORT=4319

JR=$(command -v systemd-journal-remote || echo /lib/systemd/systemd-journal-remote)
GW=/lib/systemd/systemd-journal-gatewayd
[ -x "$GW" ] || GW=/usr/lib/systemd/systemd-journal-gatewayd

BOOT_ID=$(tr -d '-' </proc/sys/kernel/random/boot_id)
CONT_JOURNAL_DIR="/tmp/ojgf-cont-journal"
CONT_CURSOR_DIR="/tmp/ojgf-cont-cursors"
CONT_SINK_DIR="/tmp/ojgf-cont-sink"
rm -rf "$CONT_JOURNAL_DIR" "$CONT_CURSOR_DIR" "$CONT_SINK_DIR"
mkdir -p "$CONT_JOURNAL_DIR" "$CONT_CURSOR_DIR" "$CONT_SINK_DIR"

START_TS=$(date +%s%N)

# Generate journal-export entries with strictly increasing monotonic timestamps.
# __MONOTONIC_TIMESTAMP is mandatory for stable ordering across journal files;
# omitting it (m=0 everywhere) makes gatewayd interleave unpredictably.
gen_batch() {
    local first="$1" last="$2"
    local i
    for i in $(seq "$first" "$last"); do
        local rt=$((START_TS / 1000 + i))
        local mt=$((1000000 + i * 100000))
        printf '__REALTIME_TIMESTAMP=%s\n__MONOTONIC_TIMESTAMP=%s\n_BOOT_ID=%s\n_HOSTNAME=testhost\n_SYSTEMD_UNIT=test.service\nPRIORITY=6\nMESSAGE=test message %04d\n\n' \
            "$rt" "$mt" "$BOOT_ID" "$i"
    done
}

start_gatewayd() {
    pkill -f "systemd-journal-gatewayd.*$GW_PORT" 2>/dev/null || true
    pkill -f "systemd-socket-activate.*$GW_PORT" 2>/dev/null || true
    sleep 0.5
    systemd-socket-activate -l "$GW_PORT" "$GW" -D "$CONT_JOURNAL_DIR" >>/tmp/cont-gatewayd.log 2>&1 &
    sleep 1
}

# Start a minimal OTLP/HTTP sink that writes each POST body to a numbered file.
python3 - "$CONT_SINK_DIR" "$SINK_PORT" >/tmp/cont-sink.log 2>&1 <<'PY' &
import http.server, sys
sink_dir = sys.argv[1]
port = int(sys.argv[2])
n = 0
class H(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        global n
        n += 1
        body = self.rfile.read(int(self.headers.get('Content-Length', 0)))
        with open(f'{sink_dir}/req{n:03d}.json', 'wb') as f:
            f.write(body)
        self.send_response(200)
        self.send_header('Content-Type', 'application/json')
        self.end_headers()
        self.wfile.write(b'{}')
    def log_message(self, *a): pass
http.server.HTTPServer(('127.0.0.1', port), H).serve_forever()
PY

# Wait for the python sink to be ready.
for i in $(seq 1 10); do
    if curl -sf -X POST "http://127.0.0.1:${SINK_PORT}/v1/logs" \
        -H "Content-Type: application/json" \
        -d '{"resourceLogs":[]}' >/dev/null 2>&1; then
        echo "[phase2] OTLP sink is ready on port ${SINK_PORT}"
        break
    fi
    if [ "$i" -eq 10 ]; then
        echo "[phase2] FAIL: OTLP sink not responding on port ${SINK_PORT}"
        cat /tmp/cont-sink.log
        exit 1
    fi
    sleep 0.5
done

cat > /tmp/ojgf-cont-config.toml <<EOF
otlp_endpoint = "http://127.0.0.1:${SINK_PORT}"
batch_size = 10
# Use a long poll interval to prove the drain loop (not the poll interval)
# consumes the backlog. Without draining, 240 entries would take > 4 minutes.
poll_interval = "30s"
cursor_dir = "$CONT_CURSOR_DIR"
[[sources]]
name = "testhost"
url = "http://127.0.0.1:${GW_PORT}"
EOF

PHASE1_FIRST=1
PHASE1_LAST=120
PHASE2_FIRST=121
PHASE2_LAST=240

run_phase() {
    local first="$1" last="$2" log="$3"
    local pid ready=false

    "$BIN" -c /tmp/ojgf-cont-config.toml -v >"$log" 2>&1 &
    pid=$!

    for i in $(seq 1 60); do
        sleep 1
        if python3 "$E2E_DIR/verify_continuation.py" "$CONT_SINK_DIR" "$first" "$last" >/dev/null 2>&1; then
            ready=true
            break
        fi
    done

    kill "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true

    if [ "$ready" != "true" ]; then
        return 1
    fi
    return 0
}

echo "[phase2] Generating $PHASE1_FIRST-$PHASE1_LAST synthetic entries..."
gen_batch "$PHASE1_FIRST" "$PHASE1_LAST" | "$JR" --split-mode=none -o "$CONT_JOURNAL_DIR/batch1.journal" - 2>/dev/null
start_gatewayd

echo "[phase2] Running forwarder (phase 1 of continuation)..."
PHASE2_1_START=$(date +%s)
if ! run_phase "$PHASE1_FIRST" "$PHASE1_LAST" /tmp/cont-run1.log; then
    echo "[phase2] FAIL: did not reach expected record count in 60s"
    cat /tmp/cont-run1.log
    exit 1
fi
PHASE2_1_ELAPSED=$(($(date +%s) - PHASE2_1_START))
echo "[phase2] Phase 1 elapsed: ${PHASE2_1_ELAPSED}s (must be < 10s with 30s poll interval to prove draining)"
if [ "$PHASE2_1_ELAPSED" -ge 10 ]; then
    echo "[phase2] FAIL: drain did not keep up; expected < 10s"
    exit 1
fi

RUN1_CYCLES=$(grep -c 'Forwarded entries' /tmp/cont-run1.log || true)
echo "[phase2] Push cycles: $RUN1_CYCLES"
echo "[phase2] Cursor after phase 1: $(cat "$CONT_CURSOR_DIR/testhost.cursor")"
python3 "$E2E_DIR/verify_continuation.py" "$CONT_SINK_DIR" "$PHASE1_FIRST" "$PHASE1_LAST"

echo ""
echo "[phase2] Generating $PHASE2_FIRST-$PHASE2_LAST additional entries..."
gen_batch "$PHASE2_FIRST" "$PHASE2_LAST" | "$JR" --split-mode=none -o "$CONT_JOURNAL_DIR/batch2.journal" - 2>/dev/null
echo "[phase2] Restarting gatewayd with updated journal..."
start_gatewayd

echo "[phase2] Running fresh forwarder process (phase 2 of continuation)..."
if ! run_phase "$PHASE1_FIRST" "$PHASE2_LAST" /tmp/cont-run2.log; then
    echo "[phase2] FAIL: did not reach expected record count in 60s"
    cat /tmp/cont-run2.log
    exit 1
fi

RUN2_CYCLES=$(grep -c 'Forwarded entries' /tmp/cont-run2.log || true)
echo "[phase2] Push cycles: $RUN2_CYCLES"
echo "[phase2] Cursor after phase 2: $(cat "$CONT_CURSOR_DIR/testhost.cursor")"
python3 "$E2E_DIR/verify_continuation.py" "$CONT_SINK_DIR" "$PHASE1_FIRST" "$PHASE2_LAST"

# Clean up background processes.
pkill -f "python3.*$CONT_SINK_DIR" 2>/dev/null || true
pkill -f "systemd-socket-activate.*$GW_PORT" 2>/dev/null || true

echo ""
echo "=========================================="
echo "E2E Test: ALL PASSED"
echo "  Phase 1 (one-shot):  $LOG_COUNT records, structure validated"
echo "  Phase 2 (continuation):"
echo "    phase 1 cycles: $RUN1_CYCLES"
echo "    phase 2 cycles: $RUN2_CYCLES"
echo "    total records: 240, no duplicates, no gaps"
echo "=========================================="

exit 0