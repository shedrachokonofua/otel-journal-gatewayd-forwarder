#!/bin/bash
# Debian-specific package installation
set -euo pipefail

export DEBIAN_FRONTEND=noninteractive

# Install base dependencies (skip system rust - it's too old)
apt-get update && apt-get install -y \
    systemd \
    systemd-journal-remote \
    libssl-dev \
    pkg-config \
    procps \
    curl \
    jq \
    ca-certificates \
    build-essential \
    && apt-get clean \
    && rm -rf /var/lib/apt/lists/*

# Create /sbin/init symlink for consistency with other distros
ln -sf /lib/systemd/systemd /sbin/init

# Install Rust via rustup (system rust on Debian is too old for Cargo.lock v4)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable --profile minimal
source "$HOME/.cargo/env"
