#!/usr/bin/env bash
# Configures Plex Media Server for a bare LXC/VM install.
# Run once after install.sh. Not used in Docker (entrypoint.sh handles Docker).
# Usage: configure.sh [plex-uid] [plex-gid]
set -euo pipefail

PLEX_UID="${1:-1000}"
PLEX_GID="${2:-1000}"

# Resolve VAAPI driver path
ARCH=$(uname -m)
case "$ARCH" in
    aarch64) LIBVA_DRIVERS_PATH="/usr/lib/aarch64-linux-gnu/dri" ;;
    armv7l)  LIBVA_DRIVERS_PATH="/usr/lib/arm-linux-gnueabihf/dri" ;;
    *)       LIBVA_DRIVERS_PATH="/usr/lib/x86_64-linux-gnu/dri" ;;
esac

# Detect GPU (runs as root so device access is not blocked)
detect_gpu() {
    if [[ -e /dev/dri/renderD128 ]]; then
        for driver in iHD radeonsi i965; do
            if LIBVA_DRIVER_NAME=$driver LIBVA_DRIVERS_PATH="$LIBVA_DRIVERS_PATH" \
                vainfo --display drm --device /dev/dri/renderD128 > /dev/null 2>&1; then
                echo "$driver"
                return
            fi
        done
    fi
    echo "none"
}

GPU_DRIVER=$(detect_gpu)
echo "[configure] GPU driver: ${GPU_DRIVER}"

# Adjust plex user uid/gid — package installer creates the user, we may need to re-id it
if getent passwd plex > /dev/null 2>&1; then
    CURRENT_UID=$(id -u plex)
    [ "$CURRENT_UID" != "$PLEX_UID" ] && usermod -u "$PLEX_UID" plex || true
fi
if getent group plex > /dev/null 2>&1; then
    CURRENT_GID=$(getent group plex | cut -d: -f3)
    [ "$CURRENT_GID" != "$PLEX_GID" ] && groupmod -g "$PLEX_GID" plex || true
fi

# Add plex to GPU device groups
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

# Write systemd drop-in with VAAPI env vars
mkdir -p /etc/systemd/system/plexmediaserver.service.d
{
    echo "[Service]"
    echo "Environment=LIBVA_DRIVERS_PATH=${LIBVA_DRIVERS_PATH}"
    if [[ "$GPU_DRIVER" != "none" ]]; then
        echo "Environment=LIBVA_DRIVER_NAME=${GPU_DRIVER}"
        # LD_PRELOAD the glibc shim (built by install.sh) so Plex's musl/gcompat
        # transcoder can load the system VAAPI driver. Without this, hardware
        # transcoding silently falls back to software. See install.sh build_glibc_shim.
        if [[ -f /usr/local/lib/plex-glibc-shim.so ]]; then
            echo "Environment=LD_PRELOAD=/usr/local/lib/plex-glibc-shim.so"
        fi
    fi
} > /etc/systemd/system/plexmediaserver.service.d/gpu.conf

systemctl daemon-reload
systemctl enable --now plexmediaserver

LXC_IP=$(hostname -I | awk '{print $1}')
echo ""
echo "[configure] Done. Plex is running at http://${LXC_IP}:32400/web"
echo "[configure] Verify GPU: vainfo --display drm --device /dev/dri/renderD128"
