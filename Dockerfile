FROM debian:12-slim

ARG PLEX_VERSION=latest

ENV DEBIAN_FRONTEND=noninteractive \
    PLEX_MEDIA_SERVER_HOME=/usr/lib/plexmediaserver \
    PLEX_MEDIA_SERVER_MAX_PLUGIN_CONNECTIONS=32 \
    PLEX_MEDIA_SERVER_TMPDIR=/tmp \
    PLEX_UID=1000 \
    PLEX_GID=1000 \
    LIBVA_DRIVER_NAME=auto

COPY scripts/install.sh /tmp/install.sh
RUN chmod +x /tmp/install.sh && SKIP_SCRIPT_DOWNLOAD=1 /tmp/install.sh "${PLEX_VERSION}" && rm /tmp/install.sh

COPY scripts/entrypoint.sh /entrypoint.sh
COPY scripts/backup.sh /usr/local/bin/backup
COPY scripts/restore.sh /usr/local/bin/restore
RUN chmod +x /entrypoint.sh /usr/local/bin/backup /usr/local/bin/restore

EXPOSE 32400

VOLUME ["/config", "/transcode"]

HEALTHCHECK --interval=30s --timeout=10s --start-period=60s --retries=3 \
    CMD curl -fsSL http://localhost:32400/identity > /dev/null || exit 1

ENTRYPOINT ["/entrypoint.sh"]
