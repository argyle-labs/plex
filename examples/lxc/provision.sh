#!/bin/sh
# plex — Plex Media Server (LXC 110) on thor
# Recreate: sh proxmox/lxcs/plex.sh
# NOTE: GPU passthrough (Intel iGPU) and NFS mount must be configured after creation.
#       See docs/services/plex.md for full setup.

CTID=110
NAME=plex
MEMORY=4096
CORES=6
DISK=108       # GB (large — Plex metadata cache)
STORAGE=local-lvm
BRIDGE=vmbr0
IP=<ip>/24
GW=<ip>
# Ubuntu template — check available: pveam list local
TEMPLATE=local:vztmpl/ubuntu-22.04-standard_22.04-1_amd64.tar.zst

pct create $CTID $TEMPLATE \
  --hostname $NAME \
  --memory $MEMORY \
  --cores $CORES \
  --rootfs ${STORAGE}:${DISK} \
  --net0 name=eth0,bridge=$BRIDGE,ip=$IP,gw=$GW \
  --unprivileged 0 \
  --onboot 1 \
  --features nesting=1 \
  --start 1

echo "LXC $CTID ($NAME) created at $IP"
echo ""
echo "Post-creation steps (in Proxmox UI > LXC 110 > Resources):"
echo "  1. Add GPU: /dev/dri/renderD128 and /dev/dri/card0"
echo "  2. Add NFS mount: <ip>:/mnt/user/data -> /mnt/data"
echo ""
echo "See docs/services/plex.md for full setup including hardware transcoding."
echo "DLNA is disabled — do NOT re-enable (causes 6.7 GB RAM leak)."
