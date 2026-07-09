#!/usr/bin/env bash
# Continuation E2E test
#
# Exercises the cursor/Range-header path across multiple batches and process
# restarts. Runs flat inside a Debian-based Rust image (e.g. rust:1.96.1-bookworm)
# with apt-provided systemd-journal-gatewayd.
#
# Usage (from repo root):
#   docker run --rm -v "$PWD:/src" -w /src rust:1.96.1-bookworm bash e2e/continuation-test.sh

set -euo pipefail
export DEBIAN_FRONTEND=noninteractive

RESULTS_DIR="/test-results"
mkdir -p "$RESULTS_DIR"

echo "### Installing system dependencies"
apt-get update -qq >/dev/null
apt-get install -y -qq systemd systemd-journal-remote curl jq python3 procps >/dev/null 2>&1

JR=$(command -v systemd-journal-remote || echo /lib/systemd/systemd-journal-remote)
GW=/lib/systemd/systemd-journal-gatewayd
[ -x "$GW" ] || GW=/usr/lib/systemd/systemd-journal-gatewayd

BOOT_ID=$(tr -d '-' </proc/sys/kernel/random/boot_id)
JOURNAL_DIR="/tmp/ojgf-journal"
CURSOR_DIR="/tmp/ojgf-cursors"
SINK_DIR="/tmp/ojgf-sink"
rm -rf "$JOURNAL_DIR" "$CURSOR_DIR" "$SINK_DIR"
mkdir -p "$JOURNAL_DIR" "$CURSOR_DIR" "$SINK_DIR"

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
    pkill -f systemd-journal-gatewayd 2>/dev/null || true
    pkill -f systemd-socket-activate 2>/dev/null || true
    sleep 0.5
    systemd-socket-activate -l 19531 "$GW" -D "$JOURNAL_DIR" >>/tmp/gatewayd.log 2>&1 &
    sleep 1
}

# Start a minimal OTLP/HTTP sink that writes each POST body to a numbered file.
python3 - "$SINK_DIR" >/tmp/sink.log 2>&1 <<'PY' &
import http.server, sys, json
sink_dir = sys.argv[1]
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
http.server.HTTPServer(('127.0.0.1', 4318), H).serve_forever()
PY
sleep 0.5

cat > /tmp/ojgf-config.toml <<EOF
otlp_endpoint = "http://127.0.0.1:4318"
batch_size = 10
# Use a long poll interval to prove the drain loop (not the poll interval)
# consumes the backlog. Without draining, 240 entries would take > 4 minutes.
poll_interval = "30s"
cursor_dir = "$CURSOR_DIR"
[[sources]]
name = "testhost"
url = "http://127.0.0.1:19531"
EOF

echo "### Building forwarder"
cd /src
cargo build >/dev/null 2>&1
BIN=/src/target/debug/otel-journal-gatewayd-forwarder

PHASE1_FIRST=1
PHASE1_LAST=120
PHASE2_FIRST=121
PHASE2_LAST=240

run_phase() {
    local first="$1" last="$2" log="$3"
    local pid ready=false

    "$BIN" -c /tmp/ojgf-config.toml -v >"$log" 2>&1 &
    pid=$!

    for i in $(seq 1 60); do
        sleep 1
        if python3 /src/e2e/verify_continuation.py "$SINK_DIR" "$first" "$last" >/dev/null 2>&1; then
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

echo ""
echo "=== PHASE 1: $PHASE1_FIRST-$PHASE1_LAST entries, batch_size 10, continuous ==="
gen_batch "$PHASE1_FIRST" "$PHASE1_LAST" | "$JR" --split-mode=none -o "$JOURNAL_DIR/batch1.journal" - 2>/dev/null
start_gatewayd

PHASE1_START=$(date +%s)
if ! run_phase "$PHASE1_FIRST" "$PHASE1_LAST" /tmp/run1.log; then
    echo "  PHASE 1 VERDICT: FAIL (did not reach expected record count in 60s)"
    exit 1
fi
PHASE1_ELAPSED=$(($(date +%s) - PHASE1_START))
echo "  phase 1 elapsed: ${PHASE1_ELAPSED}s (must be < 10s with 30s poll interval to prove draining)"
if [ "$PHASE1_ELAPSED" -ge 10 ]; then
    echo "  PHASE 1 VERDICT: FAIL (drain did not keep up; expected < 10s)"
    exit 1
fi

RUN1_CYCLES=$(grep -c 'Forwarded entries' /tmp/run1.log || true)
echo "  push cycles: $RUN1_CYCLES"
echo "  cursor after phase 1: $(cat "$CURSOR_DIR/testhost.cursor")"
python3 /src/e2e/verify_continuation.py "$SINK_DIR" "$PHASE1_FIRST" "$PHASE1_LAST"

# Snapshot how many sink requests exist before phase 2.
SINK_COUNT_BEFORE=$(ls "$SINK_DIR" | wc -l)

echo ""
echo "=== PHASE 2: +$PHASE2_FIRST-$PHASE2_LAST entries, gatewayd RESTARTED, fresh forwarder process ==="
gen_batch "$PHASE2_FIRST" "$PHASE2_LAST" | "$JR" --split-mode=none -o "$JOURNAL_DIR/batch2.journal" - 2>/dev/null
start_gatewayd

if ! run_phase "$PHASE1_FIRST" "$PHASE2_LAST" /tmp/run2.log; then
    echo "  PHASE 2 VERDICT: FAIL (did not reach expected record count in 60s)"
    exit 1
fi

RUN2_CYCLES=$(grep -c 'Forwarded entries' /tmp/run2.log || true)
echo "  push cycles: $RUN2_CYCLES"
echo "  cursor after phase 2: $(cat "$CURSOR_DIR/testhost.cursor")"
python3 /src/e2e/verify_continuation.py "$SINK_DIR" "$PHASE1_FIRST" "$PHASE2_LAST"

echo ""
echo "=========================================="
echo "CONTINUATION E2E: PASS"
echo "  phase 1 cycles: $RUN1_CYCLES"
echo "  phase 2 cycles: $RUN2_CYCLES"
echo "=========================================="
exit 0
