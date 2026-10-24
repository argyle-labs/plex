#!/usr/bin/env bash
set -euo pipefail

DRY_RUN="${DRY_RUN:-0}"
KEY_URL="https://downloads.plex.tv/plex-keys/PlexSign.v2.key"
KEYRING="/usr/share/keyrings/plexmediaserver.v2.gpg"
OLD_KEYRING="/usr/share/keyrings/plex-archive-keyring.gpg"
LIST="/etc/apt/sources.list.d/plex.list"
LEGACY_LISTS=(/etc/apt/sources.list.d/plexmediaserver.list /etc/apt/sources.list.d/plex.sources)
LINE="deb [signed-by=${KEYRING}] https://repo.plex.tv/deb/ public main"

TMP_FILES=()
cleanup() {
    if [ "${#TMP_FILES[@]}" -gt 0 ]; then
        rm -f "${TMP_FILES[@]}"
    fi
}
trap cleanup EXIT

act() {
    echo "$1"
    if [ "$DRY_RUN" != "1" ]; then
        shift
        "$@"
    fi
}

key_valid() {
    [ -s "$1" ] && gpg --batch --show-keys "$1" >/dev/null 2>&1
}

fetch_key() {
    local raw dearmored
    raw="$(mktemp)"
    dearmored="$(mktemp "${KEYRING}.XXXXXX")"
    TMP_FILES+=("$raw" "$dearmored")
    curl -fsSL --retry 3 --max-time 30 -o "$raw" "$KEY_URL"
    gpg --batch --yes --dearmor -o "$dearmored" "$raw"
    key_valid "$dearmored"
    chmod 0644 "$dearmored"
    mv -f "$dearmored" "$KEYRING"
}

apt_update() {
    apt-get update -qq >&2
}

write_list() {
    local tmp
    tmp="$(mktemp "${LIST}.XXXXXX")"
    TMP_FILES+=("$tmp")
    printf '%s\n' "$LINE" > "$tmp"
    chmod 0644 "$tmp"
    mv -f "$tmp" "$LIST"
}

changed=0

if ! key_valid "$KEYRING"; then
    act "install $KEYRING" fetch_key
    changed=1
fi

if [ ! -f "$LIST" ] || [ "$(cat "$LIST")" != "$LINE" ]; then
    act "write $LIST" write_list
    changed=1
fi

for f in "${LEGACY_LISTS[@]}" "$OLD_KEYRING"; do
    if [ -e "$f" ]; then
        act "remove $f" rm -f "$f"
        changed=1
    fi
done

if [ "$changed" = "1" ]; then
    act "apt-get update" apt_update
fi
