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

# Detect GPU
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
echo "[configure] GPU driver: $GPU_DRIVER"

# Adjust plex user uid/gid to match host
if getent passwd plex > /dev/null 2>&1; then
    usermod -u "$PLEX_UID" plex
fi
if getent group plex > /dev/null 2>&1; then
    groupmod -g "$PLEX_GID" plex
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

# Write systemd override with VAAPI env vars
mkdir -p /etc/systemd/system/plexmediaserver.service.d
cat > /etc/systemd/system/plexmediaserver.service.d/gpu.conf << EOF
[Service]
Environment=LIBVA_DRIVERS_PATH=${LIBVA_DRIVERS_PATH}
EOF

if [[ "$GPU_DRIVER" != "none" ]]; then
    cat >> /etc/systemd/system/plexmediaserver.service.d/gpu.conf << EOF
Environment=LIBVA_DRIVER_NAME=${GPU_DRIVER}
EOF
fi

systemctl daemon-reload
systemctl enable plexmediaserver
systemctl restart plexmediaserver

echo "[configure] Plex started. Reachable at http://$(hostname -I | awk '{print $1}'):32400/web"
echo "[configure] Run 'vainfo' as plex user to verify GPU access."
