FROM debian:12-slim

ARG PLEX_VERSION=latest

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

# System deps + GPU drivers (architecture-aware)
RUN apt-get update && apt-get install -y --no-install-recommends \
      ca-certificates \
      curl \
      gnupg \
      udev \
      xmlstarlet \
      uuid-runtime \
      ocl-icd-libopencl1 \
      mesa-va-drivers \
      gosu \
      vainfo \
    && apt-get clean \
    && find /var/lib/apt/lists -type f -delete

# Intel drivers are x86-only (iHD Gen8+, i965 Gen4-9, OpenCL for tone mapping)
RUN ARCH=$(dpkg --print-architecture) && \
    if [ "$ARCH" = "amd64" ]; then \
      apt-get update && apt-get install -y --no-install-recommends \
        intel-media-va-driver-non-free \
        i965-va-driver \
        intel-opencl-icd \
      && apt-get clean \
      && find /var/lib/apt/lists -type f -delete; \
    fi

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
      dpkg -i /tmp/plex.deb || apt-get install -y -f && \
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
