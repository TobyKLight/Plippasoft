#!/bin/bash
# SYS-06: tests for scripts/root/bin/plippa-install-root (run with: bash scripts/tests/install_root.sh).
# Uses DESTDIR, so nothing outside a temp directory is touched. Parts that need real root
# (chown to another uid, installing root:root files) are skipped with a message when not root.
set -u

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
INSTALLER="$ROOT/scripts/root/bin/plippa-install-root"
BOARD="$ROOT/boards/prototypes/plippa-D-poc2"
IS_ROOT=0; [ "$(id -u)" -eq 0 ] && IS_ROOT=1
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
FAILS=0; PASSES=0

pass() { PASSES=$((PASSES + 1)); echo "ok   - $1"; }
fail() { FAILS=$((FAILS + 1)); echo "FAIL - $1" >&2; }
skip() { echo "skip - $1 (needs root)"; }

# make_tree <destdir>: a payload tree exactly as deploy.sh lays it out, owned by the caller.
make_tree() {
    local d="$1" t="$1/usr/lib/plippa"
    mkdir -p "$t/bin" "$t/audio/pipewire" "$t/audio/radio" "$t/polkit" "$t/systemd" "$t/udev"
    install -m 0755 "$INSTALLER" "$t/bin/"
    install -m 0755 "$ROOT/scripts/root/audio/install.sh" "$ROOT/scripts/root/audio/retire-system-audio.sh" "$t/audio/"
    install -m 0644 "$BOARD/audio/pipewire/51-plippa-latency.conf" "$t/audio/pipewire/"
    install -m 0644 "$BOARD/audio/radio/99-plippa.conf" "$t/audio/radio/"
    install -m 0644 "$BOARD"/50-plippa-power.rules "$BOARD"/50-plippa-network.rules "$t/polkit/"
    install -m 0644 "$BOARD/plippa.service" "$t/systemd/"
    install -m 0644 "$ROOT/scripts/root/udev/99-ws281x-mem.rules" "$t/udev/"
    chmod 0755 "$d/usr" "$d/usr/lib" "$t" "$t"/*
}

new_dest() { local d; d="$(mktemp -d "$WORK/dest.XXXXXX")"; make_tree "$d"; echo "$d"; }

# run_installer <destdir> [env...]: prints combined output, returns the installer's status.
run_installer() {
    local d="$1"; shift
    env DESTDIR="$d" "$@" bash "$INSTALLER" 2>&1
}

# expect_refused <name> <destdir> [env...]: audit must fail with 3 and install nothing.
expect_refused() {
    local name="$1" d="$2"; shift 2
    local out rc
    out="$(run_installer "$d" "$@")"; rc=$?
    if [ "$rc" -eq 3 ] && [ ! -e "$d/etc" ] && echo "$out" | grep -q "AUDIT FAILED"; then
        pass "$name"
    else
        fail "$name (rc=$rc, etc exists: $([ -e "$d/etc" ] && echo yes || echo no))"; echo "$out" | tail -5 >&2
    fi
}

echo "== install_root_refuses_a_plip_writable_source_tree =="

# 1. The audit expects another owner than the files have: everything is "foreign".
d="$(new_dest)"
expect_refused "files not owned by the expected uid are refused" "$d" PLIPPA_ROOT_UID=$(( $(id -u) + 1 ))

# 2. A file owned by uid 1000 (the plip user) inside an otherwise root-owned tree.
if [ "$IS_ROOT" -eq 1 ]; then
    d="$(new_dest)"; chown -R root:root "$d/usr"
    chown 1000:1000 "$d/usr/lib/plippa/polkit/50-plippa-power.rules"
    expect_refused "a plip-owned polkit rule is refused" "$d"
    d="$(new_dest)"; chown -R root:root "$d/usr"
    chown 1000:1000 "$d/usr/lib/plippa/audio/install.sh"
    expect_refused "a plip-owned audio script is refused" "$d"
    d="$(new_dest)"; chown -R root:root "$d/usr"
    chown 1000:1000 "$d/usr/lib/plippa"
    expect_refused "a plip-owned tree root is refused" "$d"
else
    skip "chown-to-uid-1000 variants (the uid hook case above covers the audit)"
fi

# 3. group/world writable file, directory, and a writable ancestor.
d="$(new_dest)"; chmod g+w "$d/usr/lib/plippa/polkit/50-plippa-power.rules"
expect_refused "a group-writable file is refused" "$d" PLIPPA_ROOT_UID="$(id -u)"
d="$(new_dest)"; chmod o+w "$d/usr/lib/plippa/audio"
expect_refused "a world-writable directory is refused" "$d" PLIPPA_ROOT_UID="$(id -u)"
d="$(new_dest)"; chmod 0777 "$d/usr/lib"
expect_refused "a world-writable parent directory is refused" "$d" PLIPPA_ROOT_UID="$(id -u)"

# 4. setuid file.
d="$(new_dest)"; chmod u+s "$d/usr/lib/plippa/bin/plippa-install-root"
expect_refused "a setuid file is refused" "$d" PLIPPA_ROOT_UID="$(id -u)"

# 5. symlinked path component, and a symlink inside the tree.
d="$(new_dest)"; mv "$d/usr/lib/plippa" "$d/elsewhere"; ln -s ../../elsewhere "$d/usr/lib/plippa"
expect_refused "a symlinked path component is refused" "$d" PLIPPA_ROOT_UID="$(id -u)"
d="$(new_dest)"; ln -s /etc/passwd "$d/usr/lib/plippa/polkit/extra.rules"
expect_refused "a symlink inside the tree is refused" "$d" PLIPPA_ROOT_UID="$(id -u)"
d="$(new_dest)"; mv "$d/usr/lib/plippa/polkit/50-plippa-power.rules" "$WORK/real.rules"
ln -s "$WORK/real.rules" "$d/usr/lib/plippa/polkit/50-plippa-power.rules"
expect_refused "a payload file replaced by a symlink is refused" "$d" PLIPPA_ROOT_UID="$(id -u)"

# 6. A payload file missing.
d="$(new_dest)"; rm "$d/usr/lib/plippa/systemd/plippa.service"
expect_refused "a missing payload file is refused" "$d" PLIPPA_ROOT_UID="$(id -u)"

# 7. A clean tree installs the expected files.
if [ "$IS_ROOT" -eq 1 ]; then
    d="$(new_dest)"; chown -R root:root "$d/usr"
    out="$(run_installer "$d")"; rc=$?
    ok=1
    [ "$rc" -eq 0 ] || ok=0
    for f in etc/polkit-1/rules.d/50-plippa-power.rules etc/polkit-1/rules.d/50-plippa-network.rules \
             etc/systemd/system/plippa.service etc/pipewire/pipewire.conf.d/51-plippa-latency.conf \
             etc/udev/rules.d/99-ws281x-mem.rules; do
        got="$(stat -c '%U:%G %a' "$d/$f" 2>/dev/null)"
        [ "$got" = "root:root 644" ] || { ok=0; echo "   $f: '$got'" >&2; }
    done
    if [ ! -e "$d/etc/bluetooth/main.conf.d/99-plippa.conf" ] && ! grep -qs "AUDIO_OPS_01" "$d/etc/bluetooth/main.conf"; then
        ok=0; echo "   radio config not installed" >&2
    fi
    echo "$out" | grep -q "skipping systemctl daemon-reload" || { ok=0; echo "   daemon-reload not skipped under DESTDIR" >&2; }
    cmp -s "$ROOT/scripts/root/udev/99-ws281x-mem.rules" "$d/etc/udev/rules.d/99-ws281x-mem.rules" || { ok=0; echo "   udev rule not installed as written" >&2; }
    cmp -s "$BOARD/50-plippa-power.rules" "$d/etc/polkit-1/rules.d/50-plippa-power.rules" || ok=0
    [ "$ok" -eq 1 ] && pass "a clean root-owned tree installs the expected files at root:root 0644" \
        || { fail "a clean root-owned tree installs the expected files"; echo "$out" | tail -8 >&2; }
    # Re-run is safe.
    run_installer "$d" >/dev/null && pass "the installer is safe to re-run" || fail "the installer is safe to re-run"
else
    skip "clean-tree install (root:root ownership)"
fi

# 8. It never reads outside the tree: a decoy audio script next to a COPY of the installer in a
#    user-writable place must not run.
if [ "$IS_ROOT" -eq 1 ]; then
    d="$(new_dest)"; chown -R root:root "$d/usr"
    decoy="$WORK/decoy"; mkdir -p "$decoy/bin" "$decoy/audio"
    cp "$INSTALLER" "$decoy/bin/plippa-install-root"
    printf '#!/bin/bash\ntouch "%s/PWNED"\n' "$WORK" > "$decoy/audio/install.sh"
    chmod 755 "$decoy/audio/install.sh"
    run_installer "$d" >/dev/null 2>&1
    DESTDIR="$d" bash "$decoy/bin/plippa-install-root" >/dev/null 2>&1
    [ ! -e "$WORK/PWNED" ] && pass "files next to the installer copy are never executed (only the audited tree is read)" \
        || fail "a decoy next to the installer was executed"
fi

# 9. Without DESTDIR and without root the installer refuses to run at all.
if [ "$IS_ROOT" -eq 0 ]; then
    out="$(bash "$INSTALLER" 2>&1)"; rc=$?
    [ "$rc" -ne 0 ] && pass "refuses to run as non-root" || fail "refuses to run as non-root"
fi

echo "== $PASSES passed, $FAILS failed =="
[ "$FAILS" -eq 0 ]
