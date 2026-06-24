#!/usr/bin/env bash
# Backs up Plex Media Server config.
# Minimal (default): Preferences + Databases + Plug-in Preferences/Data
# Complete (--complete): also includes Metadata, Media, Scanners, Plug-ins
#
# Usage: backup.sh [--complete] [--output DIR]
# Curl: curl -fsSL https://raw.githubusercontent.com/scottdkey/plex/main/scripts/backup.sh | bash
# Curl with args: curl -fsSL .../backup.sh | bash -s -- --complete --output /mnt/backups
set -euo pipefail

COMPLETE=0
OUTPUT_DIR=""

while [[ $# -gt 0 ]]; do
    case "$1" in
        --complete) COMPLETE=1; shift ;;
        --output)   OUTPUT_DIR="$2"; shift 2 ;;
        *) echo "Unknown option: $1" >&2; exit 1 ;;
    esac
done

# Auto-detect Plex data dir
if [[ -d /var/lib/plexmediaserver ]]; then
    DATA_DIR="/var/lib/plexmediaserver"
elif [[ -d /config ]]; then
    DATA_DIR="/config"
else
    echo "[backup] Error: could not find Plex data dir (/var/lib/plexmediaserver or /config)" >&2
    exit 1
fi

PMS_DIR="${DATA_DIR}/Library/Application Support/Plex Media Server"

if [[ ! -d "$PMS_DIR" ]]; then
    echo "[backup] Error: Plex Media Server dir not found at: $PMS_DIR" >&2
    exit 1
fi

# Default output dir
if [[ -z "$OUTPUT_DIR" ]]; then
    if [[ -d /mnt/backups ]]; then
        OUTPUT_DIR="/mnt/backups"
    else
        OUTPUT_DIR="$(pwd)"
    fi
fi
mkdir -p "$OUTPUT_DIR"

LABEL=$([[ $COMPLETE -eq 1 ]] && echo "complete" || echo "minimal")
TIMESTAMP=$(date +%Y%m%d-%H%M%S)
OUT_FILE="${OUTPUT_DIR}/plex-backup-${LABEL}-${TIMESTAMP}.tar.gz"

# Detect runtime environment
IN_DOCKER=0
HAS_SYSTEMCTL=0
command -v systemctl > /dev/null 2>&1 && systemctl status > /dev/null 2>&1 && HAS_SYSTEMCTL=1 || true
[[ -f /.dockerenv ]] && IN_DOCKER=1 || true

plex_stop() {
    if [[ $HAS_SYSTEMCTL -eq 1 ]]; then
        echo "[backup] Stopping plexmediaserver..."
        systemctl stop plexmediaserver 2>/dev/null || true
    elif pgrep -x "Plex Media Server" > /dev/null 2>&1; then
        echo "[backup] Sending SIGTERM to Plex Media Server..."
        pkill -x "Plex Media Server" || true
        sleep 3
    fi
}

plex_start() {
    if [[ $HAS_SYSTEMCTL -eq 1 ]]; then
        echo "[backup] Starting plexmediaserver..."
        systemctl start plexmediaserver 2>/dev/null || true
    fi
    # Docker: entrypoint manages the process; no restart needed here
}

plex_stop
trap plex_start EXIT

echo "[backup] Mode: ${LABEL}"
echo "[backup] Source: ${PMS_DIR}"
echo "[backup] Output: ${OUT_FILE}"

# Always-excluded dirs
EXCLUDES=(
    "--exclude=./Library/Application Support/Plex Media Server/Cache"
    "--exclude=./Library/Application Support/Plex Media Server/Codecs"
    "--exclude=./Library/Application Support/Plex Media Server/Logs"
    "--exclude=./Library/Application Support/Plex Media Server/Crash Reports"
    "--exclude=./Library/Application Support/Plex Media Server/Updates"
    "--exclude=./Library/Application Support/Plex Media Server/Drivers"
)

# Minimal-only excludes (skipped for --complete)
if [[ $COMPLETE -eq 0 ]]; then
    EXCLUDES+=(
        "--exclude=./Library/Application Support/Plex Media Server/Metadata"
        "--exclude=./Library/Application Support/Plex Media Server/Media"
        "--exclude=./Library/Application Support/Plex Media Server/Scanners"
        "--exclude=./Library/Application Support/Plex Media Server/Plug-ins"
        "--exclude=./Library/Application Support/Plex Media Server/Plug-in Support/Caches"
        "--exclude=./Library/Application Support/Plex Media Server/Plug-in Support/Metadata Combination"
    )
fi

tar -czf "$OUT_FILE" "${EXCLUDES[@]}" -C "$DATA_DIR" .

SIZE=$(du -sh "$OUT_FILE" | cut -f1)
echo "[backup] Done. ${SIZE} → ${OUT_FILE}"
