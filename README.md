<p align="center">
  <img src="https://raw.githubusercontent.com/argyle-labs/plex/main/assets/icon-256.png" width="120" alt="plex" />
</p>

<p align="center">
  <a href="https://github.com/argyle-labs/plex/actions/workflows/ci.yml"><img src="https://github.com/argyle-labs/plex/actions/workflows/ci.yml/badge.svg" alt="CI" /></a>
  <a href="https://github.com/argyle-labs/plex/actions/workflows/build.yml"><img src="https://github.com/argyle-labs/plex/actions/workflows/build.yml/badge.svg" alt="Build and Push" /></a>
  <a href="https://github.com/argyle-labs/plex/actions/workflows/release.yml"><img src="https://github.com/argyle-labs/plex/actions/workflows/release.yml/badge.svg" alt="Release" /></a>
</p>

# plex

Self-hosted **[Plex Media Server](https://www.plex.tv/)** — organizes and streams
your movies, TV, music, and photos — packaged for hardware transcoding (Intel QSV
/ AMD VAAPI / NVIDIA NVENC), plus a first-party
[orca](https://github.com/argyle-labs/orca) plugin for lifecycle and diagnostics.

This repo is **self-contained**: it builds its own slim image, ships ready-to-run
compose examples for each GPU, and a one-command Proxmox LXC provisioner — so you
can run Plex **without orca** on docker, podman, an LXC, a VM, or Unraid.

---

## Run it without orca

### Docker / Podman

The image (`ghcr.io/argyle-labs/plex`, built from [`Dockerfile`](Dockerfile) on
`debian:12-slim`) runs `network_mode: host` with `/dev/dri` passed through for
Intel/AMD VAAPI. Pick the example that matches your hardware:

| Example | For |
|---|---|
| [`examples/docker-compose.basic.yml`](examples/docker-compose.basic.yml) | Intel / AMD iGPU (VAAPI, `/dev/dri`) |
| [`examples/docker-compose.nvidia.yml`](examples/docker-compose.nvidia.yml) | NVIDIA (NVENC, needs `nvidia-container-toolkit`) |
| [`examples/docker-compose.tmpfs-transcode.yml`](examples/docker-compose.tmpfs-transcode.yml) | RAM-backed transcode scratch |
| [`examples/docker-compose.dockge.yml`](examples/docker-compose.dockge.yml) | Managed via Dockge (with healthcheck) |

```sh
cp examples/docker-compose.basic.yml compose.yml
# edit the media mount + /opt/plex paths, then:
docker compose up -d          # or: podman compose up -d
```

Plex listens on **:32400** (`http://<host>:32400/web`). On first run, set
`PLEX_CLAIM` from [plex.tv/claim](https://plex.tv/claim) (valid ~4 min) to bind
the server to your account. Podman uses the same files (`podman compose up -d`).

**Not tied to our image.** `ghcr.io/argyle-labs/plex` is just a convenience
build. Swap the `image:` for any equivalent — you don't have to use ours:

| Image | Notes |
|---|---|
| `ghcr.io/argyle-labs/plex` | this repo's slim build (`Dockerfile`); Intel VAAPI ready |
| `plexinc/pms-docker` | official upstream image (`latest` / `beta` / `public` tags) |
| `lscr.io/linuxserver/plex` | LinuxServer.io build (uses `PUID`/`PGID`, `/config` layout) |

Or build your own: `docker build -t plex .`

### LXC (Proxmox)

One command on the Proxmox host — no clone needed:

```sh
bash <(curl -fsSL https://raw.githubusercontent.com/argyle-labs/plex/main/lxc/provision.sh) <vmid>
```

It builds a privileged Debian LXC with `/dev/dri` passthrough and installs Plex
natively. For the full manual walkthrough (GPU passthrough, NFS media mounts, QSV
verification, the DLNA memory-leak fix, nightly backup, migration, failover), see
**[docs/deploy-lxc.md](docs/deploy-lxc.md)**; a sample container config is in
[`lxc/plex.conf.example`](lxc/plex.conf.example).

### VM / bare metal

Install Plex from the upstream `repo.plex.tv` apt repo on the guest (same steps
as the LXC guide's *Install Plex* section), or run the container image inside the
VM. Pass through the GPU (`/dev/dri`, or an NVIDIA card) for hardware transcode.

### Unraid

Install from **Community Applications** (the *Apps* tab) — search **Plex Media
Server** and add the template; it wires up the web UI, `/config`, transcode, and
media shares for you. Add `/dev/dri` (Settings → Docker, or the template's extra
device) for Intel/AMD hardware transcoding. To use this repo's image instead, set
the template's *Repository* to `ghcr.io/argyle-labs/plex`. (Manual fallback:
*Docker → Add Container* with that image, port `32400`, `/config` + `/transcode`,
media read-only.)

### Dependencies

- **GPU (recommended)** for hardware transcoding: Intel iGPU (QSV/VAAPI) or AMD
  (VAAPI) via `/dev/dri`, or an NVIDIA card via `nvidia-container-toolkit`.
  Software transcode works without one but is CPU- and RAM-heavy.
- **A media library** mounted into the container/host (often NFS), read-only.
- **A Plex account** + claim token (plex.tv/claim) for first-run activation.

### Backup & restore

Plex's state is its config dir (`/config`, or `/var/lib/plexmediaserver` on a
native install). Stop it, `tar` that directory (exclude the regeneratable BIF
`Indexes`, `Cache`, and `Logs`), restore by extracting it back. The LXC guide
includes a ready-made nightly backup timer.

> With orca this is **`plex.backup` / `plex.restore`** — see below.

---

## With orca

Unlike the generic `service.*` backends, plex ships its **own typed tool
surface**, identical across **CLI, MCP, and REST** (generated from one
`#[orca_tool]` declaration):

| Tool | What it does |
|---|---|
| `plex.install` / `plex.update` | provision (LXC or Compose) / upgrade by release channel |
| `plex.backup` / `plex.restore` | config backup + restore (BIF/Cache/Logs excluded) |
| `plex.list` / `plex.detail` / `plex.create` / `plex.delete` | endpoint registry CRUD |
| `plex.server_info` | server name / version / platform |
| `plex.libraries` | configured library sections + paths |
| `plex.transcode_health` | classify active sessions; flag **software (CPU) fallback** (HW accel not engaging) |

```sh
orca plex transcode_health --endpoint media   # is hardware transcode actually engaging?
```

## Layout

- `src/` — the orca plugin (the `plex.*` tools above; typed upstream client via `progenitor`).
- `Dockerfile` + `scripts/` — build the slim image (`install`/`entrypoint`/`backup`/`restore`/`configure`).
- `examples/` — per-GPU compose files (Intel, NVIDIA, tmpfs, Dockge).
- `lxc/` — `provision.sh` one-command Proxmox LXC + `plex.conf.example`.
- `docs/` — [deploy-lxc.md](docs/deploy-lxc.md), the worked standalone LXC guide.
- `specs/` — the vendored OpenAPI spec the client is generated from.
- `assets/` — plugin icon.
