#!/bin/bash
# SYS-06: scripts/deploy.sh install-root against a test double for ssh and sudo (run with:
# bash scripts/tests/deploy_install_root.sh). The "Pi" is a temp directory; nothing outside it
# is touched, and the real installer is not run (the double records that it would be).
set -u

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
FAILS=0; PASSES=0
pass() { PASSES=$((PASSES + 1)); echo "ok   - $1"; }
fail() { FAILS=$((FAILS + 1)); echo "FAIL - $1" >&2; }

if [ "$(id -u)" -ne 0 ]; then
    echo "skip - deploy_install_root tests need root (the double extracts root-owned files)"; exit 0
fi

FAKEBIN="$WORK/fakebin"; mkdir -p "$FAKEBIN"
# ssh double: run the remote command locally (stdin passes through) in its own "session", after
# logging it. The session id scopes the fake sudo credential cache like a real remote login.
cat > "$FAKEBIN/ssh" <<'SH'
#!/bin/bash
cmd="${@: -1}"
printf 'SSH %s\n' "$cmd" >> "$FAKE_LOG"
export FAKE_SESSION=$$
exec bash -c "$cmd"
SH
# sudo double with the real flag semantics that matter: FAKE_SUDO_MODE=nopass never reads stdin
# and never prompts (NOPASSWD); =pass needs the password line on stdin via -S (once per session
# credential cache, cleared by -k); -n fails when a password would be needed. It never runs the
# real installer, and can tamper with what tar extracted.
cat > "$FAKEBIN/sudo" <<'SH'
#!/bin/bash
nonint=0; stdin_pw=0; validate=0; kill_cache=0
cache="$FAKE_STATE/cache.${FAKE_SESSION:-0}"
while [ $# -gt 0 ]; do
    case "$1" in
        -n) nonint=1; shift ;;
        -S) stdin_pw=1; shift ;;
        -p) shift 2 ;;
        -v) validate=1; shift ;;
        -k) kill_cache=1; shift ;;
        *) break ;;
    esac
done
printf 'SUDO flags(n=%s S=%s v=%s k=%s) %s\n' "$nonint" "$stdin_pw" "$validate" "$kill_cache" "$*" >> "$FAKE_LOG"
[ "$kill_cache" -eq 1 ] && rm -f "$cache"
if [ $# -eq 0 ] && [ "$validate" -eq 0 ]; then exit 0; fi
if [ "$FAKE_SUDO_MODE" = pass ] && [ ! -e "$cache" ]; then
    [ "$nonint" -eq 0 ] || { echo "sudo: a password is required" >&2; exit 1; }
    [ "$stdin_pw" -eq 1 ] || { echo "sudo: no tty present" >&2; exit 1; }
    IFS= read -r pw
    [ "$pw" = "$FAKE_PASSWORD" ] || { echo "sudo: wrong password line: '$pw'" >&2; exit 1; }
    : > "$cache"
fi
[ "$validate" -eq 1 ] && exit 0
case "$1" in
    */bin/plippa-install-root)
        echo INSTALLER-RUN >> "$FAKE_LOG"
        [ -z "${FAKE_INSTALLER_FAIL:-}" ] || { echo "installer: boom" >&2; exit 1; }
        exit 0 ;;
esac
"$@"; rc=$?
if [ "$1" = tar ] && [ "$2" = -x ]; then
    dest="${@: -1}"
    [ -n "${FAKE_CORRUPT:-}" ] && echo tamper >> "$dest/polkit/50-plippa-power.rules"
    [ -n "${FAKE_EXTRA:-}" ] && echo evil > "$dest/polkit/99-evil.rules"
    [ -n "${FAKE_WRITABLE:-}" ] && chmod 666 "$dest/polkit/50-plippa-network.rules"
fi
exit $rc
SH
# mv double: fails the final swap (R.new -> R) when FAKE_MV_FAIL is set.
cat > "$FAKEBIN/mv" <<'SH'
#!/bin/bash
if [ -n "${FAKE_MV_FAIL:-}" ] && [ "${@: -1}" = "$PLIPPA_ROOT_TREE" ] && [ "${@: -2:1}" = "$PLIPPA_ROOT_TREE.new" ]; then echo "mv: injected failure" >&2; exit 1; fi
exec /bin/mv "$@"
SH
chmod +x "$FAKEBIN/ssh" "$FAKEBIN/sudo" "$FAKEBIN/mv"

# run_deploy <name> [VAR=value ...]: sets up a fresh fake Pi, with an OLD marker in the live tree.
run_deploy() {
    local case_dir="$WORK/$1"; shift
    mkdir -p "$case_dir/conf" "$case_dir/state" "$case_dir/pi/usr/lib/plippa"
    [ -n "${PASSFILE:-}" ] && echo hunter2 > "$case_dir/conf/password"
    echo old > "$case_dir/pi/usr/lib/plippa/OLD"
    : > "$case_dir/log"
    CASE="$case_dir"
    OUT="$(env PATH="$FAKEBIN:$PATH" PLIPPA_DEPLOY_DIR="$case_dir/conf" PLIPPA_DEPLOY_SSH="$FAKEBIN/ssh" \
        PLIPPA_ROOT_TREE="$case_dir/pi/usr/lib/plippa" FAKE_LOG="$case_dir/log" FAKE_PASSWORD=hunter2 \
        FAKE_STATE="$case_dir/state" FAKE_SUDO_MODE="${SUDO_MODE_UNDER_TEST:-nopass}" \
        "$@" bash "$ROOT/scripts/deploy.sh" install-root 2>&1)"
    RC=$?
}

check_payload_landed() { # $1 = tree
    local t="$1" B="$ROOT/boards/prototypes/plippa-D-poc2" ok=1
    cmp -s "$ROOT/scripts/root/bin/plippa-install-root" "$t/bin/plippa-install-root" || ok=0
    cmp -s "$ROOT/scripts/root/audio/install.sh" "$t/audio/install.sh" || ok=0
    cmp -s "$ROOT/scripts/root/audio/retire-system-audio.sh" "$t/audio/retire-system-audio.sh" || ok=0
    cmp -s "$B/50-plippa-power.rules" "$t/polkit/50-plippa-power.rules" || ok=0
    cmp -s "$B/50-plippa-network.rules" "$t/polkit/50-plippa-network.rules" || ok=0
    cmp -s "$B/plippa.service" "$t/systemd/plippa.service" || ok=0
    cmp -s "$B/audio/pipewire/51-plippa-latency.conf" "$t/audio/pipewire/51-plippa-latency.conf" || ok=0
    cmp -s "$B/audio/radio/99-plippa.conf" "$t/audio/radio/99-plippa.conf" || ok=0
    cmp -s "$ROOT/scripts/root/udev/99-ws281x-mem.rules" "$t/udev/99-ws281x-mem.rules" || ok=0
    [ -x "$t/bin/plippa-install-root" ] || ok=0
    [ -z "$(find "$t" \( ! -user root -o -perm /022 \) -print -quit)" ] || ok=0
    [ ! -e "$t/OLD" ] || ok=0
    [ "$ok" -eq 1 ]
}

echo "== deploy_install_root_streams_and_verifies =="

# The sudo situations: NOPASSWD without a password file, NOPASSWD *with* a password file (the
# old code prepended the password line to the tar stream, which sudo never read), password sudo.
for mode in nopass nopass-with-passfile pass; do
    SUDO_MODE_UNDER_TEST=nopass; PASSFILE=""
    [ "$mode" = nopass-with-passfile ] && PASSFILE=1
    [ "$mode" = pass ] && { SUDO_MODE_UNDER_TEST=pass; PASSFILE=1; }
    export SUDO_MODE_UNDER_TEST PASSFILE
    run_deploy "$mode"
    if [ "$RC" -eq 0 ] && check_payload_landed "$CASE/pi/usr/lib/plippa" \
        && [ "$(grep -c INSTALLER-RUN "$CASE/log")" -eq 1 ] && [ ! -e "$CASE/pi/usr/lib/plippa.new" ] \
        && [ ! -e "$CASE/pi/usr/lib/plippa.old" ]; then
        pass "[$mode] payload streamed, verified, swapped in, installer run once, .old removed"
    else
        fail "[$mode] payload streamed, verified, swapped in, installer run once (rc=$RC)"; echo "$OUT" | tail -5 >&2
    fi
    # No plip-writable intermediate: every remote command only names the root-owned tree.
    if ! grep -E 'home/plip|projects|rsync' "$CASE/log" >/dev/null \
        && ! grep -E '^SSH' "$CASE/log" | grep -vE "$CASE/pi/usr/lib/plippa|sudo -k; sudo -n true" >/dev/null; then
        pass "[$mode] no remote command touches a plip-writable path"
    else
        fail "[$mode] a remote command touched a path outside the root-owned tree"; grep -vE "$CASE/pi/usr/lib/plippa" "$CASE/log" >&2
    fi
done
# In the NOPASSWD runs sudo is never fed a password and in the password run it is primed first.
grep -E 'tar -x|sha256sum' "$WORK/nopass-with-passfile/log" | grep -q 'S=1' \
    && fail "NOPASSWD sudo must not be given -S/a password line" \
    || pass "NOPASSWD: the streaming commands run with sudo -n and no password on the stream"
grep -q 'flags(n=0 S=1 v=1' "$WORK/pass/log" \
    && pass "password sudo is primed with -S -v, then the stream commands run under -n" \
    || fail "password sudo should be primed with sudo -S -v"
SUDO_MODE_UNDER_TEST=nopass; PASSFILE=""; export SUDO_MODE_UNDER_TEST PASSFILE

for tamper in FAKE_CORRUPT FAKE_EXTRA FAKE_WRITABLE; do
    run_deploy "$tamper" "$tamper=1"
    if [ "$RC" -ne 0 ] && ! grep -q INSTALLER-RUN "$CASE/log" && [ -e "$CASE/pi/usr/lib/plippa/OLD" ]; then
        pass "[$tamper] a tampered/mismatching landing aborts before the swap and the installer"
    else
        fail "[$tamper] a tampered landing must abort (rc=$RC)"; echo "$OUT" | tail -5 >&2
    fi
done

# Rollback: a failing swap, and a failing installer, both leave the previous tree in place.
run_deploy swapfail FAKE_MV_FAIL=1
if [ "$RC" -ne 0 ] && [ -e "$CASE/pi/usr/lib/plippa/OLD" ] && [ ! -e "$CASE/pi/usr/lib/plippa.old" ] \
    && ! grep -q INSTALLER-RUN "$CASE/log" && echo "$OUT" | grep -q "previous .* was restored"; then
    pass "a failing swap restores the previous tree and does not run the installer"
else
    fail "a failing swap must restore the previous tree (rc=$RC)"; echo "$OUT" | tail -5 >&2
fi
run_deploy installfail FAKE_INSTALLER_FAIL=1
if [ "$RC" -ne 0 ] && [ -e "$CASE/pi/usr/lib/plippa/OLD" ] && [ ! -e "$CASE/pi/usr/lib/plippa/bin" ] \
    && [ ! -e "$CASE/pi/usr/lib/plippa.old" ] && echo "$OUT" | grep -q "INSTALLER FAILED; the previous .* was restored"; then
    pass "a failing installer restores the previous tree loudly (R.old kept until it succeeded)"
else
    fail "a failing installer must restore the previous tree (rc=$RC)"; echo "$OUT" | tail -5 >&2
fi

echo "== $PASSES passed, $FAILS failed =="
[ "$FAILS" -eq 0 ]
