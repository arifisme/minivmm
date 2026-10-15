#!/usr/bin/env bash
# Usage: sudo scripts/setup-tap.sh [tap-name] [owner] [host-cidr]
set -euo pipefail

TAP="${1:-tap0}"
OWNER="${2:-${SUDO_USER:-$(id -un)}}"
HOST_CIDR="${3:-172.16.0.1/24}"

if ip link show "$TAP" >/dev/null 2>&1; then
    ip link delete "$TAP"
fi
ip tuntap add dev "$TAP" mode tap user "$OWNER" vnet_hdr
ip addr add "$HOST_CIDR" dev "$TAP"
ip link set "$TAP" up
echo "$TAP is up with $HOST_CIDR, owned by $OWNER"
