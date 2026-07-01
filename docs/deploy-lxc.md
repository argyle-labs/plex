# Plex on a Proxmox LXC (native, Intel Quick Sync hardware transcoding)

A worked, standalone deployment: Plex Media Server installed **natively** (not in
Docker) inside a **privileged Debian 12 LXC** on Proxmox, with Intel Quick Sync
(`/dev/dri`) passthrough and NFS-mounted media. Privileged is required for
`/dev/dri` access. Nothing here needs orca.

> Placeholders: `<proxmox-host>` = your Proxmox node, `<nas>` = your NAS/NFS
> server, `<ip>` = a LAN address. Pick the CT ID with `pvesh get /cluster/nextid`
> (shown here as `<CTID>`); never hard-code one.

- **Port**: 32400 (`http://<ip>:32400/web`)
- **Type**: Proxmox LXC — Debian 12 minimal, **privileged** (for `/dev/dri`)
- **GPU**: Intel QSV via `/dev/dri` passthrough

Design goal: the smallest possible LXC — Debian 12 minimal, only the packages
Plex + NFS + Intel VA-API need. A modern Intel iGPU (e.g. UHD 770 on 12th-gen
Alder Lake) does hardware transcoding via Quick Sync.

---

## Step 1 — Create the LXC

```bash
pveam available | grep debian-12   # find the current template
pct create "$(pvesh get /cluster/nextid)" \
  local:vztmpl/debian-12-standard_12.7-1_amd64.tar.zst \
  --hostname plex \
  --storage local-lvm \
  --rootfs local-lvm:100 \
  --cores 6 --memory 4096 --swap 512 \
  --net0 name=eth0,bridge=vmbr0,ip=dhcp \
  --unprivileged 0
```

Disk is 100 GB because Plex's chapter-thumbnail **BIF** files (seek-bar previews)
can run tens of GB; the SQLite databases and metadata themselves are small. RAM
4 GB / 6 cores comfortably handles multiple simultaneous HW transcodes.

## Step 2 — GPU passthrough + NFS bind mounts

Stop the LXC, then edit `/etc/pve/lxc/<CTID>.conf` on `<proxmox-host>` (a full
sample is in [`lxc/plex.conf.example`](../lxc/plex.conf.example)):

```ini
# Intel iGPU passthrough — GIDs must match the host
# (verify: stat -c '%g' /dev/dri/renderD128)
dev0: /dev/dri/card0,gid=44
dev1: /dev/dri/renderD128,gid=44

# NFS bind mounts (host must have these mounted, e.g. via fstab)
mp0: /mnt/<nas>/data,mp=/mnt/data
mp1: /mnt/<nas>/backups/plex,mp=/mnt/backups
```

Verify the host has the GPU and NFS available:

```bash
ls /dev/dri/          # must show card0 and renderD128
df -h | grep <nas>    # must show the NFS mounts
mkdir -p /mnt/<nas>/backups/plex
```

## Step 3 — Minimal Debian

```bash
pct start <CTID>
pct enter <CTID>

apt-get update && apt-get upgrade -y
apt-get install -y --no-install-recommends \
  curl gnupg nfs-common intel-media-va-driver-non-free vainfo
apt-get remove --purge -y rsyslog cron at logrotate vim-tiny nano
apt-get autoremove --purge -y && apt-get clean && rm -rf /var/lib/apt/lists/*
```

> `intel-media-va-driver-non-free` is the iHD driver for 8th-gen+ Intel (needed
> for QSV). If it's not found, enable non-free first (`add-apt-repository non-free`).

## Step 4 — Install Plex

Plex moved its Linux package repo in March 2026 — use the current `repo.plex.tv`
repo (the old `downloads.plex.tv` repo no longer receives updates):

```bash
curl -LsSf https://repo.plex.tv/scripts/setupRepo.sh | bash
apt-get install -y plexmediaserver

# Give the plex user /dev/dri access, then start
usermod -aG render plex
systemctl enable --now plexmediaserver
```

<details>
<summary>Manual repo setup (if the script is unavailable)</summary>

```bash
curl -L https://downloads.plex.tv/plex-keys/PlexSign.v2.key \
  | gpg --yes --dearmor -o /usr/share/keyrings/plexmediaserver.v2.gpg
echo "deb [signed-by=/usr/share/keyrings/plexmediaserver.v2.gpg] https://repo.plex.tv/deb/ public main" \
  > /etc/apt/sources.list.d/plex.list
apt-get update && apt-get install -y plexmediaserver
apt-cache policy plexmediaserver   # should show https://repo.plex.tv/deb/ public/main
```
</details>

## Step 5 — Verify GPU access

```bash
ls /dev/dri/     # card0 and renderD128 must appear
id plex          # should include the render group
vainfo           # should show the Intel iHD driver with H264/HEVC/AV1 profiles
```

If `vainfo` fails on permissions, the service user needs the `render` group (gid 44):

```bash
usermod -aG render plex && systemctl restart plexmediaserver
```

## Step 6 — Static IP + first-run

Set a static DHCP lease (`<ip>`) for the LXC's MAC (`ip link show eth0`), then
open **http://<ip>:32400/web**, sign in / claim the server, and add libraries
from the `/mnt/data` bind mount (e.g. `/mnt/data/media/{movies,tv,music}`).

## Step 7 — Hardware transcoding

Plex → **Settings → Transcoder → Use hardware acceleration when available** →
Save. Start a transcode stream and confirm the GPU is doing the work:

```bash
ps aux | grep -i transcode          # a hw_transcode process should appear
intel_gpu_top                        # if intel-gpu-tools is installed
```

Without QSV, each 1080p software transcode burns 1–4 GB RAM and 1–2 cores — the
primary cause of memory/swap exhaustion on this LXC.

## Step 8 — Disable DLNA (memory-leak fix)

Plex's **`Plex DLNA Server`** subprocess has a well-known memory leak — it grows
without releasing and can consume several GB, pushing the LXC into swap. Unless
you use legacy DLNA devices (PS3, old smart TVs, some AV receivers), disable it:

**Settings → DLNA → Enable DLNA Server → OFF → Save.** This kills the process
immediately and frees the memory; no restart needed.

If you must keep DLNA, cap Plex with a systemd drop-in so a runaway is OOM-killed
fast:

```bash
mkdir -p /etc/systemd/system/plexmediaserver.service.d
cat > /etc/systemd/system/plexmediaserver.service.d/limits.conf << 'EOF'
[Service]
MemoryMax=8G
MemorySwapMax=256M
EOF
systemctl daemon-reload && systemctl restart plexmediaserver
```

## Step 9 — Nightly metadata backup

Plex state lives under `/var/lib/plexmediaserver/`. Back up metadata/databases
but **exclude the BIF scrubber index** (`*/Contents/Indexes`) — those are large
and regeneratable — along with `Logs/` and `Cache/`:

```bash
cat > /usr/local/bin/backup-plex.sh << 'EOF'
#!/bin/sh
set -e
BASE="/var/lib/plexmediaserver/Library/Application Support"
DEST=/mnt/backups; DATE=$(date +%Y%m%d_%H%M%S)
systemctl stop plexmediaserver
tar czf "$DEST/plex_${DATE}.tar.gz" \
  --exclude='*/Contents/Indexes' --exclude='Logs' --exclude='Cache' \
  -C "$BASE" "Plex Media Server"
systemctl start plexmediaserver
ls -dt "$DEST"/plex_*.tar.gz | tail -n +8 | xargs -r rm -f
EOF
chmod +x /usr/local/bin/backup-plex.sh
```

Schedule with a systemd timer (`OnCalendar=*-*-* 05:00:00`, `Persistent=true`)
rather than cron on minimal Debian.

**Restore:**

```bash
systemctl stop plexmediaserver
tar xzf /mnt/backups/plex_<date>.tar.gz \
  -C "/var/lib/plexmediaserver/Library/Application Support"
chown -R plex:plex "/var/lib/plexmediaserver/Library/Application Support/Plex Media Server"
systemctl start plexmediaserver
```

---

## Migrating an existing install (e.g. from an Unraid Docker container)

Stop the old container, `tar` its Plex appdata, and restore into the new LXC:

```bash
# On the old host — stop Plex first, then archive the appdata
tar czf /mnt/<nas>/backups/plex-migration.tar.gz -C /mnt/<nas>/appdata plex

# In the new LXC
systemctl stop plexmediaserver
tar xzf /mnt/backups/plex-migration.tar.gz \
  -C "/var/lib/plexmediaserver/Library/Application Support"
chown -R plex:plex "/var/lib/plexmediaserver/Library/Application Support/Plex Media Server"
systemctl start plexmediaserver
```

Re-verify GPU/transcoding and DLNA settings after restore (Steps 7–8).

## Failover to another node

1. Recreate the LXC on the other node with the same template + conf entries.
2. Restore: `tar xzf /mnt/backups/plex_<latest>.tar.gz -C "…/Application Support"`.
3. Repoint the static DHCP lease to the new LXC's MAC → same IP.
4. Start Plex — it picks up the same metadata and settings. (Any node with an
   Intel iGPU does QSV the same way.)

## Troubleshooting

**GPU not visible in the LXC** — on the host: `grep dev /etc/pve/lxc/<CTID>.conf`
and confirm the `dev0`/`dev1` lines; `ls -la /dev/dri/` inside the CT.

**vainfo fails / QSV off** — `LIBVA_DRIVER_NAME=iHD vainfo`; if permission denied,
`usermod -aG render plex && systemctl restart plexmediaserver`.

**Swap filling / LXC locked** — almost always the DLNA leak (Step 8). Check with
`ps aux --sort=-%mem | head`; a high-RSS `Plex DLNA Server` is the culprit.

**Client error "check that the file exists and the necessary drive is mounted"**
— usually a *connection* failure, not a missing file. Plex serves media over
`*.plex.direct` hostnames that resolve to LAN IPs; if a DNS ad-blocker's
**DNS-rebind protection** strips those answers, clients can't reach the server.
Add a rebind exception for `plex.direct`. Quick check from a LAN host:

```bash
dig +short 10-0-0-5.<hash>.plex.direct   # should return a LAN IP; empty = DNS stripping it
```
