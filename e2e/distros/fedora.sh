#!/bin/bash
# Fedora-specific package installation
set -euo pipefail

dnf install -y \
    systemd \
    systemd-journal-remote \
    rust \
    cargo \
    openssl-devel \
    procps-ng \
    curl \
    jq \
    python3 \
    && dnf clean all

