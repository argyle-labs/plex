# plex

Self-hosted Plex Media Server with automatic GPU detection and hardware transcoding.

Built from `debian:12-slim`. Works with Intel, AMD, and NVIDIA GPUs. Runs on `amd64` and `arm64`.

## Features

- Auto-detects GPU: Intel Quick Sync (iHD/i965), AMD VAAPI (radeonsi), NVIDIA NVENC/NVDEC
- HDR → SDR tone mapping via `tonemap_vaapi` with OpenCL (Intel iHD Gen8+)
- Timezone inherited from host automatically via `/etc/localtime` mount
- Automatic uid/gid remapping — no permission headaches
- Works on Docker, Podman, and Dockge
- Published to GHCR: `ghcr.io/scottdkey/plex:latest`
- Versioned tags synced daily to Plex upstream (last 5 versions kept)

## GPU Support

| GPU | Generations | Driver | Notes |
|---|---|---|---|
| Intel iHD | Gen8+ (Broadwell → Arrow Lake, UHD 600+) | `iHD` | HDR tone mapping via OpenCL; amd64 only |
| Intel i965 | Gen4–9 (HD 2000–6000, Haswell, Skylake) | `i965` | Open source; amd64 only |
| AMD | GCN+ (RX 400+), RDNA 1/2/3 | `radeonsi` | Via Mesa; hardware encode/decode only |
| NVIDIA | GTX 900+ / RTX | NVENC/NVDEC | Requires `nvidia-container-toolkit` on host |

`LIBVA_DRIVER_NAME=auto` (the default) probes the available hardware and selects the right driver automatically.

## Quick Start

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

`--shm-size=4g` is required; the default 64MB shm is too small for Plex transcode buffers.

## Compose Examples

See the [`examples/`](examples/) directory:

| File | Description |
|---|---|
| `docker-compose.basic.yml` | Minimal setup — works for Intel and AMD |
| `docker-compose.dockge.yml` | Dockge-managed deployment with healthcheck |
| `docker-compose.tmpfs-transcode.yml` | RAM-backed transcode dir (fastest, no disk wear) |
| `docker-compose.nvidia.yml` | NVIDIA GPU via nvidia-container-toolkit |

All examples use `LIBVA_DRIVER_NAME: auto` and inherit timezone from the host.

```sh
cp examples/docker-compose.dockge.yml compose.yml
# edit: set PLEX_UID, PLEX_GID, volume paths
docker compose up -d
```

## Environment Variables

| Variable | Default | Description |
|---|---|---|
| `PLEX_UID` | `1000` | UID for the plex process |
| `PLEX_GID` | `1000` | GID for the plex process |
| `LIBVA_DRIVER_NAME` | `auto` | `auto`, `iHD`, `i965`, `radeonsi` — auto-detected at startup |
| `LIBVA_DRIVERS_PATH` | _(arch-detected)_ | Path to VA driver `.so` files |
| `PLEX_CLAIM` | _(unset)_ | Claim token from plex.tv/claim — used only on first run |
| `CONFIG_DIR` | `/config/Library/Application Support` | Plex application support dir |
| `TRANSCODE_DIR` | `/transcode` | Transcode working directory |

## Volumes

| Mount | Description |
|---|---|
| `/etc/localtime` | Mount from host (`ro`) — sets container timezone automatically |
| `/config` | Plex library, database, preferences — **must be persistent** |
| `/transcode` | Transcode working directory — can be tmpfs or a fast disk |
| `/mnt/media` | Your media library — mount any path here (read-only recommended) |

The `/config` volume must be the **parent** of `Library/` — i.e. `/your/path:/config`, not `/your/path/Library/Application Support:/config`.

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

## Proxmox LXC

For Docker inside a Proxmox LXC, add to `/etc/pve/lxc/<vmid>.conf`:

```
features: nesting=1
lxc.apparmor.profile: unconfined
lxc.seccomp.profile:
dev0: /dev/dri/renderD128,gid=44
dev1: /dev/dri/card0,gid=44
```

`lxc.seccomp.profile:` with no value disables the default seccomp filter, which is required for nested Docker. `gid=44` matches the `video` group on most Debian/Ubuntu hosts — verify with `stat -c '%g' /dev/dri/renderD128` on the Proxmox host.

## Enabling Hardware Transcoding in Plex

After first run, open Plex Settings:

1. **Settings → Transcoder → Enable Hardware-Accelerated Encoding** ✓
2. **Settings → Transcoder → Use Hardware-Accelerated Video Encoding** ✓
3. **Settings → Transcoder → Enable HDR tone mapping** ✓ _(Intel iHD only)_

Verify GPU access inside the container:

```sh
docker exec plex vainfo
```

## Plex Pass

Hardware transcoding requires an active [Plex Pass](https://www.plex.tv/plex-pass/) subscription.

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
# or pin a specific Plex version:
docker build --build-arg PLEX_VERSION=1.41.2.9200-c6bbc1b53 -t plex-local .
```
