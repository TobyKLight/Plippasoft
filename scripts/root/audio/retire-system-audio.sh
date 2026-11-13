#!/bin/bash
# Retire the POC1 system audio stack on plippa-D-poc2 (AUDIO_OPS_01).
#
# Disables/stops the old target and its wanted units, then moves their unit files
# from /etc/systemd/system into /etc/systemd/system/plippa-audio-retired/. Vendor
# units under /lib/systemd/system are disabled/stopped but left in place. Nothing
# is deleted: the retired files stay as a trail. Safe to re-run.
#
# SYS-06: runs as root only from the audited, root-owned /usr/lib/plippa/audio (started by
# plippa-install-root). DESTDIR (tests only) prefixes the unit directories and skips systemctl.
set -eu

D="${DESTDIR:-}"
UNIT_DIR="$D"/etc/systemd/system
VENDOR_DIR="$D"/lib/systemd/system
RETIRED_DIR="$UNIT_DIR/plippa-audio-retired"
TARGET=plippa-audio.target
UNITS=(
    plippa-audio-runtime.service
    plippa-pipewire.socket
    plippa-pipewire.service
    plippa-wireplumber.service
    plippa-pipewire-pulse.socket
    plippa-pipewire-pulse.service
    plippa-audio-setup.service
)

if [ "$(id -u)" -ne 0 ]; then
    echo "retire-system-audio.sh: run as root, via /usr/lib/plippa/bin/plippa-install-root" >&2
    exit 1
fi

run() {
    echo "+ $*"
    "$@"
}

# systemctl, except under DESTDIR where there is no running systemd to talk to.
sc() {
    if [ -n "$D" ]; then
        echo "(DESTDIR set: skipping systemctl $*)"
        [ "$1" != cat ]
    else
        systemctl "$@"
    fi
}

unit_known() {
    sc cat "$1" >/dev/null 2>&1
}

echo "Retiring the POC1 system audio stack (unit files are moved, never deleted)."

if unit_known "$TARGET"; then
    run sc disable --now "$TARGET"
else
    echo "skip: $TARGET is not installed"
fi

for unit in "${UNITS[@]}"; do
    if unit_known "$unit"; then
        run sc disable --now "$unit"
    else
        echo "skip: $unit is not installed"
    fi
done

run mkdir -p "$RETIRED_DIR"

move_unit() {
    src="$UNIT_DIR/$1"
    dst="$RETIRED_DIR/$1"
    vendor="$VENDOR_DIR/$1"
    if [ -e "$src" ] || [ -L "$src" ]; then
        if [ -e "$dst" ] || [ -L "$dst" ]; then
            echo "skip: $dst already exists; leaving $src in place"
        else
            run mv "$src" "$dst"
        fi
    elif [ -e "$vendor" ] || [ -L "$vendor" ]; then
        echo "left in place: $vendor (vendor unit disabled/stopped, not moved)"
    else
        echo "skip: $1 is not installed"
    fi
}

move_unit "$TARGET"
for unit in "${UNITS[@]}"; do
    move_unit "$unit"
done

run sc daemon-reload

echo "Done. Retired unit files are in $RETIRED_DIR."
echo "Vendor units under $VENDOR_DIR were disabled/stopped and left in place."
echo "Rollback: move them back to $UNIT_DIR, then: systemctl enable $TARGET"
