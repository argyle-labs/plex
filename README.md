# plex

Self-hosted Plex Media Server with automatic GPU detection and hardware transcoding.

Built from `debian:12-slim`. Supports native LXC, Docker, Podman, and Dockge. Works with Intel, AMD, and NVIDIA GPUs on `amd64` and `arm64`.

## Deployment Paths

| Path | Use case |
|---|---|
| [Native LXC](#proxmox-lxc-native) | Proxmox — preferred, no Docker overhead |
| [Docker / Compose](#docker--compose) | Any Linux host with Docker |

## Specs

### Minimal (software transcode only)

| Resource | Value |
|---|---|
| CPU | 2 cores |
| RAM | 2 GB |
| Disk | 16 GB (rootfs) |
| GPU | none required |
| shm | 2 GB |

Software transcode works but is CPU-bound. 4K content will max out cores.

### Recommended (hardware transcode + HDR tone mapping)

| Resource | Value |
|---|---|
| CPU | 4 cores |
| RAM | 4 GB |
| Disk | 32 GB (rootfs) |
| GPU | Intel iHD Gen8+ (UHD 600+) or AMD GCN+ |
| shm | 4 GB |

Hardware transcode offloads encode/decode to the GPU. Intel iHD additionally supports HDR→SDR tone mapping via OpenCL.

## GPU Support

| GPU | Generations | Driver | Notes |
|---|---|---|---|
| Intel iHD | Gen8+ (Broadwell → Arrow Lake, UHD 600+) | `iHD` | HDR tone mapping via OpenCL; amd64 only |
| Intel i965 | Gen4–9 (HD 2000–6000, Haswell, Skylake) | `i965` | Open source; amd64 only |
| AMD | GCN+ (RX 400+), RDNA 1/2/3 | `radeonsi` | Via Mesa; hardware encode/decode only |
| NVIDIA | GTX 900+ / RTX | NVENC/NVDEC | Requires `nvidia-container-toolkit` on host |

`LIBVA_DRIVER_NAME=auto` (the default) probes available hardware and selects the right driver automatically.

---

## Proxmox LXC (native)

The preferred deployment on Proxmox: plain Debian 12 LXC with Plex installed directly — no Docker.

### Automated provisioning

Run on the Proxmox host as root — no git clone needed:

```sh
# Minimal — software transcode only, 2 cores / 2 GB RAM
bash <(curl -fsSL https://raw.githubusercontent.com/scottdkey/plex/main/lxc/provision.sh) 116 \
  --hostname plex \
  --disk 16G \
  --memory 2048 \
  --cores 2 \
  --no-gpu \
  --config /opt/plex/config \
  --media /mnt/pool/data

# Recommended — Intel/AMD GPU, hardware transcode + HDR tone mapping
bash <(curl -fsSL https://raw.githubusercontent.com/scottdkey/plex/main/lxc/provision.sh) 116 \
  --hostname plex \
  --memory 4096 \
  --cores 4 \
  --config /opt/plex/config \
  --media /mnt/pool/data

# Recommended — static IP
bash <(curl -fsSL https://raw.githubusercontent.com/scottdkey/plex/main/lxc/provision.sh) 116 \
  --hostname plex \
  --memory 4096 \
  --cores 4 \
  --ip 192.168.1.50/24 \
  --gw 192.168.1.1 \
  --config /opt/plex/config \
  --media /mnt/pool/data

# Pinned Plex version
bash <(curl -fsSL https://raw.githubusercontent.com/scottdkey/plex/main/lxc/provision.sh) 116 \
  --hostname plex \
  --plex-version 1.41.2.9200-c6bbc1b53 \
  --config /opt/plex/config \
  --media /mnt/pool/data
```

The script resolves the latest Debian 12 template, creates the LXC, configures GPU passthrough, starts it, downloads and runs `install.sh` + `configure.sh` from this repo, and prints the Plex URL. No local files required.

GPU device GIDs default to 44 (standard on Debian/Ubuntu Proxmox hosts). Override with `--render-gid` / `--card-gid` if needed, or skip with `--no-gpu`.

### Manual install

Create and start a Debian 12 LXC using `lxc/plex.conf.example` as a reference, then inside the LXC:

```sh
# Run directly from the public repo — no git clone needed
curl -fsSL https://raw.githubusercontent.com/scottdkey/plex/main/scripts/install.sh | bash
curl -fsSL https://raw.githubusercontent.com/scottdkey/plex/main/scripts/configure.sh | bash
```

### LXC config reference

See [`lxc/plex.conf.example`](lxc/plex.conf.example) for minimal and recommended annotated configs.

Key entries for GPU passthrough (get GIDs from host: `stat -c '%g' /dev/dri/renderD128`):

```
features: nesting=1
lxc.apparmor.profile: unconfined
lxc.seccomp.profile:
lxc.mount.entry: tmpfs dev/shm tmpfs nodev,nosuid,size=4g,mode=1777,create=dir 0 0
dev0: /dev/dri/renderD128,gid=44
dev1: /dev/dri/card0,gid=44
```

### Verify GPU (LXC)

```sh
pct exec <vmid> -- vainfo --display drm --device /dev/dri/renderD128
```

---

## Docker / Compose

### Quick start

```sh
docker run -d \
  --name plex \
  --network=host \
  --shm-size=4g \
  --restart=unless-stopped \
  -e PLEX_UID=$(id -u) \
  -e PLEX_GID=$(id -g) \
  -e PLEX_CLAIM=claim-xxxxxxxxxxxx \
  -v /etc/localtime:/etc/localtime:ro \
  -v /opt/plex/config:/config \
  -v /opt/plex/transcode:/transcode \
  -v /mnt/media:/mnt/media:ro \
  --device /dev/dri/renderD128 \
  --device /dev/dri/card0 \
  ghcr.io/scottdkey/plex:latest
```

Get a claim token at [plex.tv/claim](https://plex.tv/claim) — expires in 4 minutes.

`--shm-size=4g` is required; Docker's default 64MB shm is too small for Plex transcode buffers.

### Compose examples

See the [`examples/`](examples/) directory:

| File | Description |
|---|---|
| `docker-compose.basic.yml` | Minimal setup — Intel/AMD GPU, auto-detect |
| `docker-compose.dockge.yml` | Dockge-managed with healthcheck |
| `docker-compose.tmpfs-transcode.yml` | RAM-backed transcode (fastest, no disk wear) |
| `docker-compose.nvidia.yml` | NVIDIA GPU via nvidia-container-toolkit |

All examples use `LIBVA_DRIVER_NAME: auto` and inherit timezone from the host.

```sh
cp examples/docker-compose.dockge.yml compose.yml
# edit: set PLEX_UID, PLEX_GID, volume paths
docker compose up -d
```

### Environment variables

| Variable | Default | Description |
|---|---|---|
| `PLEX_UID` | `1000` | UID for the plex process |
| `PLEX_GID` | `1000` | GID for the plex process |
| `LIBVA_DRIVER_NAME` | `auto` | `auto`, `iHD`, `i965`, `radeonsi` — auto-detected at startup |
| `LIBVA_DRIVERS_PATH` | _(arch-detected)_ | Path to VA driver `.so` files |
| `PLEX_CLAIM` | _(unset)_ | Claim token from plex.tv/claim — first run only |
| `CONFIG_DIR` | `/config/Library/Application Support` | Plex application support dir |
| `TRANSCODE_DIR` | `/transcode` | Transcode working directory |

### Volumes

| Mount | Description |
|---|---|
| `/etc/localtime` | Mount from host (`ro`) — sets container timezone automatically |
| `/config` | Plex library, database, preferences — **must be persistent** |
| `/transcode` | Transcode working dir — can be tmpfs or a fast disk |
| `/mnt/media` | Your media library — mount any path here (read-only recommended) |

The `/config` volume must be the **parent** of `Library/` — i.e. `/your/path:/config`, not `/your/path/Library/Application Support:/config`.

### Verify GPU (Docker)

```sh
docker exec plex vainfo --display drm --device /dev/dri/renderD128
```

---

## NVIDIA Prerequisites

NVIDIA requires `nvidia-container-toolkit` on the host:

```sh
curl -fsSL https://nvidia.github.io/libnvidia-container/gpgkey \
  | sudo gpg --dearmor -o /usr/share/keyrings/nvidia-container-toolkit-keyring.gpg
curl -sL https://nvidia.github.io/libnvidia-container/stable/deb/nvidia-container-toolkit.list \
  | sed 's#deb https://#deb [signed-by=/usr/share/keyrings/nvidia-container-toolkit-keyring.gpg] https://#g' \
  | sudo tee /etc/apt/sources.list.d/nvidia-container-toolkit.list
sudo apt-get update && sudo apt-get install -y nvidia-container-toolkit
sudo nvidia-ctk runtime configure --runtime=docker
sudo systemctl restart docker
```

Then use `examples/docker-compose.nvidia.yml`.

---

## Enabling Hardware Transcoding in Plex

After first run, open Plex Settings:

1. **Settings → Transcoder → Enable Hardware-Accelerated Encoding** ✓
2. **Settings → Transcoder → Use Hardware-Accelerated Video Encoding** ✓
3. **Settings → Transcoder → Enable HDR tone mapping** ✓ _(Intel iHD only)_

## Plex Pass

Hardware transcoding requires an active [Plex Pass](https://www.plex.tv/plex-pass/) subscription.

---

## Backup & Restore

`backup.sh` stops Plex, archives config, then restarts. Minimal is the default — complete is opt-in.

| Mode | What's included | What's excluded |
|---|---|---|
| **Minimal** (default) | `Preferences.xml`, `Plug-in Support/Databases`, `Plug-in Support/Preferences`, `Plug-in Support/Data` | Cache, Codecs, Logs, Metadata, Media, Scanners |
| **Complete** (`--complete`) | All of minimal + `Metadata`, `Media`, `Scanners`, `Plug-ins` | Cache, Codecs, Logs, Crash Reports, Updates, Drivers |

Cache and Codecs are always excluded — Plex regenerates them automatically.

### LXC

```sh
# Minimal backup (default) — writes to /mnt/backups if mounted, otherwise ./
pct exec <vmid> -- bash <(curl -fsSL https://raw.githubusercontent.com/scottdkey/plex/main/scripts/backup.sh)

# Complete backup
pct exec <vmid> -- bash <(curl -fsSL https://raw.githubusercontent.com/scottdkey/plex/main/scripts/backup.sh) --complete

# Restore
pct exec <vmid> -- bash <(curl -fsSL https://raw.githubusercontent.com/scottdkey/plex/main/scripts/restore.sh) /mnt/backups/plex-backup-minimal-20260101-120000.tar.gz
```

### Docker

```sh
# Minimal backup
docker exec plex bash <(curl -fsSL https://raw.githubusercontent.com/scottdkey/plex/main/scripts/backup.sh)

# Complete backup
docker exec plex bash <(curl -fsSL https://raw.githubusercontent.com/scottdkey/plex/main/scripts/backup.sh) --complete --output /mnt/backups

# Restore
docker exec plex bash <(curl -fsSL https://raw.githubusercontent.com/scottdkey/plex/main/scripts/restore.sh) /mnt/backups/plex-backup-minimal-20260101-120000.tar.gz --force
```

### From the host (volume path)

```sh
# If you have the backup script locally
bash scripts/backup.sh --output /mnt/backups
bash scripts/restore.sh /mnt/backups/plex-backup-minimal-20260101-120000.tar.gz
```

Backup output: `plex-backup-minimal-YYYYMMDD-HHMMSS.tar.gz` or `plex-backup-complete-YYYYMMDD-HHMMSS.tar.gz`.

---

## Tags

Images are synced daily to Plex upstream versions. The last 5 versions are kept.

```
ghcr.io/scottdkey/plex:latest                    # newest Plex release
ghcr.io/scottdkey/plex:1.41.2.9200-c6bbc1b53    # pinned Plex version
```

Tags match Plex's own version strings from `plex.tv/api/downloads/5.json`.

## Building Locally

```sh
git clone https://github.com/scottdkey/plex
cd plex
docker build -t plex-local .
# pin a specific Plex version:
docker build --build-arg PLEX_VERSION=1.41.2.9200-c6bbc1b53 -t plex-local .
```
