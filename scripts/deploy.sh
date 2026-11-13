#!/usr/bin/env bash
# Cross-compile Plippa for the Pi CM4 (aarch64) on this machine, then rsync to the Pi.
#   scripts/deploy.sh setup     one-time: store password, trust host, sync Pi sysroot (libs for linking)
#   scripts/deploy.sh sysroot   re-sync the Pi sysroot (after installing new libs on the Pi)
#   scripts/deploy.sh           build + push + setcap + install-root + restart service
#   scripts/deploy.sh install-root   install the root-owned tree /usr/lib/plippa on the Pi (polkit rules,
#                               audio files, plippa.service) through the audited plippa-install-root
#                               (SYS-06). Streams the payload over SSH into root-run tar, verifies the
#                               sha256 of every file, then runs the installer: nothing root reads is ever
#                               in a plip-writable directory. Also part of push.
#   scripts/deploy.sh build     build only
#   scripts/deploy.sh push      push last build only
#   scripts/deploy.sh eval <name>   build + push ONE eval app (package poc2_<name>, --features pulse) to
#                               the Pi; built through eval/Cargo.toml (a separate workspace)
#                               $PI_DIR/target/$PROFILE/ on the Pi: no service restart, no setcap.
#                               e.g. PROFILE=release scripts/deploy.sh eval warp_bench   (WARP_02)
# Needs on this machine: rustup target add aarch64-unknown-linux-gnu; sudo apt install gcc-aarch64-linux-gnu
#   g++-aarch64-linux-gnu libclang-dev python3 rsync (g++ is needed by the C++ stretcher, WARP_02/WARP_09).
# Performance numbers must come from a release build, and release is what the device runs: the
# base unit (boards/prototypes/plippa-D-poc2/plippa.service) says target/release/plippa; a drop-in
# in /etc/systemd/system/plippa.service.d/profile.conf may override ExecStart (e.g. a debug build).
# The launcher starts its siblings from its own directory, so that one path picks the profile for
# all four binaries: PROFILE must match the drop-in. Pushing the other profile "succeeds" (rsync
# ok) and changes nothing that runs, which just looks like the device ignored the deploy.
# Config + password live in ~/.config/plippa-deploy/ (never in the repo). See README "Deploy".
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CONF_DIR="${PLIPPA_DEPLOY_DIR:-$HOME/.config/plippa-deploy}"
PASS_FILE="$CONF_DIR/password"
[ -f "$CONF_DIR/deploy.env" ] && . "$CONF_DIR/deploy.env"

PI_HOST="${PI_HOST:-192.168.0.235}"
PI_USER="${PI_USER:-plip}"
PI_DIR="${PI_DIR:-/home/$PI_USER/projects/Plippasoft}"
PROFILE="${PROFILE:-release}"                # must match ExecStart in the Pi's unit (the drop-in:
                                             # plippa.service.d/profile.conf -> target/release/)
SYSROOT="${SYSROOT:-$CONF_DIR/sysroot}"
TARGET=aarch64-unknown-linux-gnu
OUT="$ROOT/target/cross"                      # inside gitignored /target/
# plippa-media-helper decodes untrusted media in a process of its own (SECFIX_06): the engine and
# the server start it as a sibling of their own executable, so it is pushed with them.
# plippa-leds is the only process holding CAP_SYS_RAWIO (SYS-02): the UI talks to it over
# $PLIPPA_IPC_DIR/leds.sock and holds no capability itself.
BINS=(plippa plippa-engine plippa-server plippa-ui plippa-media-helper plippa-leds)
PROFILE_FLAG=(); [ "$PROFILE" = release ] && PROFILE_FLAG=(--release)

# Password via SSH_ASKPASS helper: never in argv, env, or the repo. Unused when key auth works.
ASKPASS="$CONF_DIR/askpass.sh"
ssh_env() {
  if [ -r "$PASS_FILE" ]; then
    export SSH_ASKPASS="$ASKPASS" SSH_ASKPASS_REQUIRE=force DISPLAY="${DISPLAY:-none}"
  fi
}
SSH_OPTS=(-o StrictHostKeyChecking=accept-new -o ControlMaster=auto -o ControlPersist=60
          -o "ControlPath=$CONF_DIR/cm-%C")
SSH="${PLIPPA_DEPLOY_SSH:-ssh}"               # tests substitute a double; never set in real use
ROOT_TREE="${PLIPPA_ROOT_TREE:-/usr/lib/plippa}"  # root-owned install tree on the Pi (tests substitute a path)
pi() { ssh_env; "$SSH" "${SSH_OPTS[@]}" "$PI_USER@$PI_HOST" "$@"; }
pi_sudo() { # password to sudo via stdin
  if [ -r "$PASS_FILE" ]; then pi "sudo -S -p '' $*" < "$PASS_FILE"; else pi "sudo $*"; fi
}
SUDO_MODE=""   # nopass | pass | tty, decided once by sudo_mode
sudo_mode() { # how streaming commands get root; probed so the payload stream never depends on a guess
  [ -n "$SUDO_MODE" ] && return 0
  # `sudo -k` drops any cached credential first, so success means NOPASSWD, not a cache hit.
  if pi "sudo -k; sudo -n true" >/dev/null 2>&1; then SUDO_MODE=nopass
  elif [ -r "$PASS_FILE" ]; then SUDO_MODE=pass
  else SUDO_MODE=tty; fi
}
pi_sudo_stdin() { # like pi_sudo, but the command's stdin is the caller's stream (needs sudo_mode first)
  case "$SUDO_MODE" in
    nopass) pi "sudo -n $*" ;;   # sudo reads nothing: the stream is exactly the payload
    pass)   # The password is the first line, consumed by `sudo -S -v` (guaranteed to prompt after
            # `sudo -k`); the payload follows, and the command itself runs under the credential
            # that `-v` cached for this one session (`sudo -n`).
            { cat "$PASS_FILE"; cat; } | pi "sudo -k; sudo -S -p '' -v && sudo -n $*" ;;
    *)      pi "sudo $*" ;;
  esac
}

cmd_setup() {
  umask 077; mkdir -p "$CONF_DIR"
  if [ ! -f "$PASS_FILE" ]; then
    read -rsp "Pi password for $PI_USER@$PI_HOST (stored in $PASS_FILE, mode 600): " pw; echo
    printf '%s\n' "$pw" > "$PASS_FILE"; unset pw
  fi
  printf '#!/bin/sh\ncat "%s"\n' "$PASS_FILE" > "$ASKPASS"; chmod 700 "$ASKPASS"
  [ -f "$CONF_DIR/deploy.env" ] || cp "$ROOT/scripts/deploy/deploy.env.example" "$CONF_DIR/deploy.env"
  echo "Testing connection..."; pi 'echo connected; . /etc/os-release; echo "$PRETTY_NAME"; ldd --version | head -1'
  cmd_sysroot
  echo "Setup OK. Optional: ssh-copy-id $PI_USER@$PI_HOST  (then you can delete $PASS_FILE except for sudo/setcap)."
}

cmd_sysroot() { # Pi's libs/pkgconfig only (libpulse etc.), so linking matches the Pi's glibc
  command -v aarch64-linux-gnu-gcc >/dev/null || { echo "install: sudo apt install gcc-aarch64-linux-gnu"; exit 1; }
  mkdir -p "$SYSROOT/usr/lib"
  ssh_env
  rsync -rlt --copy-unsafe-links --delete --max-size=5m -e "ssh ${SSH_OPTS[*]}" \
    --include='*/' --include='*.so*' --include='*.a' --include='*.o' --include='*.pc' --exclude='*' \
    --prune-empty-dirs "$PI_USER@$PI_HOST:/usr/lib/aarch64-linux-gnu" "$SYSROOT/usr/lib/"
  mkdir -p "$SYSROOT/usr/share"
  rsync -rlt -e "ssh ${SSH_OPTS[*]}" "$PI_USER@$PI_HOST:/usr/share/pkgconfig" "$SYSROOT/usr/share/" 2>/dev/null || true
  rsync -tL -e "ssh ${SSH_OPTS[*]}" "$PI_USER@$PI_HOST:/usr/lib/ld-linux-aarch64.so.1" "$SYSROOT/usr/lib/"
  ln -sfn usr/lib "$SYSROOT/lib"   # libc.so/libm.so linker scripts use absolute /lib/... paths
  prune_sysroot
  du -sh "$SYSROOT"
}

prune_sysroot() { # keep only libpulse/libasound/libc + their NEEDED closure (Pi's lib dir is >1GB)
  python3 - "$SYSROOT/usr/lib/aarch64-linux-gnu" <<'PY'
import os, re, subprocess, shutil, sys
os.chdir(sys.argv[1])
def needed(f):
    out = subprocess.run(['aarch64-linux-gnu-readelf', '-d', f], capture_output=True, text=True).stdout
    return re.findall(r'Shared library: \[(.+?)\]', out)
keep, queue = set(), [f for f in os.listdir('.') if re.match(r'(libpulse|libasound|libc\.|libm\.|libmvec|libgcc_s|libstdc\+\+|libdl|libpthread|librt|libutil)', f)] + ['pulseaudio/' + f for f in os.listdir('pulseaudio') if f.startswith('libpulsecommon')]
while queue:
    f = queue.pop()
    if f in keep or not os.path.lexists(f): continue
    keep.add(f)
    if os.path.islink(f): queue.append(os.path.basename(os.readlink(f)))
    elif os.path.isfile(f) and open(f, 'rb').read(4) == b'\x7fELF': queue += needed(f)
for f in os.listdir('.'):
    if f in keep or f in ('pkgconfig', 'pulseaudio') or f.endswith(('.o', '.a')): continue
    if os.path.islink(f) and os.path.basename(os.readlink(f)) in keep: continue
    if os.path.isfile(f) and open(f, 'rb').read(4) != b'\x7fELF': continue
    shutil.rmtree(f) if os.path.isdir(f) and not os.path.islink(f) else os.remove(f)
PY
}

cross_env() { # one place for the cross-compile environment (used by build and eval)
  [ -d "$SYSROOT/usr/lib/aarch64-linux-gnu" ] || { echo "no sysroot — run: $0 setup"; exit 1; }
  local L="$SYSROOT/usr/lib/aarch64-linux-gnu"
  export CARGO_TARGET_DIR="$OUT" CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-2}"
  export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc
  export CC_aarch64_unknown_linux_gnu=aarch64-linux-gnu-gcc
  # C++ dependencies (Signalsmith Stretch, WARP_02/WARP_09): needs `sudo apt install g++-aarch64-linux-gnu`
  export CXX_aarch64_unknown_linux_gnu=aarch64-linux-gnu-g++
  command -v aarch64-linux-gnu-g++ >/dev/null || echo "warning: aarch64-linux-gnu-g++ not found — install: sudo apt install g++-aarch64-linux-gnu (needed for the stretcher / eval app)"
  export RUSTFLAGS="-C link-arg=--sysroot=$SYSROOT -C link-arg=-Wl,-rpath-link=$L -C link-arg=-Wl,-rpath-link=$L/pulseaudio"
  export PKG_CONFIG_ALLOW_CROSS=1 PKG_CONFIG_SYSROOT_DIR="$SYSROOT" \
         PKG_CONFIG_LIBDIR="$L/pkgconfig:$SYSROOT/usr/share/pkgconfig"
  export BINDGEN_EXTRA_CLANG_ARGS_aarch64_unknown_linux_gnu="--target=aarch64-linux-gnu -I/usr/aarch64-linux-gnu/include"
  # bindgen (rs_ws281x only; signalsmith-stretch ships pre-generated bindings) needs an unversioned libclang.so; libclang-dev may not be installed
  if ! ls /usr/lib/llvm-*/lib/libclang.so /usr/lib/x86_64-linux-gnu/libclang.so >/dev/null 2>&1; then
    local lc; lc="$(ls /usr/lib/llvm-*/lib/libclang-*.so.* /usr/lib/x86_64-linux-gnu/libclang-*.so.* 2>/dev/null | head -1)"
    [ -n "$lc" ] || { echo "install: sudo apt install libclang1 (or libclang-dev)"; exit 1; }
    mkdir -p "$CONF_DIR/libclang"; ln -sfn "$lc" "$CONF_DIR/libclang/libclang.so"
    export LIBCLANG_PATH="$CONF_DIR/libclang"
  fi
}

cmd_build() {
  cross_env
  cd "$ROOT"
  # Same per-package builds + features as README "Run — plippa-D-poc2", one at a time.
  cargo build --target "$TARGET" "${PROFILE_FLAG[@]}" -p plippa-server
  cargo build --target "$TARGET" "${PROFILE_FLAG[@]}" -p plippa-engine --features pulse
  cargo build --target "$TARGET" "${PROFILE_FLAG[@]}" -p plippa-ui --features poc2,accessory-2g4
  cargo build --target "$TARGET" "${PROFILE_FLAG[@]}" -p plippa-leds --features ws281x
  cargo build --target "$TARGET" "${PROFILE_FLAG[@]}" -p plippa
  cargo build --target "$TARGET" "${PROFILE_FLAG[@]}" -p plippa-audio --bin plippa-media-helper
}

cmd_eval() { # build + push ONE eval app; no service restart, no setcap
  local name="${1:-}"
  [ -n "$name" ] || { echo "usage: $0 eval <name>   (package poc2_<name> in eval/poc2/<name>, e.g. warp_bench)"; exit 2; }
  local dir="$ROOT/eval/poc2/$name"
  [ -f "$dir/Cargo.toml" ] || { echo "no eval app at eval/poc2/$name"; exit 2; }
  cross_env
  cd "$ROOT"
  local feat=(); grep -q '^pulse *=' "$dir/Cargo.toml" && feat=(--features pulse)
  # `eval/` is its own workspace (it is excluded from the root one), so the package
  # has to be built through its manifest rather than as a root-workspace member.
  cargo build --manifest-path "$ROOT/eval/Cargo.toml" --target "$TARGET" "${PROFILE_FLAG[@]}" -p "poc2_$name" "${feat[@]}"
  local src="$OUT/$TARGET/$PROFILE" bin="$name"
  [ -f "$src/$bin" ] || bin="poc2_$name"
  [ -f "$src/$bin" ] || { echo "missing $src/$name (or poc2_$name) after the build"; exit 1; }
  [ "$PROFILE" = release ] || echo "NOTE: debug build. Performance numbers are only valid from a release build: PROFILE=release $0 eval $name"
  ssh_env
  pi "mkdir -p '$PI_DIR/target/$PROFILE'"
  rsync -avz -e "ssh ${SSH_OPTS[*]}" "$src/$bin" "$PI_USER@$PI_HOST:$PI_DIR/target/$PROFILE/"
  echo "pushed $bin to $PI_USER@$PI_HOST:$PI_DIR/target/$PROFILE/$bin (service untouched)"
}

build_payload() { # $1 = empty directory: lay out what /usr/lib/plippa will hold (SYS-06)
  local st="$1" B="$ROOT/boards/prototypes/plippa-D-poc2" R="$ROOT/scripts/root"
  install -d -m 0755 "$st" "$st/bin" "$st/audio" "$st/audio/pipewire" "$st/audio/radio" "$st/polkit" "$st/systemd" "$st/udev"
  install -m 0755 "$R/bin/plippa-install-root" "$st/bin/"
  install -m 0755 "$R/audio/install.sh" "$R/audio/retire-system-audio.sh" "$st/audio/"
  install -m 0644 "$B/audio/pipewire/51-plippa-latency.conf" "$st/audio/pipewire/"
  install -m 0644 "$B/audio/radio/99-plippa.conf" "$st/audio/radio/"
  install -m 0644 "$B/50-plippa-power.rules" "$B/50-plippa-network.rules" "$st/polkit/"
  install -m 0644 "$B/plippa.service" "$st/systemd/"
  install -m 0644 "$R/udev/99-ws281x-mem.rules" "$st/udev/"
}

cmd_install_root() { # SYS-06: stream -> verify -> swap -> run the audited installer; any failure aborts
  local R="$ROOT_TREE" stage want got
  sudo_mode
  work="$(mktemp -d)"; trap 'rm -rf "$work"' EXIT   # global: the EXIT trap runs after this function returns
  stage="$work/tree"
  build_payload "$stage"
  (cd "$stage" && find . -type f -print0 | LC_ALL=C sort -z | xargs -0 sha256sum) > "$work/manifest"
  (cd "$stage" && find . -mindepth 1 | LC_ALL=C sort) > "$work/list"
  # Extract into a fresh root-owned directory next to the final one: root-run tar reads the
  # stream from the developer machine, so no plip-writable directory is ever involved.
  pi_sudo "rm -rf '$R.new'"
  pi_sudo "install -d -m 0755 -o root -g root '$R.new'"
  tar -C "$stage" --owner=0 --group=0 --numeric-owner -cf - . \
    | pi_sudo_stdin "tar -x --no-same-owner --no-same-permissions -C '$R.new'"
  # Verify what landed against the developer copy: every hash, no extra or missing path, and
  # root ownership with no group/world-writable bit.
  pi_sudo_stdin "sh -c \"cd '$R.new' && sha256sum -c --quiet --strict -\"" < "$work/manifest" \
    || { echo "install-root: sha256 mismatch in $R.new; nothing installed" >&2; exit 1; }
  got="$(pi_sudo "sh -c \"cd '$R.new' && find . -mindepth 1 | LC_ALL=C sort\"")"
  want="$(cat "$work/list")"
  [ "$got" = "$want" ] || { echo "install-root: file list in $R.new differs from the developer copy; nothing installed" >&2; exit 1; }
  got="$(pi_sudo "find '$R.new' \\( ! -user root -o -perm /022 \\) -print -quit")"
  [ -z "$got" ] || { echo "install-root: $got is not root-owned or is group/world-writable; nothing installed" >&2; exit 1; }
  # Swap in, keeping the previous tree as $R.old until the installer has succeeded.
  pi_sudo "sh -c \"rm -rf '$R.old' && { [ ! -e '$R' ] || mv '$R' '$R.old'; } && { mv '$R.new' '$R' || { [ ! -e '$R.old' ] || mv '$R.old' '$R'; exit 1; }; }\"" \
    || { echo "install-root: could not swap $R.new into place; the previous $R was restored" >&2; exit 1; }
  if ! pi_sudo "'$R/bin/plippa-install-root'"; then
    pi_sudo "sh -c \"if [ -e '$R.old' ]; then rm -rf '$R' && mv '$R.old' '$R'; fi\"" \
      && echo "install-root: INSTALLER FAILED; the previous $R was restored (anything the installer already wrote under /etc is as it left it: fix the cause and re-run install-root)" >&2 \
      || echo "install-root: INSTALLER FAILED and the previous $R could NOT be restored; inspect $R and $R.old by hand" >&2
    exit 1
  fi
  pi_sudo "rm -rf '$R.old'"
  echo "install-root: $R verified and installed"
}

cmd_push() {
  local src="$OUT/$TARGET/$PROFILE" files=()
  for b in "${BINS[@]}"; do [ -f "$src/$b" ] || { echo "missing $src/$b — run build"; exit 1; }; files+=("$src/$b"); done
  [ -f "$src/plippa-np" ] && files+=("$src/plippa-np")
  ssh_env
  pi "mkdir -p '$PI_DIR/target/$PROFILE'"
  rsync -avz -e "ssh ${SSH_OPTS[*]}" "${files[@]}" "$PI_USER@$PI_HOST:$PI_DIR/target/$PROFILE/"
  # Installed check: every binary, the media helper included, is there and executable.
  for b in "${BINS[@]}"; do
    pi "test -x '$PI_DIR/target/$PROFILE/$b'" || { echo "installed check: $b is missing or not executable on the Pi"; exit 1; }
  done
  echo "installed: ${BINS[*]}"
  # The engine loads its board profile and the UI sounds from $PI_DIR (PLIPPA_ROOT), not from
  # the binary: push them, so a Pi with no checkout (a fresh $PI_DIR) works.
  pi "mkdir -p '$PI_DIR/boards/prototypes/plippa-D-poc2/config' '$PI_DIR/assets/plippaSFX'"
  rsync -avz -e "ssh ${SSH_OPTS[*]}" \
    "$ROOT/boards/prototypes/plippa-D-poc2/config/board.toml" \
    "$PI_USER@$PI_HOST:$PI_DIR/boards/prototypes/plippa-D-poc2/config/"
  rsync -avz --delete -e "ssh ${SSH_OPTS[*]}" \
    "$ROOT/assets/plippaSFX/" "$PI_USER@$PI_HOST:$PI_DIR/assets/plippaSFX/"
  # Capability discipline (SYS-02): the LED helper alone holds CAP_SYS_RAWIO. rsync rewrites the
  # binary and drops the capability, so it is re-applied on every push and then verified; the UI
  # is stripped once so a capability left by an older deploy cannot linger.
  # `getcap` prints `/path name=ep` on libcap 2.75 and `/path name = ep` on older releases:
  # strip the path and any spaces so both forms compare as `cap_sys_rawio=ep`. (It lives in
  # /usr/sbin, which a non-login SSH shell does not have on PATH, hence `pi_sudo`.)
  ver_cap() { pi_sudo "getcap '$1'" 2>/dev/null | sed -e 's/^[^ ]*//' -e 's/[[:space:]]//g'; }
  pi_sudo "setcap cap_sys_rawio+ep '$PI_DIR/target/$PROFILE/plippa-leds'" \
    || { echo "setcap failed on plippa-leds — LED output will be dark" >&2; exit 1; }
  pi_sudo "setcap -r '$PI_DIR/target/$PROFILE/plippa-ui'" 2>/dev/null || true
  got="$(ver_cap "$PI_DIR/target/$PROFILE/plippa-leds")"
  [ "$got" = "cap_sys_rawio=ep" ] \
    || { echo "capability check: plippa-leds has '$got', expected 'cap_sys_rawio=ep'" >&2; exit 1; }
  got="$(ver_cap "$PI_DIR/target/$PROFILE/plippa-ui")"
  [ -z "$got" ] \
    || { echo "capability check: plippa-ui still has '$got'; it must hold none" >&2; exit 1; }
  # Polkit rules, audio files and plippa.service go to the root-owned tree and are installed from
  # there (never from the plip-writable checkout).
  cmd_install_root
  # Store credentials live here (BCS series). The server runs as $PI_USER, so the directory
  # must exist and belong to it: it refuses to write anywhere else and the store feature
  # degrades to "unavailable" rather than running half-configured.
  pi_sudo "install -d -m 0700 -o $PI_USER -g $PI_USER /var/lib/plippa/secrets"
  pi_sudo "systemctl restart plippa.service" || echo "(service restart skipped/failed)"
  # Verify the RUNNING processes: a file capability can be right while the started process
  # silently lacks it (or a stale UI process still holds one). Bit 17 is CAP_SYS_RAWIO.
  # A process that has not exec'd yet (a Pi can take several seconds to get there) is not a
  # capability failure, so poll with a deadline instead of guessing with a fixed sleep; a process
  # that is running with the wrong capability fails at once.
  check_capeff() { # $1 = process, $2 = want (yes|no): 0 right, 1 wrong, 2 not running
    local eff; eff="$(pi "grep -m1 '^CapEff:' /proc/\$(pidof '$1')/status 2>/dev/null | cut -f2")"
    if [ -z "$eff" ]; then return 2; fi
    local bit=$(( (16#$eff >> 17) & 1 ))
    [ "$bit" = "$([ "$2" = yes ] && echo 1 || echo 0)" ]
  }
  wait_capeff() { # $1 = process, $2 = want: 0 right, 1 wrong, 2 never appeared within 20 s
    local i rc
    for i in $(seq 1 40); do
      if check_capeff "$1" "$2"; then
        return 0
      else
        rc=$?
      fi
      if [ "$rc" -eq 1 ]; then
        return 1
      fi
      sleep 0.5
    done
    return 2
  }
  report_capeff() { # $1 = process, $2 = want, $3 = the sentence for a wrong capability
    local rc=0
    wait_capeff "$1" "$2" || rc=$?
    case "$rc" in
      0) return 0 ;;
      2) echo "capability check: $1 is not running" ;;
      *) echo "$3" ;;
    esac
    return 1
  }
  report_capeff plippa-leds yes \
    "capability check: the running plippa-leds has no CAP_SYS_RAWIO (LED output is dark)" \
    || exit 1
  report_capeff plippa-ui no \
    "capability check: the running plippa-ui holds CAP_SYS_RAWIO; it must hold none" \
    || exit 1
  echo "capability verified: plippa-leds has CAP_SYS_RAWIO (bit 17), plippa-ui has none"
}

case "${1:-all}" in
  setup) cmd_setup ;;
  sysroot) cmd_sysroot ;;
  build) cmd_build ;;
  push)  cmd_push ;;
  install-root) cmd_install_root ;;
  eval)  shift; cmd_eval "$@" ;;
  all)   cmd_build; cmd_push ;;
  *) echo "usage: $0 [setup|sysroot|build|push|install-root|all|eval <name>]"; exit 2 ;;
esac
