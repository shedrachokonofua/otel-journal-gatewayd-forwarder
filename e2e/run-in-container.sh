#!/bin/bash
# Runs the e2e test inside a container with systemd
set -euo pipefail

IMAGE="${1:-otel-journal-e2e-test}"
RESULTS_DIR="${2:-/tmp/otel-journal-e2e-results}"

rm -rf "$RESULTS_DIR" && mkdir -p "$RESULTS_DIR"

# Start container with systemd in background
CONTAINER_ID=$(podman run -d --rm --privileged \
  -v "$RESULTS_DIR":/test-results:Z \
  "$IMAGE")
echo "Container started: $CONTAINER_ID"

cleanup() {
  podman stop -t 2 "$CONTAINER_ID" 2>/dev/null || true
}
trap cleanup EXIT

# Wait for systemd to be ready
echo "Waiting for systemd to initialize..."
sleep 5

# Run the test (output goes to stdout)
echo ""
podman exec -it "$CONTAINER_ID" /app/e2e/run-e2e-test.sh
TEST_EXIT=$?

# Report results
echo ""
echo "=========================================="
if [ $TEST_EXIT -eq 0 ]; then
  echo "E2E TEST PASSED"
  echo "=========================================="
  echo "Log records captured: $(jq -s '[.[].resourceLogs[]?.scopeLogs[]?.logRecords[]?] | length' "$RESULTS_DIR"/logs.json 2>/dev/null || echo 'N/A')"
else
  echo "E2E TEST FAILED (exit code: $TEST_EXIT)"
  echo "=========================================="
fi

exit $TEST_EXIT

