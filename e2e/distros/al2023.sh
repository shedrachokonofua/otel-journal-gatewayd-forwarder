#!/bin/bash
# Amazon Linux 2023 specific package installation
set -euo pipefail

# AL2023 comes with curl-minimal which conflicts with curl.
# Skip system rust/cargo — packages lag the crate MSRV (1.96.1).
dnf install -y --allowerasing \
    systemd \
    systemd-journal-remote \
    openssl-devel \
    gcc \
    procps-ng \
    curl \
    jq \
    tar \
    gzip \
    python3 \
    && dnf clean all

curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
  | sh -s -- -y --default-toolchain 1.96.1 --profile minimal
# shellcheck source=/dev/null
source "$HOME/.cargo/env"
