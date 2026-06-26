#!/usr/bin/env bash
# Backs up Plex Media Server config.
#
# Modes:
#   Minimal (default)  Preferences + Databases + Plug-in Preferences/Data
#   --complete         Also includes Metadata, Media, Scanners, Plug-ins
#   --pbs              Push to Proxmox Backup Server via proxmox-backup-client
#   --pbs-prune-only   Prune PBS snapshots without taking a new backup
#
# Invocation (installed as /usr/local/bin/backup by install.sh):
#   LXC:    pct exec <vmid> -- backup [--complete]
#   Docker: docker exec plex backup [--complete]
#   Host:   backup --container plex --output /opt/plex/backups
#
# PBS env vars (required for --pbs):
#   PBS_REPOSITORY   e.g. backup@pbs@<ip>:datastore
#   PBS_FINGERPRINT  server certificate fingerprint (optional if trusted)
#   PBS_PASSWORD     service account password or API token secret
set -euo pipefail

COMPLETE=0
OUTPUT_DIR=""
CONTAINER=""
PBS=0
PBS_PRUNE_ONLY=0
KEEP_DAILY=7
KEEP_WEEKLY=4
KEEP_MONTHLY=2

while [[ $# -gt 0 ]]; do
    case "$1" in
        --complete)        COMPLETE=1;          shift ;;
        --output)          OUTPUT_DIR="$2";     shift 2 ;;
        --container)       CONTAINER="$2";      shift 2 ;;
        --pbs)             PBS=1;               shift ;;
        --pbs-prune-only)  PBS_PRUNE_ONLY=1;    shift ;;
        --keep-daily)      KEEP_DAILY="$2";     shift 2 ;;
        --keep-weekly)     KEEP_WEEKLY="$2";    shift 2 ;;
        --keep-monthly)    KEEP_MONTHLY="$2";   shift 2 ;;
        *) echo "Unknown option: $1" >&2; exit 1 ;;
    esac
done

# ── Runtime detection ────────────────────────────────────────────────────────
# Three modes: lxc (systemctl), docker-inside (pkill), docker-host (docker stop)
HAS_SYSTEMCTL=0
command -v systemctl > /dev/null 2>&1 && systemctl is-system-running > /dev/null 2>&1 && HAS_SYSTEMCTL=1 || true

plex_stop() {
    if [[ -n "$CONTAINER" ]]; then
        echo "[backup] Stopping Docker container: ${CONTAINER}..."
        docker stop "$CONTAINER" 2>/dev/null || true
    elif [[ $HAS_SYSTEMCTL -eq 1 ]]; then
        echo "[backup] Stopping plexmediaserver (systemctl)..."
        systemctl stop plexmediaserver 2>/dev/null || true
    elif pgrep -f "Plex Media Server" > /dev/null 2>&1; then
        echo "[backup] Stopping Plex Media Server (pkill)..."
        pkill -f "Plex Media Server" || true
        sleep 3
    fi
}

plex_start() {
    if [[ -n "$CONTAINER" ]]; then
        echo "[backup] Starting Docker container: ${CONTAINER}..."
        docker start "$CONTAINER" 2>/dev/null || true
    elif [[ $HAS_SYSTEMCTL -eq 1 ]]; then
        echo "[backup] Starting plexmediaserver (systemctl)..."
        systemctl start plexmediaserver 2>/dev/null || true
    fi
}

# ── PBS prune-only ────────────────────────────────────────────────────────────
if [[ $PBS_PRUNE_ONLY -eq 1 ]]; then
    [[ -n "${PBS_REPOSITORY:-}" ]] || { echo "[backup] PBS_REPOSITORY required for --pbs-prune-only" >&2; exit 1; }
    echo "[backup] Pruning PBS host/plex (daily=${KEEP_DAILY} weekly=${KEEP_WEEKLY} monthly=${KEEP_MONTHLY})..."
    proxmox-backup-client prune "host/plex" \
        --keep-daily "$KEEP_DAILY" \
        --keep-weekly "$KEEP_WEEKLY" \
        --keep-monthly "$KEEP_MONTHLY"
    echo "[backup] Prune done."
    exit 0
fi

# ── Auto-detect Plex data dir ────────────────────────────────────────────────
if [[ -n "$CONTAINER" ]]; then
    # Host-side Docker: get the volume mount from the container
    DATA_DIR=$(docker inspect "$CONTAINER" \
        --format '{{range .Mounts}}{{if eq .Destination "/config"}}{{.Source}}{{end}}{{end}}' 2>/dev/null || true)
    if [[ -z "$DATA_DIR" ]]; then
        echo "[backup] Error: could not determine /config volume for container '${CONTAINER}'" >&2
        echo "[backup] Pass --output and ensure /config is a named volume or bind mount." >&2
        exit 1
    fi
elif [[ -d /var/lib/plexmediaserver ]]; then
    DATA_DIR="/var/lib/plexmediaserver"
elif [[ -d /config ]]; then
    DATA_DIR="/config"
else
    echo "[backup] Error: Plex data dir not found. Use --container NAME for host-side Docker." >&2
    exit 1
fi

PMS_DIR="${DATA_DIR}/Library/Application Support/Plex Media Server"
[[ -d "$PMS_DIR" ]] || { echo "[backup] Error: Plex Media Server dir not found: ${PMS_DIR}" >&2; exit 1; }

LABEL=$([[ $COMPLETE -eq 1 ]] && echo "complete" || echo "minimal")
echo "[backup] Mode: ${LABEL}"
echo "[backup] Source: ${PMS_DIR}"

plex_stop
trap plex_start EXIT

# ── PBS path ──────────────────────────────────────────────────────────────────
if [[ $PBS -eq 1 ]]; then
    [[ -n "${PBS_REPOSITORY:-}" ]] || { echo "[backup] PBS_REPOSITORY required for --pbs" >&2; exit 1; }
    command -v proxmox-backup-client > /dev/null 2>&1 || {
        echo "[backup] proxmox-backup-client not found. Install:" >&2
        echo "  echo 'deb http://download.proxmox.com/debian/pbs-client bookworm main' > /etc/apt/sources.list.d/pbs-client.list" >&2
        echo "  curl -fsSL https://enterprise.proxmox.com/debian/proxmox-release-bookworm.gpg -o /etc/apt/trusted.gpg.d/proxmox-release-bookworm.gpg" >&2
        echo "  apt-get update && apt-get install -y proxmox-backup-client" >&2
        exit 1
    }

    PBS_EXCLUDES=(
        --exclude "Cache"
        --exclude "Codecs"
        --exclude "Logs"
        --exclude "Crash Reports"
        --exclude "Updates"
        --exclude "Drivers"
    )
    if [[ $COMPLETE -eq 0 ]]; then
        PBS_EXCLUDES+=(
            --exclude "Metadata"
            --exclude "Media"
            --exclude "Scanners"
            --exclude "Plug-ins"
            --exclude "Plug-in Support/Caches"
            --exclude "Plug-in Support/Metadata Combination"
        )
    fi

    echo "[backup] Pushing to PBS: ${PBS_REPOSITORY} (host/plex)..."
    proxmox-backup-client backup \
        "plex-config.pxar:${PMS_DIR}" \
        --backup-type host \
        --backup-id plex \
        "${PBS_EXCLUDES[@]}"

    echo "[backup] Pruning (daily=${KEEP_DAILY} weekly=${KEEP_WEEKLY} monthly=${KEEP_MONTHLY})..."
    proxmox-backup-client prune "host/plex" \
        --keep-daily "$KEEP_DAILY" \
        --keep-weekly "$KEEP_WEEKLY" \
        --keep-monthly "$KEEP_MONTHLY"

    echo "[backup] Done (PBS)."
    exit 0
fi

# ── tar.gz path ───────────────────────────────────────────────────────────────
if [[ -z "$OUTPUT_DIR" ]]; then
    if [[ -d /mnt/backups ]]; then
        OUTPUT_DIR="/mnt/backups"
    else
        OUTPUT_DIR="$(pwd)"
    fi
fi
mkdir -p "$OUTPUT_DIR"

TIMESTAMP=$(date +%Y%m%d-%H%M%S)
OUT_FILE="${OUTPUT_DIR}/plex-backup-${LABEL}-${TIMESTAMP}.tar.gz"
echo "[backup] Output: ${OUT_FILE}"

TAR_EXCLUDES=(
    "--exclude=./Library/Application Support/Plex Media Server/Cache"
    "--exclude=./Library/Application Support/Plex Media Server/Codecs"
    "--exclude=./Library/Application Support/Plex Media Server/Logs"
    "--exclude=./Library/Application Support/Plex Media Server/Crash Reports"
    "--exclude=./Library/Application Support/Plex Media Server/Updates"
    "--exclude=./Library/Application Support/Plex Media Server/Drivers"
)
if [[ $COMPLETE -eq 0 ]]; then
    TAR_EXCLUDES+=(
        "--exclude=./Library/Application Support/Plex Media Server/Metadata"
        "--exclude=./Library/Application Support/Plex Media Server/Media"
        "--exclude=./Library/Application Support/Plex Media Server/Scanners"
        "--exclude=./Library/Application Support/Plex Media Server/Plug-ins"
        "--exclude=./Library/Application Support/Plex Media Server/Plug-in Support/Caches"
        "--exclude=./Library/Application Support/Plex Media Server/Plug-in Support/Metadata Combination"
    )
fi

tar -czf "$OUT_FILE" "${TAR_EXCLUDES[@]}" -C "$DATA_DIR" .

SIZE=$(du -sh "$OUT_FILE" | cut -f1)
echo "[backup] Done. ${SIZE} → ${OUT_FILE}"
