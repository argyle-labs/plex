#!/usr/bin/env bash
# Restores Plex Media Server config from a backup created by backup.sh.
#
# Usage: restore.sh <backup-file.tar.gz> [--force]
# Curl: curl -fsSL https://raw.githubusercontent.com/scottdkey/plex/main/scripts/restore.sh | bash -s -- /path/to/backup.tar.gz
set -euo pipefail

BACKUP_FILE="${1:?Usage: $0 <backup-file.tar.gz> [--force]}"
FORCE=0

shift
while [[ $# -gt 0 ]]; do
    case "$1" in
        --force) FORCE=1; shift ;;
        *) echo "Unknown option: $1" >&2; exit 1 ;;
    esac
done

if [[ ! -f "$BACKUP_FILE" ]]; then
    echo "[restore] Error: backup file not found: $BACKUP_FILE" >&2
    exit 1
fi

# Auto-detect Plex data dir
if [[ -d /var/lib/plexmediaserver ]]; then
    DATA_DIR="/var/lib/plexmediaserver"
elif [[ -d /config ]]; then
    DATA_DIR="/config"
else
    echo "[restore] Error: could not find Plex data dir (/var/lib/plexmediaserver or /config)" >&2
    exit 1
fi

echo "[restore] Backup: ${BACKUP_FILE}"
echo "[restore] Target: ${DATA_DIR}"

if [[ $FORCE -eq 0 ]]; then
    read -r -p "[restore] This will overwrite existing Plex config. Continue? [y/N] " confirm
    [[ "${confirm,,}" == "y" ]] || { echo "[restore] Aborted."; exit 0; }
fi

# Detect runtime environment
HAS_SYSTEMCTL=0
command -v systemctl > /dev/null 2>&1 && systemctl status > /dev/null 2>&1 && HAS_SYSTEMCTL=1 || true

plex_stop() {
    if [[ $HAS_SYSTEMCTL -eq 1 ]]; then
        echo "[restore] Stopping plexmediaserver..."
        systemctl stop plexmediaserver 2>/dev/null || true
    elif pgrep -x "Plex Media Server" > /dev/null 2>&1; then
        echo "[restore] Sending SIGTERM to Plex Media Server..."
        pkill -x "Plex Media Server" || true
        sleep 3
    fi
}

plex_start() {
    if [[ $HAS_SYSTEMCTL -eq 1 ]]; then
        echo "[restore] Starting plexmediaserver..."
        systemctl start plexmediaserver 2>/dev/null || true
        echo "[restore] Plex started."
    fi
}

plex_stop
trap plex_start EXIT

echo "[restore] Extracting..."
tar -xzf "$BACKUP_FILE" -C "$DATA_DIR"

# Fix ownership — plex user owns everything
if getent passwd plex > /dev/null 2>&1; then
    chown -R plex:plex "$DATA_DIR"
fi

echo "[restore] Done. Restored to: ${DATA_DIR}"
