#!/bin/bash
# Fedora-specific package installation
set -euo pipefail

# Skip system rust/cargo — Fedora's packages lag the crate MSRV (1.96.1).
dnf install -y \
    systemd \
    systemd-journal-remote \
    openssl-devel \
    gcc \
    procps-ng \
    curl \
    jq \
    python3 \
    && dnf clean all

curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
  | sh -s -- -y --default-toolchain 1.96.1 --profile minimal
# shellcheck source=/dev/null
source "$HOME/.cargo/env"
