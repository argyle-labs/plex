#!/usr/bin/env bash
# Creates and configures a Plex LXC on Proxmox VE.
# Run on the Proxmox host as root — no git clone required.
#
# Usage:
#   bash <(curl -fsSL https://raw.githubusercontent.com/scottdkey/plex/main/lxc/provision.sh) <vmid> [options]
#
# Options:
#   --hostname NAME       LXC hostname (default: plex)
#   --storage POOL        Proxmox storage pool for rootfs (default: local-lvm)
#   --disk SIZE           Root disk size (default: 32G)
#   --memory MB           RAM in MB (default: 4096)
#   --cores N             CPU cores (default: 4)
#   --bridge BRIDGE       Network bridge (default: vmbr0)
#   --ip IP/CIDR          Static IP with prefix (e.g. 192.168.1.50/24)
#   --gw GATEWAY          Default gateway IP
#   --media PATH          Host path to media dir (mounted read-only at /mnt/media)
#   --config PATH         Host path for Plex config (mounted at /var/lib/plexmediaserver)
#   --plex-version VER    Plex version to install (default: latest)
#   --branch BRANCH       Repo branch to pull scripts from (default: main)
#   --render-gid GID      GID of /dev/dri/renderD128 on the host (default: 44)
#   --card-gid GID        GID of /dev/dri/card0 on the host (default: 44)
#   --no-gpu              Skip GPU passthrough (minimal/software-transcode setup)
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
BRANCH="main"
RENDER_GID="44"
CARD_GID="44"
NO_GPU=0

while [[ $# -gt 0 ]]; do
    case "$1" in
        --hostname)     HOSTNAME="$2";      shift 2 ;;
        --storage)      STORAGE="$2";       shift 2 ;;
        --disk)         DISK="$2";          shift 2 ;;
        --memory)       MEMORY="$2";        shift 2 ;;
        --cores)        CORES="$2";         shift 2 ;;
        --bridge)       BRIDGE="$2";        shift 2 ;;
        --ip)           IP="$2";            shift 2 ;;
        --gw)           GW="$2";            shift 2 ;;
        --media)        MEDIA_PATH="$2";    shift 2 ;;
        --config)       CONFIG_PATH="$2";   shift 2 ;;
        --plex-version) PLEX_VERSION="$2";  shift 2 ;;
        --branch)       BRANCH="$2";        shift 2 ;;
        --render-gid)   RENDER_GID="$2";    shift 2 ;;
        --card-gid)     CARD_GID="$2";      shift 2 ;;
        --no-gpu)       NO_GPU=1;           shift ;;
        *) echo "Unknown option: $1" >&2; exit 1 ;;
    esac
done

REPO_RAW="https://raw.githubusercontent.com/scottdkey/plex/${BRANCH}"

# ── Template ──────────────────────────────────────────────────────────────────
TEMPLATE_STORE="local"

# Find the newest Debian 12 standard template — prefer already-downloaded
TEMPLATE=$(pveam list "$TEMPLATE_STORE" 2>/dev/null \
    | awk '{print $1}' \
    | sed 's|.*vztmpl/||' \
    | grep '^debian-12-standard' \
    | sort -V | tail -1)

if [[ -z "$TEMPLATE" ]]; then
    echo "[provision] No local Debian 12 template found, downloading..."
    pveam update
    TEMPLATE=$(pveam available --section system 2>/dev/null \
        | awk '{print $2}' \
        | grep '^debian-12-standard' \
        | sort -V | tail -1)
    if [[ -z "$TEMPLATE" ]]; then
        echo "[provision] ERROR: No debian-12-standard template available." >&2
        exit 1
    fi
    pveam download "$TEMPLATE_STORE" "$TEMPLATE"
fi

echo "[provision] Using template: ${TEMPLATE}"

# ── Network ───────────────────────────────────────────────────────────────────
NET_ARGS="name=eth0,bridge=${BRIDGE},firewall=1"
if [[ -n "$IP" ]]; then
    NET_ARGS="${NET_ARGS},ip=${IP}"
    [[ -n "$GW" ]] && NET_ARGS="${NET_ARGS},gw=${GW}"
else
    NET_ARGS="${NET_ARGS},ip=dhcp"
fi

# ── Create container ──────────────────────────────────────────────────────────
echo "[provision] Creating LXC ${VMID} (${HOSTNAME})..."
# Privileged (--unprivileged 0) is required for GPU device passthrough in LXC
pct create "$VMID" "${TEMPLATE_STORE}:vztmpl/${TEMPLATE}" \
    --hostname "$HOSTNAME" \
    --storage "$STORAGE" \
    --rootfs "${STORAGE}:${DISK}" \
    --memory "$MEMORY" \
    --cores "$CORES" \
    --net0 "$NET_ARGS" \
    --ostype debian \
    --unprivileged 0 \
    --start 0

# ── LXC config extras ─────────────────────────────────────────────────────────
{
    echo "lxc.apparmor.profile: unconfined"
    echo "lxc.seccomp.profile:"
} >> "/etc/pve/lxc/${VMID}.conf"

if [[ "$NO_GPU" -eq 0 ]]; then
    {
        echo "lxc.mount.entry: tmpfs dev/shm tmpfs nodev,nosuid,size=4g,mode=1777,create=dir 0 0"
        echo "dev0: /dev/dri/renderD128,gid=${RENDER_GID}"
        echo "dev1: /dev/dri/card0,gid=${CARD_GID}"
    } >> "/etc/pve/lxc/${VMID}.conf"
else
    echo "lxc.mount.entry: tmpfs dev/shm tmpfs nodev,nosuid,size=2g,mode=1777,create=dir 0 0" \
        >> "/etc/pve/lxc/${VMID}.conf"
fi

# ── Mount points (mp0 = media if provided, then config) ───────────────────────
MP=0
if [[ -n "$MEDIA_PATH" ]]; then
    echo "mp${MP}: ${MEDIA_PATH},mp=/mnt/media,ro=1" >> "/etc/pve/lxc/${VMID}.conf"
    MP=$((MP + 1))
fi
mkdir -p "$CONFIG_PATH"
echo "mp${MP}: ${CONFIG_PATH},mp=/var/lib/plexmediaserver" >> "/etc/pve/lxc/${VMID}.conf"

# ── Start and wait for network ────────────────────────────────────────────────
echo "[provision] Starting LXC ${VMID}..."
pct start "$VMID"

echo "[provision] Waiting for network..."
for i in $(seq 1 30); do
    if pct exec "$VMID" -- curl -fsSL --max-time 3 https://downloads.plex.tv > /dev/null 2>&1; then
        break
    fi
    sleep 2
done

# ── Install and configure Plex (fetched from public repo, no local files needed) ──
echo "[provision] Fetching and running install.sh..."
pct exec "$VMID" -- bash -c \
    "curl -fsSL '${REPO_RAW}/scripts/install.sh' | bash -s -- '${PLEX_VERSION}'"

echo "[provision] Fetching and running configure.sh..."
pct exec "$VMID" -- bash -c \
    "curl -fsSL '${REPO_RAW}/scripts/configure.sh' | bash"

# ── Done ──────────────────────────────────────────────────────────────────────
LXC_IP=$(pct exec "$VMID" -- hostname -I 2>/dev/null | awk '{print $1}')
echo ""
echo "[provision] Done. LXC ${VMID} (${HOSTNAME}) is running Plex."
echo "            http://${LXC_IP}:32400/web"
