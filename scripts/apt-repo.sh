#!/usr/bin/env bash
set -euo pipefail

DRY_RUN="${DRY_RUN:-0}"
KEY_URL="https://downloads.plex.tv/plex-keys/PlexSign.v2.key"
KEYRING="/usr/share/keyrings/plexmediaserver.v2.gpg"
OLD_KEYRING="/usr/share/keyrings/plex-archive-keyring.gpg"
LIST="/etc/apt/sources.list.d/plex.list"
LINE="deb [signed-by=${KEYRING}] https://repo.plex.tv/deb/ public main"

act() {
    echo "$1"
    if [ "$DRY_RUN" != "1" ]; then
        shift
        "$@"
    fi
}

write_list() {
    printf '%s\n' "$LINE" > "$LIST"
}

fetch_key() {
    curl -fsSL "$KEY_URL" | gpg --batch --yes --dearmor -o "$KEYRING"
}

changed=0

for f in /etc/apt/sources.list.d/plexmediaserver.list /etc/apt/sources.list.d/plex.sources; do
    if [ -e "$f" ]; then
        act "remove $f" rm -f "$f"
        changed=1
    fi
done

if [ -e "$OLD_KEYRING" ]; then
    act "remove $OLD_KEYRING" rm -f "$OLD_KEYRING"
    changed=1
fi

if [ ! -s "$KEYRING" ]; then
    act "install $KEYRING" fetch_key
    changed=1
fi

if [ ! -f "$LIST" ] || [ "$(cat "$LIST")" != "$LINE" ]; then
    act "write $LIST" write_list
    changed=1
fi

if [ "$changed" = "1" ]; then
    act "apt-get update" apt-get update -qq
fi
