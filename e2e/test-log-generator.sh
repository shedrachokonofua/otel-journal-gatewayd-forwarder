#!/bin/bash
# Generate test logs at regular intervals
# These logs will be picked up by the forwarder

set -e

COUNTER=0

echo "[test-log-generator] Starting log generator"

while true; do
    COUNTER=$((COUNTER + 1))

    # Generate various log levels
    case $((COUNTER % 4)) in
        0)
            echo "[test-log-generator] INFO: Test message #${COUNTER} - everything is fine"
            ;;
        1)
            echo "[test-log-generator] WARNING: Test message #${COUNTER} - something to note" >&2
            ;;
        2)
            echo "[test-log-generator] DEBUG: Test message #${COUNTER} - detailed info"
            ;;
        3)
            echo "[test-log-generator] ERROR: Test message #${COUNTER} - simulated error" >&2
            ;;
    esac

    sleep 1
done

