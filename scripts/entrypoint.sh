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

# Ensure plex user/group match requested uid/gid
if ! getent group plex > /dev/null 2>&1; then
    groupadd -g "${PLEX_GID}" plex
fi
if ! getent passwd plex > /dev/null 2>&1; then
    useradd -u "${PLEX_UID}" -g "${PLEX_GID}" -d /config -s /bin/bash plex
else
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

# Write initial preferences if claim token provided and prefs don't exist yet
if [[ -n "${PLEX_CLAIM:-}" ]] && [[ ! -f "${PREFERENCES_PATH}" ]]; then
    mkdir -p "$(dirname "${PREFERENCES_PATH}")"
    cat > "${PREFERENCES_PATH}" << XML
<?xml version="1.0" encoding="utf-8"?>
<Preferences PlexOnlineToken="${PLEX_CLAIM}" HardwareAcceleratedEncoders="1" HardwareAcceleratedCodecs="1" TranscoderToneMapping="1" TranscoderToneMappingAlgorithm="mobius" />
XML
    chown plex:plex "${PREFERENCES_PATH}"
fi

exec gosu plex env \
    LIBVA_DRIVERS_PATH="${LIBVA_DRIVERS_PATH}" \
    ${LIBVA_DRIVER_NAME:+LIBVA_DRIVER_NAME="${LIBVA_DRIVER_NAME}"} \
    /usr/lib/plexmediaserver/Plex\ Media\ Server
