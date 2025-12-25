#!/bin/bash
# Amazon Linux 2023 specific package installation
set -euo pipefail

# AL2023 comes with curl-minimal which conflicts with curl
# Use --allowerasing to replace it, or skip curl since curl-minimal works fine
dnf install -y --allowerasing \
    systemd \
    systemd-journal-remote \
    rust \
    cargo \
    openssl-devel \
    procps-ng \
    curl \
    jq \
    tar \
    gzip \
    && dnf clean all
