#!/usr/bin/env bash
# Restores Plex Media Server config from a backup created by backup.
# Installed as /usr/local/bin/restore by install.sh.
#
# Usage:
#   restore                        # list backups, restore latest
#   restore --list                 # list available backups and exit
#   restore <backup-file.tar.gz>   # restore specific file
#   restore --pbs                  # list PBS snapshots, restore latest
#   restore --pbs --snapshot TS    # restore specific PBS snapshot
#
# Options:
#   --container NAME   Docker container name (host-side invocation)
#   --force            Skip the 3-second abort window
#
# PBS env vars (required for --pbs):
#   PBS_REPOSITORY   e.g. backup@pbs@<ip>:datastore
#   PBS_FINGERPRINT  server certificate fingerprint (optional if trusted)
#   PBS_PASSWORD     (optional)
set -euo pipefail

BACKUP_FILE=""
CONTAINER=""
PBS=0
PBS_SNAPSHOT=""
FORCE=0
LIST_ONLY=0

if [[ $# -gt 0 ]] && [[ "$1" != --* ]]; then
    BACKUP_FILE="$1"
    shift
fi

while [[ $# -gt 0 ]]; do
    case "$1" in
        --container)  CONTAINER="$2";      shift 2 ;;
        --pbs)        PBS=1;               shift ;;
        --snapshot)   PBS_SNAPSHOT="$2";   shift 2 ;;
        --force)      FORCE=1;             shift ;;
        --list)       LIST_ONLY=1;         shift ;;
        *) echo "Unknown option: $1" >&2; exit 1 ;;
    esac
done

# ── Auto-detect Plex data dir ─────────────────────────────────────────────────
if [[ -n "$CONTAINER" ]]; then
    DATA_DIR=$(docker inspect "$CONTAINER" \
        --format '{{range .Mounts}}{{if eq .Destination "/config"}}{{.Source}}{{end}}{{end}}' 2>/dev/null || true)
    [[ -n "$DATA_DIR" ]] || { echo "[restore] Error: could not determine /config volume for '${CONTAINER}'" >&2; exit 1; }
elif [[ -d /var/lib/plexmediaserver ]]; then
    DATA_DIR="/var/lib/plexmediaserver"
elif [[ -d /config ]]; then
    DATA_DIR="/config"
else
    echo "[restore] Error: Plex data dir not found. Use --container NAME for host-side Docker." >&2
    exit 1
fi

PMS_DIR="${DATA_DIR}/Library/Application Support/Plex Media Server"

# ── Runtime detection ─────────────────────────────────────────────────────────
HAS_SYSTEMCTL=0
command -v systemctl > /dev/null 2>&1 && systemctl is-system-running > /dev/null 2>&1 && HAS_SYSTEMCTL=1 || true

plex_stop() {
    if [[ -n "$CONTAINER" ]]; then
        echo "[restore] Stopping Docker container: ${CONTAINER}..."
        docker stop "$CONTAINER" 2>/dev/null || true
    elif [[ $HAS_SYSTEMCTL -eq 1 ]]; then
        echo "[restore] Stopping plexmediaserver..."
        systemctl stop plexmediaserver 2>/dev/null || true
    elif pgrep -f "Plex Media Server" > /dev/null 2>&1; then
        pkill -f "Plex Media Server" || true
        sleep 3
    fi
}

plex_start() {
    if [[ -n "$CONTAINER" ]]; then
        docker start "$CONTAINER" 2>/dev/null || true
    elif [[ $HAS_SYSTEMCTL -eq 1 ]]; then
        systemctl start plexmediaserver 2>/dev/null || true
    fi
}

# ── Backup dir scan ───────────────────────────────────────────────────────────
find_backup_dir() {
    if [[ -n "${BACKUP_DIR:-}" ]] && [[ -d "$BACKUP_DIR" ]]; then
        echo "$BACKUP_DIR"
    elif [[ -d /mnt/backups ]]; then
        echo "/mnt/backups"
    elif [[ -d /backups ]]; then
        echo "/backups"
    else
        echo "$(pwd)"
    fi
}

# ── PBS path ─────────────────────────────────────────────────────────────────
if [[ $PBS -eq 1 ]]; then
    [[ -n "${PBS_REPOSITORY:-}" ]] || { echo "[restore] PBS_REPOSITORY is required for --pbs" >&2; exit 1; }
    command -v proxmox-backup-client > /dev/null 2>&1 || { echo "[restore] proxmox-backup-client not found." >&2; exit 1; }

    echo "[restore] Available PBS snapshots (host/plex):"
    proxmox-backup-client snapshots "host/plex" 2>/dev/null || { echo "[restore] No snapshots found." >&2; exit 1; }
    echo ""

    if [[ $LIST_ONLY -eq 1 ]]; then exit 0; fi

    if [[ -z "$PBS_SNAPSHOT" ]]; then
        PBS_SNAPSHOT=$(proxmox-backup-client snapshots "host/plex" 2>/dev/null \
            | tail -n +2 | awk '{print $3}' | sort | tail -1)
        [[ -n "$PBS_SNAPSHOT" ]] || { echo "[restore] No PBS snapshots found." >&2; exit 1; }
        echo "[restore] Using latest: ${PBS_SNAPSHOT}"
    fi

    if [[ $FORCE -eq 0 ]]; then
        echo "[restore] Restoring from PBS snapshot ${PBS_SNAPSHOT} in 3 seconds — Ctrl-C to abort"
        sleep 3
    fi

    plex_stop
    trap plex_start EXIT
    mkdir -p "$PMS_DIR"
    proxmox-backup-client restore "host/plex/${PBS_SNAPSHOT}" "plex-config.pxar" "$PMS_DIR"
    getent passwd plex > /dev/null 2>&1 && chown -R plex:plex "$DATA_DIR" || true
    echo "[restore] Done. Restored to: ${PMS_DIR}"
    exit 0
fi

# ── tar.gz path ───────────────────────────────────────────────────────────────
BACKUP_SEARCH_DIR=$(find_backup_dir)

if [[ -z "$BACKUP_FILE" ]]; then
    # List available backups
    mapfile -t BACKUPS < <(find "$BACKUP_SEARCH_DIR" -maxdepth 1 -name 'plex-backup-*.tar.gz' | sort -r)

    if [[ ${#BACKUPS[@]} -eq 0 ]]; then
        echo "[restore] No backups found in ${BACKUP_SEARCH_DIR}" >&2
        exit 1
    fi

    echo "[restore] Available backups in ${BACKUP_SEARCH_DIR}:"
    for i in "${!BACKUPS[@]}"; do
        echo "  [$i] ${BACKUPS[$i]##*/}"
    done
    echo ""

    if [[ $LIST_ONLY -eq 1 ]]; then exit 0; fi

    BACKUP_FILE="${BACKUPS[0]}"
    echo "[restore] Using latest: ${BACKUP_FILE##*/}"
fi

[[ -f "$BACKUP_FILE" ]] || { echo "[restore] Error: backup file not found: $BACKUP_FILE" >&2; exit 1; }

if [[ $FORCE -eq 0 ]]; then
    echo "[restore] Restoring ${BACKUP_FILE##*/} in 3 seconds — Ctrl-C to abort"
    sleep 3
fi

plex_stop
trap plex_start EXIT

echo "[restore] Extracting to ${DATA_DIR}..."
tar -xzf "$BACKUP_FILE" -C "$DATA_DIR"
getent passwd plex > /dev/null 2>&1 && chown -R plex:plex "$DATA_DIR" || true

echo "[restore] Done. Restored to: ${DATA_DIR}"
