#!/usr/bin/env bash
# Creates and configures a Plex LXC on Proxmox VE.
# Run on the Proxmox host as root.
#
# Usage: provision.sh <vmid> [options]
#
# Options:
#   --hostname NAME       LXC hostname (default: plex)
#   --storage POOL        Proxmox storage pool for rootfs (default: local-lvm)
#   --disk SIZE           Root disk size (default: 32G)
#   --memory MB           RAM in MB (default: 4096)
#   --cores N             CPU cores (default: 4)
#   --bridge BRIDGE       Network bridge (default: vmbr0)
#   --ip IP/CIDR          Static IP with prefix (e.g. <ip>/24)
#   --gw GATEWAY          Default gateway IP
#   --media PATH          Host path to media directory (mounted read-only at /mnt/media)
#   --config PATH         Host path for Plex config (mounted at /var/lib/plexmediaserver)
#   --plex-version VER    Plex version to install (default: latest)
#   --render-gid GID      GID of /dev/dri/renderD128 on the host (default: 44)
#   --card-gid GID        GID of /dev/dri/card0 on the host (default: 44)
set -euo pipefail

VMID="${1:?Usage: $0 <vmid> [options]}"
shift

HOSTNAME="plex"
STORAGE="local-lvm"
DISK="32G"
MEMORY="4096"
CORES="4"
BRIDGE="vmbr0"
IP=""
GW=""
MEDIA_PATH=""
CONFIG_PATH="/opt/plex/config"
PLEX_VERSION="latest"
RENDER_GID="44"
CARD_GID="44"

while [[ $# -gt 0 ]]; do
    case "$1" in
        --hostname)    HOSTNAME="$2";      shift 2 ;;
        --storage)     STORAGE="$2";       shift 2 ;;
        --disk)        DISK="$2";          shift 2 ;;
        --memory)      MEMORY="$2";        shift 2 ;;
        --cores)       CORES="$2";         shift 2 ;;
        --bridge)      BRIDGE="$2";        shift 2 ;;
        --ip)          IP="$2";            shift 2 ;;
        --gw)          GW="$2";            shift 2 ;;
        --media)       MEDIA_PATH="$2";    shift 2 ;;
        --config)      CONFIG_PATH="$2";   shift 2 ;;
        --plex-version) PLEX_VERSION="$2"; shift 2 ;;
        --render-gid)  RENDER_GID="$2";    shift 2 ;;
        --card-gid)    CARD_GID="$2";      shift 2 ;;
        *) echo "Unknown option: $1" >&2; exit 1 ;;
    esac
done

# Download Debian 12 template if not present
TEMPLATE="debian-12-standard_12.7-1_amd64.tar.zst"
TEMPLATE_STORE="local"
if ! pveam list "$TEMPLATE_STORE" 2>/dev/null | grep -q "$TEMPLATE"; then
    echo "[provision] Downloading Debian 12 template..."
    pveam update
    pveam download "$TEMPLATE_STORE" "$TEMPLATE"
fi

# Network config
NET_ARGS="name=eth0,bridge=${BRIDGE},firewall=1"
if [[ -n "$IP" ]]; then
    NET_ARGS="${NET_ARGS},ip=${IP}"
    [[ -n "$GW" ]] && NET_ARGS="${NET_ARGS},gw=${GW}"
else
    NET_ARGS="${NET_ARGS},ip=dhcp"
fi

# Create the container
echo "[provision] Creating LXC ${VMID}..."
pct create "$VMID" "${TEMPLATE_STORE}:vztmpl/${TEMPLATE}" \
    --hostname "$HOSTNAME" \
    --storage "$STORAGE" \
    --rootfs "${STORAGE}:${DISK}" \
    --memory "$MEMORY" \
    --cores "$CORES" \
    --net0 "$NET_ARGS" \
    --ostype debian \
    --unprivileged 0 \
    --features nesting=1 \
    --start 0

# GPU passthrough
cat >> "/etc/pve/lxc/${VMID}.conf" << EOF
lxc.apparmor.profile: unconfined
lxc.seccomp.profile:
lxc.mount.entry: tmpfs dev/shm tmpfs nodev,nosuid,size=4g,mode=1777,create=dir 0 0
dev0: /dev/dri/renderD128,gid=${RENDER_GID}
dev1: /dev/dri/card0,gid=${CARD_GID}
EOF

# Media mount (optional)
if [[ -n "$MEDIA_PATH" ]]; then
    MP_INDEX=0
    echo "mp${MP_INDEX}: ${MEDIA_PATH},mp=/mnt/media,ro=1" >> "/etc/pve/lxc/${VMID}.conf"
fi

# Config mount
mkdir -p "$CONFIG_PATH"
MP_INDEX=1
echo "mp${MP_INDEX}: ${CONFIG_PATH},mp=/var/lib/plexmediaserver" >> "/etc/pve/lxc/${VMID}.conf"

echo "[provision] Starting LXC ${VMID}..."
pct start "$VMID"
sleep 5

# Wait for network
echo "[provision] Waiting for network..."
for i in $(seq 1 30); do
    if pct exec "$VMID" -- curl -fsSL --max-time 3 https://downloads.plex.tv > /dev/null 2>&1; then
        break
    fi
    sleep 2
done

# Copy and run install script
echo "[provision] Installing Plex..."
pct push "$VMID" "$(dirname "$0")/../scripts/install.sh" /tmp/install.sh --perms 0755
pct exec "$VMID" -- bash /tmp/install.sh "$PLEX_VERSION"

# Copy and run configure script
echo "[provision] Configuring Plex..."
pct push "$VMID" "$(dirname "$0")/../scripts/configure.sh" /tmp/configure.sh --perms 0755
pct exec "$VMID" -- bash /tmp/configure.sh

echo ""
echo "[provision] Done. LXC ${VMID} (${HOSTNAME}) is running Plex."
echo "            Open http://$(pct exec "$VMID" -- hostname -I | awk '{print $1}'):32400/web to set up."
