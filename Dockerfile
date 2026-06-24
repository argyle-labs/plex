FROM debian:12-slim

ARG PLEX_VERSION=latest
ARG TARGETARCH=amd64

ENV DEBIAN_FRONTEND=noninteractive \
    PLEX_MEDIA_SERVER_HOME=/usr/lib/plexmediaserver \
    PLEX_MEDIA_SERVER_MAX_PLUGIN_CONNECTIONS=32 \
    PLEX_MEDIA_SERVER_TMPDIR=/tmp \
    PLEX_UID=1000 \
    PLEX_GID=1000 \
    LIBVA_DRIVER_NAME=auto

# Timezone from host via /etc/localtime mount — tzdata needed as fallback
RUN apt-get update && apt-get install -y --no-install-recommends tzdata \
    && apt-get clean && find /var/lib/apt/lists -type f -delete

# Enable non-free and non-free-firmware for intel-media-va-driver-non-free
RUN sed -i 's/^Components: main$/Components: main contrib non-free non-free-firmware/' \
      /etc/apt/sources.list.d/debian.sources

# System deps + Intel VAAPI drivers (both generations) + OpenCL
RUN apt-get update && apt-get install -y --no-install-recommends \
      ca-certificates \
      curl \
      gnupg \
      udev \
      xmlstarlet \
      uuid-runtime \
      # Intel VA-API: Gen8+ (Broadwell → Arrow Lake) — iHD driver, non-free
      intel-media-va-driver-non-free \
      # Intel VA-API: Gen4–9 (HD Graphics 2000–6000) — open source i965 driver
      i965-va-driver \
      # OpenCL — required for HDR→SDR tone mapping via tonemap_vaapi
      intel-opencl-icd \
      ocl-icd-libopencl1 \
      # AMD VA-API: GCN+ (RX 400 series and newer) via Mesa radeonsi driver
      mesa-va-drivers \
      # Privilege drop helper
      gosu \
      # Diagnostics (small, useful for debugging)
      vainfo \
    && apt-get clean \
    && find /var/lib/apt/lists -type f -delete

# Install Plex — pinned version via direct .deb, or latest via apt
RUN ARCH=$(dpkg --print-architecture) && \
    if [ "${PLEX_VERSION}" = "latest" ]; then \
      curl -fsSL https://downloads.plex.tv/plex-keys/PlexSign.key \
        | gpg --dearmor -o /usr/share/keyrings/plex-archive-keyring.gpg && \
      echo "deb [signed-by=/usr/share/keyrings/plex-archive-keyring.gpg] https://downloads.plex.tv/repo/deb public main" \
        > /etc/apt/sources.list.d/plexmediaserver.list && \
      apt-get update && \
      apt-get install -y --no-install-recommends plexmediaserver; \
    else \
      curl -fsSL \
        "https://downloads.plex.tv/plex-media-server-new/${PLEX_VERSION}/debian/plexmediaserver_${PLEX_VERSION}_${ARCH}.deb" \
        -o /tmp/plex.deb && \
      dpkg -i /tmp/plex.deb && \
      rm /tmp/plex.deb; \
    fi \
    && apt-get clean \
    && find /var/lib/apt/lists -type f -delete

# Runtime user config — plex uid/gid are set via env at startup
COPY scripts/entrypoint.sh /entrypoint.sh
RUN chmod +x /entrypoint.sh

EXPOSE 32400

VOLUME ["/config", "/transcode"]

HEALTHCHECK --interval=30s --timeout=10s --start-period=60s --retries=3 \
    CMD curl -fsSL http://localhost:32400/identity > /dev/null || exit 1

ENTRYPOINT ["/entrypoint.sh"]
