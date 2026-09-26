#!/usr/bin/env bash
set -euo pipefail

PLEX_MEDIA_SERVER_APPLICATION_SUPPORT_DIR="${CONFIG_DIR:-/config/Library/Application Support}"
PREFERENCES_PATH="${PLEX_MEDIA_SERVER_APPLICATION_SUPPORT_DIR}/Plex Media Server/Preferences.xml"

# Resolve VAAPI driver path for this architecture — must happen before detect_gpu
ARCH=$(uname -m)
case "$ARCH" in
    aarch64) LIBVA_DRIVERS_PATH="${LIBVA_DRIVERS_PATH:-/usr/lib/aarch64-linux-gnu/dri}" ;;
    armv7l)  LIBVA_DRIVERS_PATH="${LIBVA_DRIVERS_PATH:-/usr/lib/arm-linux-gnueabihf/dri}" ;;
    *)       LIBVA_DRIVERS_PATH="${LIBVA_DRIVERS_PATH:-/usr/lib/x86_64-linux-gnu/dri}" ;;
esac
export LIBVA_DRIVERS_PATH

# Ensure plex user/group match requested uid/gid. Capture the identity we are
# moving FROM — existing files under /config still carry it and have to be
# re-stamped further down, or the server cannot write its own databases.
OLD_UID=""
OLD_GID=""
if ! getent group plex > /dev/null 2>&1; then
    groupadd -g "${PLEX_GID}" plex
else
    # Move the GROUP itself. `usermod -g` needs the target gid to already exist,
    # so assuming some other group owns it silently fails the remap.
    OLD_GID=$(getent group plex | cut -d: -f3)
    [[ "$OLD_GID" == "${PLEX_GID}" ]] || groupmod -g "${PLEX_GID}" plex
fi
if ! getent passwd plex > /dev/null 2>&1; then
    useradd -u "${PLEX_UID}" -g "${PLEX_GID}" -d /config -s /bin/bash plex
else
    OLD_UID=$(id -u plex)
    usermod -u "${PLEX_UID}" -g "${PLEX_GID}" plex
fi

# Add plex user to whatever groups own the GPU devices
shopt -s nullglob
for dev in /dev/dri/renderD128 /dev/dri/card0 /dev/nvidia*; do
    [[ -e "$dev" ]] || continue
    dev_gid=$(stat -c '%g' "$dev")
    if ! getent group "$dev_gid" > /dev/null 2>&1; then
        groupadd -g "$dev_gid" "gpu-${dev_gid}"
    fi
    usermod -aG "gpu-${dev_gid}" plex 2>/dev/null || true
done
shopt -u nullglob

# Auto-detect GPU and configure hardware transcoding
detect_gpu() {
    # NVIDIA: runtime mounts /dev/nvidia* — NVENC/NVDEC, no VAAPI needed
    if [[ -e /dev/nvidia0 ]]; then
        echo "nvidia"
        return
    fi

    # VAAPI: probe each driver in preference order
    if [[ -e /dev/dri/renderD128 ]]; then
        for driver in iHD radeonsi i965; do
            if LIBVA_DRIVER_NAME=$driver LIBVA_DRIVERS_PATH="${LIBVA_DRIVERS_PATH}" \
                vainfo --display drm --device /dev/dri/renderD128 > /dev/null 2>&1; then
                echo "$driver"
                return
            fi
        done
    fi

    echo "none"
}

if [[ "${LIBVA_DRIVER_NAME:-auto}" == "auto" ]]; then
    detected=$(detect_gpu)
    case "$detected" in
        nvidia)
            echo "[entrypoint] GPU: NVIDIA (NVENC/NVDEC)"
            unset LIBVA_DRIVER_NAME
            ;;
        none)
            echo "[entrypoint] GPU: none detected — software transcoding only"
            unset LIBVA_DRIVER_NAME
            ;;
        *)
            echo "[entrypoint] GPU: VAAPI driver=${detected}"
            export LIBVA_DRIVER_NAME="$detected"
            ;;
    esac
fi

# Create required directories
mkdir -p \
    "${PLEX_MEDIA_SERVER_APPLICATION_SUPPORT_DIR}/Plex Media Server" \
    "${TRANSCODE_DIR:-/transcode}"

# Only chown top-level entries to avoid scanning a large library on every start
chown plex:plex /config "${TRANSCODE_DIR:-/transcode}"
chown plex:plex "${PLEX_MEDIA_SERVER_APPLICATION_SUPPORT_DIR}/Plex Media Server"

# ...but top-level alone is wrong when the IDENTITY changed. Everything already
# inside /config keeps the old owner, so changing PLEX_UID/GID on an existing
# install leaves Plex unable to open com.plexapp.plugins.library.db (EACCES) and
# the container crash-loops. Gate the deep walk on the identity actually changing:
# a normal start still costs nothing, which is what the comment above is protecting,
# and the one-off cost is paid only on the rare remap.
#
# Media mounts are deliberately NOT touched: they arrive from outside, can hold
# millions of files, and their ownership belongs to whoever provisioned the share.
if [[ -n "$OLD_UID" && "$OLD_UID" != "${PLEX_UID}" ]] \
   || [[ -n "$OLD_GID" && "$OLD_GID" != "${PLEX_GID}" ]]; then
    echo "[entrypoint] identity remapped ${OLD_UID:-?}:${OLD_GID:-?} -> ${PLEX_UID}:${PLEX_GID} — re-stamping state"
    for state_dir in /config "${TRANSCODE_DIR:-/transcode}"; do
        [[ -d "$state_dir" ]] || continue
        if [[ -n "$OLD_UID" ]]; then
            find "$state_dir" -uid "$OLD_UID" -exec chown -h "${PLEX_UID}" {} + 2>/dev/null || true
        fi
        if [[ -n "$OLD_GID" ]]; then
            find "$state_dir" -gid "$OLD_GID" -exec chgrp -h "${PLEX_GID}" {} + 2>/dev/null || true
        fi
    done
fi

# Write initial preferences if claim token provided and prefs don't exist yet
if [[ -n "${PLEX_CLAIM:-}" ]] && [[ ! -f "${PREFERENCES_PATH}" ]]; then
    mkdir -p "$(dirname "${PREFERENCES_PATH}")"
    cat > "${PREFERENCES_PATH}" << XML
<?xml version="1.0" encoding="utf-8"?>
<Preferences PlexOnlineToken="${PLEX_CLAIM}" HardwareAcceleratedEncoders="1" HardwareAcceleratedCodecs="1" TranscoderToneMapping="1" TranscoderToneMappingAlgorithm="mobius" />
XML
    chown plex:plex "${PREFERENCES_PATH}"
fi

# LD_PRELOAD the glibc shim (built by install.sh) when using a VAAPI driver, so
# Plex's musl/gcompat transcoder can dlopen the system VAAPI driver. Without it,
# hardware transcoding silently falls back to software. Not needed for NVIDIA.
PRELOAD=""
if [[ -n "${LIBVA_DRIVER_NAME:-}" ]] && [[ -f /usr/local/lib/plex-glibc-shim.so ]]; then
    PRELOAD="/usr/local/lib/plex-glibc-shim.so"
fi

exec gosu plex env \
    LIBVA_DRIVERS_PATH="${LIBVA_DRIVERS_PATH}" \
    ${LIBVA_DRIVER_NAME:+LIBVA_DRIVER_NAME="${LIBVA_DRIVER_NAME}"} \
    ${PRELOAD:+LD_PRELOAD="${PRELOAD}"} \
    /usr/lib/plexmediaserver/Plex\ Media\ Server
