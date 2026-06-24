#!/usr/bin/env bash
# Restores Plex Media Server config from a backup created by backup.sh.
# Supports tar.gz backups (default) and PBS backups (--pbs).
#
# Usage (tar.gz):
#   restore.sh <backup-file.tar.gz> [--force]
#
# Usage (PBS):
#   restore.sh --pbs [--snapshot TIMESTAMP] [--force]
#
# PBS env vars (required for --pbs):
#   PBS_REPOSITORY   e.g. backup@pbs@192.168.1.10:datastore
#   PBS_FINGERPRINT  server certificate fingerprint (optional if trusted)
#   PBS_PASSWORD     (optional)
#
# Curl: curl -fsSL https://raw.githubusercontent.com/scottdkey/plex/main/scripts/restore.sh | bash -s -- /path/to/backup.tar.gz
set -euo pipefail

BACKUP_FILE=""
PBS=0
PBS_SNAPSHOT=""
FORCE=0

# First arg may be positional (tar.gz path) or a flag
if [[ $# -gt 0 ]] && [[ "$1" != --* ]]; then
    BACKUP_FILE="$1"
    shift
fi

while [[ $# -gt 0 ]]; do
    case "$1" in
        --pbs)       PBS=1;               shift ;;
        --snapshot)  PBS_SNAPSHOT="$2";   shift 2 ;;
        --force)     FORCE=1;             shift ;;
        *) echo "Unknown option: $1" >&2; exit 1 ;;
    esac
done

# Auto-detect Plex data dir
if [[ -d /var/lib/plexmediaserver ]]; then
    DATA_DIR="/var/lib/plexmediaserver"
elif [[ -d /config ]]; then
    DATA_DIR="/config"
else
    echo "[restore] Error: could not find Plex data dir (/var/lib/plexmediaserver or /config)" >&2
    exit 1
fi

PMS_DIR="${DATA_DIR}/Library/Application Support/Plex Media Server"

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

# ── PBS path ─────────────────────────────────────────────────────────────────
if [[ $PBS -eq 1 ]]; then
    [[ -n "${PBS_REPOSITORY:-}" ]] || { echo "[restore] PBS_REPOSITORY is required for --pbs" >&2; exit 1; }
    command -v proxmox-backup-client > /dev/null 2>&1 || {
        echo "[restore] proxmox-backup-client not found." >&2; exit 1
    }

    # List snapshots if no specific one requested
    if [[ -z "$PBS_SNAPSHOT" ]]; then
        echo "[restore] Available PBS snapshots for host/plex:"
        proxmox-backup-client snapshots "host/plex"
        echo ""
        # Pick latest
        PBS_SNAPSHOT=$(proxmox-backup-client snapshots "host/plex" 2>/dev/null \
            | grep -v '^Backup' | awk '{print $3}' | sort | tail -1)
        [[ -n "$PBS_SNAPSHOT" ]] || { echo "[restore] No PBS snapshots found for host/plex" >&2; exit 1; }
        echo "[restore] Using latest snapshot: ${PBS_SNAPSHOT}"
    fi

    echo "[restore] PBS snapshot: ${PBS_SNAPSHOT}"
    echo "[restore] Target: ${PMS_DIR}"

    if [[ $FORCE -eq 0 ]]; then
        read -r -p "[restore] This will overwrite existing Plex config. Continue? [y/N] " confirm
        [[ "${confirm,,}" == "y" ]] || { echo "[restore] Aborted."; exit 0; }
    fi

    plex_stop
    trap plex_start EXIT

    mkdir -p "$PMS_DIR"
    echo "[restore] Restoring from PBS..."
    proxmox-backup-client restore "host/plex/${PBS_SNAPSHOT}" "plex-config.pxar" "$PMS_DIR"

    if getent passwd plex > /dev/null 2>&1; then
        chown -R plex:plex "$DATA_DIR"
    fi

    echo "[restore] Done. Restored to: ${PMS_DIR}"
    exit 0
fi

# ── tar.gz path ───────────────────────────────────────────────────────────────
[[ -n "$BACKUP_FILE" ]] || { echo "Usage: $0 <backup-file.tar.gz> [--force]  OR  $0 --pbs [--snapshot TS] [--force]" >&2; exit 1; }
[[ -f "$BACKUP_FILE" ]] || { echo "[restore] Error: backup file not found: $BACKUP_FILE" >&2; exit 1; }

echo "[restore] Backup: ${BACKUP_FILE}"
echo "[restore] Target: ${DATA_DIR}"

if [[ $FORCE -eq 0 ]]; then
    read -r -p "[restore] This will overwrite existing Plex config. Continue? [y/N] " confirm
    [[ "${confirm,,}" == "y" ]] || { echo "[restore] Aborted."; exit 0; }
fi

plex_stop
trap plex_start EXIT

echo "[restore] Extracting..."
tar -xzf "$BACKUP_FILE" -C "$DATA_DIR"

if getent passwd plex > /dev/null 2>&1; then
    chown -R plex:plex "$DATA_DIR"
fi

echo "[restore] Done. Restored to: ${DATA_DIR}"
