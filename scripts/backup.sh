#!/usr/bin/env bash
# Backup the Plippa media library from the device to this host, atomically.
#
#   scripts/backup.sh --dest DIR      copy to DIR (required; or set PLIPPA_BACKUP_DEST) and swap it in
#   scripts/backup.sh --source PATH    remote library root (default: the single mount under
#                                      /media/<user> on the device, or $PLIPPA_LIBRARY_ROOT)
#   scripts/backup.sh --list-sources   print the device's library roots and exit
#   scripts/backup.sh --quiesce        stop plippa.service for a consistent snapshot, restart after
#   scripts/backup.sh --exclude-plippa skip .plippa/ (index DB + waveform sidecars)
#   scripts/backup.sh --exclude GLOB   additional exclusion (repeatable)
#   scripts/backup.sh --verify MODE    sha256 | size (default) | none
#   scripts/backup.sh --transport T    rsync | tar (default: rsync when installed, else tar)
#   scripts/backup.sh --keep-staging   keep the partial copy if this run fails
#   scripts/backup.sh --force          break a stale lock left by an interrupted run
#   scripts/backup.sh --dry-run        print the plan; no writes, no remote change
#
# The destination is never left half-written:
#   1. the copy goes to <dest>.incoming.<pid> in the same folder (same filesystem)
#   2. file count and byte count are compared against the device
#   3. <dest> is renamed to <dest>.old.<pid>, then the staging tree is renamed to <dest>
#   4. the previous backup is deleted
# A failed or interrupted run leaves the previous backup exactly as it was. Steps 3-4 are two
# renames plus a delete, so the window where <dest> is missing is microseconds.
#
# Authentication is NOT set up here. This script only *uses* what scripts/deploy.sh already
# established: ~/.config/plippa-deploy/deploy.env for PI_HOST/PI_USER, the askpass helper for the
# optional stored password, and whatever ssh auth that machine has (key or password). See the
# README "Backup" section. Nothing secret is ever passed on a command line.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CONF_DIR="${PLIPPA_DEPLOY_DIR:-$HOME/.config/plippa-deploy}"
PASS_FILE="$CONF_DIR/password"
ASKPASS="$CONF_DIR/askpass.sh"
# shellcheck disable=SC1090
[ -f "$CONF_DIR/deploy.env" ] && . "$CONF_DIR/deploy.env"

PI_HOST="${PI_HOST:-192.168.0.235}"
PI_USER="${PI_USER:-plip}"
SERVICE="${PLIPPA_SERVICE:-plippa.service}"

die() { printf 'backup: %s\n' "$*" >&2; exit 1; }
say() { printf '%s\n' "$*"; }
warn() { printf 'backup: warning: %s\n' "$*" >&2; }

is_windows() { case "$(uname -s)" in MINGW* | MSYS* | CYGWIN*) return 0 ;; esac; return 1; }

# C:\x\y -> /c/x/y (Git Bash / MSYS). Anything already POSIX passes through.
to_posix() {
  case "$1" in
    [A-Za-z]:[\\/]*)
      printf '/%s%s\n' \
        "$(printf '%s' "${1:0:1}" | tr '[:upper:]' '[:lower:]')" \
        "$(printf '%s' "${1:2}" | sed 's#\\#/#g')"
      ;;
    *) printf '%s\n' "$1" ;;
  esac
}

# single-quote for a remote shell
sq() { printf "'%s'" "$(printf '%s' "$1" | sed "s/'/'\\\\''/g")"; }

hr() { awk -v b="${1:-0}" 'BEGIN{split("B KiB MiB GiB TiB",u," ");i=1;while(b>=1024&&i<5){b/=1024;i++}printf "%.1f %s",b,u[i]}'; }

SSH_OPTS=(-o StrictHostKeyChecking=accept-new -o ConnectTimeout=10)
# Connection sharing keeps a large multi-file copy to one TCP session. Not on Windows: the
# OpenSSH there does not do multiplexing.
is_windows || SSH_OPTS+=(-o ControlMaster=auto -o ControlPersist=60 -o "ControlPath=$CONF_DIR/cm-%C")

# Same mechanism as scripts/deploy.sh: the password (when one is stored) is handed to ssh through
# an askpass helper, never on the command line or in the environment.
ssh_env() {
  if [ -r "$PASS_FILE" ] && [ -x "$ASKPASS" ]; then
    export SSH_ASKPASS="$ASKPASS" SSH_ASKPASS_REQUIRE=force DISPLAY="${DISPLAY:-none}"
  fi
}

pi() { ssh_env; ssh "${SSH_OPTS[@]}" "$PI_USER@$PI_HOST" "$@"; }
pi_sudo() {
  if [ -r "$PASS_FILE" ]; then
    ssh_env
    ssh "${SSH_OPTS[@]}" "$PI_USER@$PI_HOST" "sudo -S -p '' $*" <"$PASS_FILE"
  else
    ssh_env
    ssh "${SSH_OPTS[@]}" -t "$PI_USER@$PI_HOST" "sudo $*"
  fi
}

# --- defaults -------------------------------------------------------------------------------
DEST="${PLIPPA_BACKUP_DEST:-}"
SRC="${PLIPPA_LIBRARY_ROOT:-}"
QUIESCE=0
VERIFY=size
TRANSPORT=""
KEEP_STAGING=0
FORCE=0
DRY=0
SHOW_SOURCES=0

# Junk a FAT/exFAT volume picks up from Windows and macOS hosts. Excluded from the copy *and*
# from the byte-count check, so both sides always compare like for like.
EXCLUDES=(
  'System Volume Information'
  '$RECYCLE.BIN'
  'lost+found'
  '.Trash-*'
  '.Trashes'
  '.Spotlight-V100'
  '.fseventsd'
  '.TemporaryItems'
  '.DS_Store'
  'Thumbs.db'
)

usage() {
  cat <<'EOF'
usage: scripts/backup.sh [options]

  --dest DIR          destination folder (required; or set PLIPPA_BACKUP_DEST)
  --source PATH       remote library root (default: auto-discover under /media/<user>)
  --list-sources      list library roots on the device and exit
  --quiesce           stop plippa.service for a consistent snapshot, restart after
  --exclude-plippa    skip .plippa/ (index DB + waveform sidecars)
  --exclude GLOB      additional exclusion (repeatable)
  --verify MODE       sha256 | size (default) | none
  --transport T       rsync | tar (default: rsync when installed, else tar)
  --keep-staging      keep the partial copy if this run fails
  --force             break a stale lock from an interrupted run
  --dry-run           print the plan; no writes, no remote change
  -h, --help          this text
EOF
}

while [ $# -gt 0 ]; do
  case "$1" in
    --list-sources) SHOW_SOURCES=1 ;;
    --dest)
      [ $# -ge 2 ] || die "--dest needs a value"
      DEST="$2"
      shift
      ;;
    --dest=*) DEST="${1#*=}" ;;
    --source)
      [ $# -ge 2 ] || die "--source needs a value"
      SRC="$2"
      shift
      ;;
    --source=*) SRC="${1#*=}" ;;
    --exclude)
      [ $# -ge 2 ] || die "--exclude needs a value"
      EXCLUDES+=("$2")
      shift
      ;;
    --exclude=*) EXCLUDES+=("${1#*=}") ;;
    --exclude-plippa) EXCLUDES+=('.plippa') ;;
    --verify)
      [ $# -ge 2 ] || die "--verify needs a value"
      VERIFY="$2"
      shift
      ;;
    --verify=*) VERIFY="${1#*=}" ;;
    --transport)
      [ $# -ge 2 ] || die "--transport needs a value"
      TRANSPORT="$2"
      shift
      ;;
    --transport=*) TRANSPORT="${1#*=}" ;;
    --quiesce) QUIESCE=1 ;;
    --keep-staging) KEEP_STAGING=1 ;;
    --force) FORCE=1 ;;
    --dry-run) DRY=1 ;;
    -h | --help) usage; exit 0 ;;
    *) die "unknown argument: $1 (try --help)" ;;
  esac
  shift
done

case "$VERIFY" in sha256 | size | none) ;; *) die "--verify must be sha256, size or none" ;; esac
[ -n "$DEST" ] || [ "$SHOW_SOURCES" -eq 1 ] || die "no destination: pass --dest DIR (or set PLIPPA_BACKUP_DEST)"
DEST="$(to_posix "${DEST%/}")"

# --- remote helpers -------------------------------------------------------------------------
find_excludes() {
  local e
  for e in "${EXCLUDES[@]}"; do
    printf ' -not -path %s -not -path %s' "$(sq "./$e")" "$(sq "./$e/*")"
  done
}

list_roots() {
  # Mounted filesystems only: lsblk prints nothing for an unmounted device, so leftover
  # udisks placeholder directories are not mistaken for a library.
  pi "lsblk -rno MOUNTPOINT 2>/dev/null | grep '^/media/' | sed 's/[[:space:]]*\$//' | sort -u"
}

preflight() {
  pi true || die "cannot log in to $PI_USER@$PI_HOST — see README 'Backup the media library' → Authentication"
}

resolve_source() {
  local roots n
  roots="$(list_roots)"
  [ -n "$roots" ] || die "no mounted library under /media on $PI_HOST (is the stick plugged in?)"
  n="$(printf '%s\n' "$roots" | grep -c . || true)"
  if [ "$n" -gt 1 ]; then
    printf 'backup: several libraries are mounted on %s:\n' "$PI_HOST" >&2
    printf '%s\n' "$roots" | sed 's/^/  /' >&2
    die "pick one with --source PATH"
  fi
  printf '%s\n' "$roots" | head -1
}

remote_size_stats() {
  pi "cd $(sq "$SRC") && find . -type f $(find_excludes) -printf '%s\n' | awk '{n++; s+=\$1} END {printf \"%d %d\", n+0, s+0}'"
}

remote_sha_manifest() {
  # Normalised to "<hash>  <path>": coreutils on Windows writes "<hash> *<path>" in binary mode,
  # so without this every line would look different from the device's.
  pi "cd $(sq "$SRC") && find . -type f $(find_excludes) -print0 | LC_ALL=C sort -z | xargs -0 -r sha256sum" |
    sed 's/^\([0-9a-f]\{64\}\) [ *]/\1  /'
}

# --- local helpers --------------------------------------------------------------------------
# GNU vs BSD userland: the host side may be Git Bash (GNU), Linux (GNU) or macOS (BSD).
if stat -c '%s' "$0" >/dev/null 2>&1; then STAT_FMT=(-c '%s'); else STAT_FMT=(-f '%z'); fi
if command -v sha256sum >/dev/null 2>&1; then SHA_CMD=(sha256sum); else SHA_CMD=(shasum -a 256); fi
XARGS_OPT=(-0)
xargs --no-run-if-empty true </dev/null >/dev/null 2>&1 && XARGS_OPT=(-0 -r)

local_size_stats() {
  local n
  n="$(cd "$STAGE" && find . -type f | wc -l | tr -d ' ')"
  if [ "$n" -eq 0 ]; then
    printf '0 0'
    return 0
  fi
  (cd "$STAGE" && find . -type f -print0 |
    xargs "${XARGS_OPT[@]}" stat "${STAT_FMT[@]}" |
    awk '{n++; s+=$1} END {printf "%d %d", n+0, s+0}')
}

local_sha_manifest() {
  (cd "$STAGE" && find . -type f -print0 | LC_ALL=C sort -z | xargs "${XARGS_OPT[@]}" "${SHA_CMD[@]}") |
    sed 's/^\([0-9a-f]\{64\}\) [ *]/\1  /'
}

# --- copy -----------------------------------------------------------------------------------
copy_rsync() {
  local f=() e
  for e in "${EXCLUDES[@]}"; do f+=(--exclude="$e"); done
  # -rlt: recurse, keep symlinks and mtimes; no owner/group/perms because the destination is NTFS
  # or a different uid. -s stops the remote shell re-splitting paths that contain spaces.
  ssh_env
  rsync -rlt --no-perms --no-owner --no-group -s "${f[@]}" \
    -e "ssh ${SSH_OPTS[*]}" \
    "$PI_USER@$PI_HOST:$SRC/" "$STAGE/"
}

copy_tar() {
  local e ex=""
  for e in "${EXCLUDES[@]}"; do ex+=" --exclude=$(sq "$e")"; done
  # Streamed over one ssh connection: nothing is staged on the device.
  pi "tar -C $(sq "$SRC") -cf -$ex ." | tar -C "$STAGE" -xf -
}

pick_transport() {
  if [ -z "$TRANSPORT" ]; then
    if command -v rsync >/dev/null 2>&1; then TRANSPORT=rsync; else TRANSPORT=tar; fi
  fi
  case "$TRANSPORT" in
    rsync) command -v rsync >/dev/null 2>&1 || die "rsync is not installed (use --transport tar)" ;;
    tar) command -v tar >/dev/null 2>&1 || die "tar is not installed" ;;
    *) die "--transport must be rsync or tar" ;;
  esac
}

do_copy() {
  case "$TRANSPORT" in
    rsync) copy_rsync ;;
    tar) copy_tar ;;
  esac
}

# --- verify ---------------------------------------------------------------------------------
do_verify() {
  case "$VERIFY" in
    none)
      say "verification skipped (--verify none)"
      ;;
    size)
      local r l rn rs ln ls
      r="$(remote_size_stats)"
      l="$(local_size_stats)"
      rn="${r%% *}"
      rs="${r##* }"
      ln="${l%% *}"
      ls="${l##* }"
      if [ "$rn" != "$ln" ] || [ "$rs" != "$ls" ]; then
        die "mismatch: device has $rn files / $(hr "$rs"), copy has $ln files / $(hr "$ls") — the library changed while copying; re-run with --quiesce"
      fi
      VERIFIED_FILES="$rn"
      VERIFIED_BYTES="$rs"
      say "verified: $rn files, $(hr "$rs") — matches the device"
      ;;
    sha256)
      local rt lt n
      rt="$(mktemp)" || die "cannot create a temp file"
      lt="$(mktemp)" || die "cannot create a temp file"
      remote_sha_manifest >"$rt"
      local_sha_manifest >"$lt"
      n="$(grep -c . "$lt" || true)"
      if ! diff -q "$rt" "$lt" >/dev/null 2>&1; then
        warn "first differences (device vs copy):"
        diff -u "$rt" "$lt" 2>/dev/null | sed -n '1,12p' >&2 || true
        rm -f "$rt" "$lt"
        die "sha256 verification failed — the previous backup was kept"
      fi
      rm -f "$rt" "$lt"
      VERIFIED_FILES="$n"
      say "verified: $n files by sha256"
      ;;
  esac
}

# --- state + cleanup ------------------------------------------------------------------------
STAGE=""
LOCK=""
OLD_PATH=""
SWAP_STATE=none
QUIESCED=0
VERIFIED_FILES=0
VERIFIED_BYTES=0

restore_service() {
  [ "$QUIESCED" -eq 1 ] || return 0
  say "restarting $SERVICE on the device"
  pi_sudo "systemctl start $SERVICE" || warn "could not restart $SERVICE — start it manually"
  QUIESCED=0
}

cleanup() {
  local rc=$?
  trap - EXIT INT TERM
  if [ "$SWAP_STATE" = moved_old ] && [ -e "$OLD_PATH" ] && [ ! -e "$DEST" ]; then
    mv "$OLD_PATH" "$DEST" 2>/dev/null || warn "could not restore the previous backup from $OLD_PATH"
  fi
  restore_service || true
  if [ -n "$STAGE" ]; then
    if [ "$rc" -eq 0 ] || [ "$KEEP_STAGING" -eq 0 ]; then
      rm -rf "$STAGE" 2>/dev/null || true
    else
      warn "partial copy kept at $STAGE"
    fi
  fi
  [ -n "$LOCK" ] && rm -rf "$LOCK" 2>/dev/null
  exit "$rc"
}

commit_stage() {
  OLD_PATH="$DEST.old.$$"
  if [ -e "$DEST" ]; then
    [ -d "$DEST" ] || die "$DEST exists and is not a directory"
    mv "$DEST" "$OLD_PATH" || die "could not move the previous backup aside"
    SWAP_STATE=moved_old
  fi
  if ! mv "$STAGE" "$DEST"; then
    [ -e "$OLD_PATH" ] && mv "$OLD_PATH" "$DEST" 2>/dev/null
    SWAP_STATE=none
    die "could not move the new copy into place (previous backup restored)"
  fi
  SWAP_STATE=done
  STAGE=""
}

# --- main -----------------------------------------------------------------------------------
preflight

if [ "$SHOW_SOURCES" -eq 1 ]; then
  list_roots | sed 's/^/  /'
  exit 0
fi

[ -r "$PASS_FILE" ] && [ ! -x "$ASKPASS" ] &&
  warn "$PASS_FILE exists but $ASKPASS does not; ssh may prompt (scripts/deploy.sh setup creates it)"

[ -n "$SRC" ] || SRC="$(resolve_source)"
pick_transport

say "source   $PI_USER@$PI_HOST:$SRC"
say "dest     $DEST"
say "mode     $TRANSPORT, verify=$VERIFY"
[ "$QUIESCE" -eq 1 ] && say "quiesce  $SERVICE will be stopped for the copy"
[ "$VERIFY" = none ] && warn "verification disabled: a truncated copy would still be swapped in"

if [ "$DRY" -eq 1 ]; then
  say "--- dry run: nothing is written ---"
  if [ "$TRANSPORT" = rsync ]; then
    f=()
    for e in "${EXCLUDES[@]}"; do f+=(--exclude="$e"); done
    ssh_env
    rsync -rlt --no-perms --no-owner --no-group -n --stats -s "${f[@]}" \
      -e "ssh ${SSH_OPTS[*]}" "$PI_USER@$PI_HOST:$SRC/" "$DEST/" || true
  else
    say "would stream: ssh tar -C $SRC -cf - | tar -C <stage> -xf -"
  fi
  exit 0
fi

mkdir -p "$(dirname "$DEST")"
LOCK="$DEST.lock"
if ! mkdir "$LOCK" 2>/dev/null; then
  [ "$FORCE" -eq 1 ] || die "another run is in progress (or a stale lock at $LOCK) — use --force"
  rm -rf "$LOCK"
  mkdir "$LOCK" || die "cannot create the lock $LOCK"
fi
trap cleanup EXIT INT TERM

STAGE="$DEST.incoming.$$"
rm -rf "$STAGE"
mkdir -p "$STAGE"

if [ "$QUIESCE" -eq 1 ]; then
  say "stopping $SERVICE for a consistent snapshot"
  pi_sudo "systemctl stop $SERVICE"
  QUIESCED=1
  pi 'sync'
fi

say "copying (a full library over Wi-Fi can take a while)…"
START="$(date +%s)"
do_copy
say "copied in $(( $(date +%s) - START ))s"

do_verify

say "swapping in the new backup"
commit_stage
if [ -n "$OLD_PATH" ] && [ -e "$OLD_PATH" ]; then
  say "removing the previous backup"
  rm -rf "$OLD_PATH"
fi

STAGE=""
say "backup complete: $DEST ($VERIFIED_FILES files)"
