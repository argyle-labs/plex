#!/usr/bin/env bash
# Installs Plex Media Server + GPU drivers on Debian 12.
# Works in Docker (called from Dockerfile) and bare LXC/VM.
# Usage: install.sh [plex-version]   — defaults to "latest"
set -euo pipefail

PLEX_VERSION="${1:-latest}"

export DEBIAN_FRONTEND=noninteractive

apt-get update
apt-get install -y --no-install-recommends \
    ca-certificates \
    curl \
    gnupg \
    tzdata \
    udev \
    xmlstarlet \
    uuid-runtime \
    ocl-icd-libopencl1 \
    mesa-va-drivers \
    gosu \
    vainfo

# Enable non-free repos for Intel drivers (idempotent).
# Debian 12 Docker image uses DEB822 format; Proxmox LXC templates use legacy sources.list.
if [ -f /etc/apt/sources.list.d/debian.sources ]; then
    if ! grep -q 'non-free' /etc/apt/sources.list.d/debian.sources; then
        sed -i 's/^Components: main$/Components: main contrib non-free non-free-firmware/' \
            /etc/apt/sources.list.d/debian.sources
    fi
elif [ -f /etc/apt/sources.list ]; then
    if ! grep -q 'non-free' /etc/apt/sources.list; then
        sed -i '/debian\.org\/debian/s/$/ contrib non-free non-free-firmware/' \
            /etc/apt/sources.list
    fi
fi

# Intel drivers are x86-only
ARCH=$(dpkg --print-architecture)
if [ "$ARCH" = "amd64" ]; then
    apt-get update
    apt-get install -y --no-install-recommends \
        intel-media-va-driver-non-free \
        i965-va-driver \
        intel-opencl-icd
fi

# ── glibc shim for Plex's musl/gcompat transcoder ─────────────────────────────
# Plex's bundled ffmpeg ("Plex Transcoder") is musl-linked and uses an old
# libgcompat shim. When it dlopen()s the system Intel iHD VAAPI driver, the
# driver pulls in glibc libstdc++/libgcc which reference symbols gcompat does
# not provide (arc4random, _dl_find_object, __isoc23_*). The dlopen then fails
# with "Failed to initialise VAAPI connection: -1 (unknown libva error)" and
# Plex SILENTLY falls back to software transcoding (100% CPU, stutter).
#
# This shim provides those symbols; LD_PRELOAD'd into plexmediaserver (wired up
# by configure.sh for LXC and entrypoint.sh for Docker) it restores hardware
# transcoding. Built here so it ships in both the Docker image and LXC installs.
build_glibc_shim() {
    local need_gcc=0
    command -v cc > /dev/null 2>&1 || need_gcc=1
    if [ "$need_gcc" = "1" ]; then
        apt-get install -y --no-install-recommends gcc libc6-dev
    fi
    install -d /usr/local/lib
    cat > /tmp/plex-glibc-shim.c <<'SHIM'
/* plex-glibc-shim: glibc symbols missing from Plex's bundled musl/gcompat
 * runtime, so the system Intel iHD VAAPI driver can be dlopen'd by Plex's
 * musl-linked transcoder. LD_PRELOAD this into plexmediaserver. */
#define _GNU_SOURCE
#include <stdint.h>
#include <stdlib.h>
#include <stdio.h>
#include <stdarg.h>
#include <sys/random.h>

/* arc4random family (glibc 2.36) — needed by libstdc++ */
uint32_t arc4random(void) { uint32_t v; getrandom(&v, sizeof v, 0); return v; }
void arc4random_buf(void *buf, size_t n) {
    unsigned char *p = buf; size_t got = 0;
    while (got < n) { ssize_t r = getrandom(p + got, n - got, 0); if (r > 0) got += r; }
}
uint32_t arc4random_uniform(uint32_t upper) {
    if (upper < 2) return 0;
    uint32_t min = -upper % upper, r;
    do { r = arc4random(); } while (r < min);
    return r % upper;
}

/* _dl_find_object (glibc 2.35) — needed by libgcc_s; -1 = "not found" fallback */
int _dl_find_object(void *address, void *result) { (void)address; (void)result; return -1; }

/* __isoc23_* integer/scanf parsers (glibc 2.38) — classic impls are equivalent */
long      __isoc23_strtol(const char *n, char **e, int b)   { return strtol(n, e, b); }
long long __isoc23_strtoll(const char *n, char **e, int b)  { return strtoll(n, e, b); }
unsigned long      __isoc23_strtoul(const char *n, char **e, int b)  { return strtoul(n, e, b); }
unsigned long long __isoc23_strtoull(const char *n, char **e, int b) { return strtoull(n, e, b); }
int __isoc23_sscanf(const char *s, const char *f, ...) {
    va_list ap; va_start(ap, f); int r = vsscanf(s, f, ap); va_end(ap); return r;
}
int __isoc23_fscanf(FILE *fp, const char *f, ...) {
    va_list ap; va_start(ap, f); int r = vfscanf(fp, f, ap); va_end(ap); return r;
}
int __isoc23_scanf(const char *f, ...) {
    va_list ap; va_start(ap, f); int r = vfscanf(stdin, f, ap); va_end(ap); return r;
}
int __isoc23_vsscanf(const char *s, const char *f, va_list ap) { return vsscanf(s, f, ap); }
SHIM
    cc -O2 -shared -fPIC -o /usr/local/lib/plex-glibc-shim.so /tmp/plex-glibc-shim.c
    rm -f /tmp/plex-glibc-shim.c
    if [ "$need_gcc" = "1" ]; then
        apt-get purge -y gcc libc6-dev > /dev/null 2>&1 || true
        apt-get autoremove -y > /dev/null 2>&1 || true
    fi
}

if [ "$ARCH" = "amd64" ]; then
    build_glibc_shim
fi

# Install Plex
if [ "$PLEX_VERSION" = "latest" ]; then
    curl -fsSL https://downloads.plex.tv/plex-keys/PlexSign.key \
        | gpg --dearmor -o /usr/share/keyrings/plex-archive-keyring.gpg
    echo "deb [signed-by=/usr/share/keyrings/plex-archive-keyring.gpg] https://downloads.plex.tv/repo/deb public main" \
        > /etc/apt/sources.list.d/plexmediaserver.list
    apt-get update
    apt-get install -y --no-install-recommends plexmediaserver
else
    curl -fsSL \
        "https://downloads.plex.tv/plex-media-server-new/${PLEX_VERSION}/debian/plexmediaserver_${PLEX_VERSION}_${ARCH}.deb" \
        -o /tmp/plex.deb
    dpkg -i /tmp/plex.deb || apt-get install -y -f
    rm /tmp/plex.deb
fi

apt-get clean
find /var/lib/apt/lists -type f -delete

# Install backup and restore commands.
# SKIP_SCRIPT_DOWNLOAD=1 when called from Dockerfile (Dockerfile COPYs them directly after this step).
if [[ "${SKIP_SCRIPT_DOWNLOAD:-0}" != "1" ]]; then
    REPO_RAW="${REPO_RAW:-https://raw.githubusercontent.com/argyle-labs/plex/main}"
    curl -fsSL "${REPO_RAW}/scripts/backup.sh" -o /usr/local/bin/backup
    curl -fsSL "${REPO_RAW}/scripts/restore.sh" -o /usr/local/bin/restore
    chmod +x /usr/local/bin/backup /usr/local/bin/restore
fi
