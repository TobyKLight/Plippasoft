#!/usr/bin/env bash
# MVP_09 multi-process IPC smoke (no hardware).
# Run from Plippasoft after building the three bins:
#
#   export CARGO_BUILD_JOBS=1
#   cargo build -p plippa-engine
#   cargo build -p plippa-ui
#   cargo build -p plippa-server
#   ./scripts/mvp09_ipc_smoke.sh
#
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="${CARGO_TARGET_DIR:-$ROOT/target}/debug"
IPC_DIR="$(mktemp -d /tmp/plippa-ipc-smoke-XXXXXX)"
LIB_DIR="$(mktemp -d /tmp/plippa-lib-smoke-XXXXXX)"
export PLIPPA_IPC_DIR="$IPC_DIR"
export PLIPPA_LIBRARY_ROOT="$LIB_DIR"
export PLIPPA_IPC_SMOKE_SECS=12
export PLIPPA_SMOKE_TAP_KEY="Tone.noubin.SMOKE"

FIX="$ROOT/crates/plippa-audio/tests/fixtures/tone_48k_16.wav"
cp "$FIX" "$LIB_DIR/tone.wav"
cat >"$LIB_DIR/_tone.noudata" <<'EOF'
{
  "noubinKey": "Tone.noubin.SMOKE",
  "noubinKeyNormalised": "tone.noubin.smoke",
  "playableItemTitle": "Tone",
  "mediaList": [{"localFilePaths": ["tone.wav"]}]
}
EOF

ENGINE_PID=""
UI_PID=""
SERVER_PID=""
cleanup() {
  [[ -n "$ENGINE_PID" ]] && kill "$ENGINE_PID" 2>/dev/null || true
  [[ -n "$UI_PID" ]] && kill "$UI_PID" 2>/dev/null || true
  [[ -n "$SERVER_PID" ]] && kill "$SERVER_PID" 2>/dev/null || true
  wait 2>/dev/null || true
  rm -rf "$IPC_DIR" "$LIB_DIR"
}
trap cleanup EXIT

# Pre-index library (engine requires an existing WAL sqlite).
PLIPPA_IPC_CONNECT_SECS=0 "$BIN/plippa-server" --bind 127.0.0.1:18786 --library-root "$LIB_DIR" &
PRE_PID=$!
sleep 1.5
kill "$PRE_PID" 2>/dev/null || true
wait "$PRE_PID" 2>/dev/null || true

"$BIN/plippa-engine" --sink=mock --library-root "$LIB_DIR" &
ENGINE_PID=$!
sleep 0.8
PLIPPA_ONCE=1 "$BIN/plippa-ui" &
UI_PID=$!
"$BIN/plippa-server" --bind 127.0.0.1:18787 --library-root "$LIB_DIR" &
SERVER_PID=$!
sleep 0.8

[[ -S "$IPC_DIR/engine.sock" ]] || { echo "engine.sock missing"; exit 1; }
"$BIN/plippa-ipc-smoke-client"
echo "mvp09_ipc_smoke: OK"
