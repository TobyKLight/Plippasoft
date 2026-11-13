#!/bin/bash
# Install the plippa-D-poc2 device audio files (AUDIO_OPS_01).
#
# Installs the PipeWire quantum and the radio identity/power drop-in, retires the
# old POC1 system audio target, then prints the operator verification block.
# Safe to re-run. It does not build, install packages, or touch plippa.service.
#
# SYS-06: this runs as root, so it only ever runs from the root-owned tree
# /usr/lib/plippa/audio, started by /usr/lib/plippa/bin/plippa-install-root after that tool has
# audited the tree. Never run a copy from a user-writable checkout with sudo.
# DESTDIR (tests only): prefix for every destination path; systemctl is then skipped.
set -eu

D="${DESTDIR:-}"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
LATENCY_SRC="$SCRIPT_DIR/pipewire/51-plippa-latency.conf"
LATENCY_DST="$D"/etc/pipewire/pipewire.conf.d/51-plippa-latency.conf
RADIO_SRC="$SCRIPT_DIR/radio/99-plippa.conf"
RADIO_DST="$D"/etc/bluetooth/main.conf.d/99-plippa.conf
RADIO_MARKER="# AUDIO_OPS_01: Plippa radio settings"
MAIN_CONF="$D"/etc/bluetooth/main.conf

if [ "$(id -u)" -ne 0 ]; then
    echo "install.sh: run as root, via /usr/lib/plippa/bin/plippa-install-root" >&2
    exit 1
fi

run() {
    echo "+ $*"
    "$@"
}

main_conf_d_supported() {
    if command -v bluetoothd >/dev/null 2>&1; then
        if grep -aq 'main.conf.d' "$(command -v bluetoothd)"; then
            return 0
        fi
    fi
    for man in /usr/share/man/man5/main.conf.5.gz /usr/share/man/man5/main.conf.5; do
        [ -e "$man" ] || continue
        case "$man" in
            *.gz)
                if gzip -cd "$man" 2>/dev/null | grep -q 'main.conf.d'; then
                    return 0
                fi
                ;;
            *)
                if grep -q 'main.conf.d' "$man" 2>/dev/null; then
                    return 0
                fi
                ;;
        esac
    done
    return 1
}

echo "== PipeWire quantum =="
run install -d "$D"/etc/pipewire/pipewire.conf.d
run install -m 644 "$LATENCY_SRC" "$LATENCY_DST"

echo "== Radio name and auto power =="
if main_conf_d_supported; then
    run install -d "$D"/etc/bluetooth/main.conf.d
    run install -m 644 "$RADIO_SRC" "$RADIO_DST"
    echo "radio config: installed $RADIO_DST (main.conf.d is supported)"
else
    run install -d "$D"/etc/bluetooth
    if [ -e "$MAIN_CONF" ] && grep -qF "$RADIO_MARKER" "$MAIN_CONF"; then
        echo "radio config: Plippa block already appended to $MAIN_CONF"
    else
        echo "+ append Plippa block to $MAIN_CONF"
        {
            printf '\n%s\n' "$RADIO_MARKER"
            printf '[General]\nName = Plippa\n\n[Policy]\nAutoEnable = true\n'
        } >>"$MAIN_CONF"
        echo "radio config: appended Plippa block to $MAIN_CONF (no main.conf.d support)"
    fi
fi

echo "== Retire the old system audio target =="
run "$SCRIPT_DIR/retire-system-audio.sh"

echo "== Operator verification (after a reboot) =="
cat <<'EOF'
sudo reboot
# then:
pw-metadata -n settings | grep quantum          # expect: clock.quantum = 256
pw-top                                          # expect: QUANT 256, ERR 0 while playing
cat /sys/class/rfkill/rfkill0/soft              # expect: 0
cat /var/lib/systemd/rfkill/platform-soc:bluetooth  # expect: 0
systemctl is-enabled plippa-audio.target        # expect: not-found (or disabled)
systemctl status plippa --no-pager | head -5
journalctl -u plippa -n 40 --no-pager | grep -E "audio target|IPC listening|notice"
pactl list short sinks                          # expect: one sink until an accessory connects

Expect the engine log line: audio target alsa_output.platform-soc_sound.stereo-fallback
(profile low-latency, generation N), and the TFT System > Audio list showing it.
EOF
