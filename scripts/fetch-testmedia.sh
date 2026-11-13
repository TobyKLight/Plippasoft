#!/usr/bin/env bash
# Download the test media (kept out of git) from the GitHub release and unpack it into
# assets/testMedia. Safe to re-run: it does nothing when the pinned version is already there.
#
#   scripts/fetch-testmedia.sh            fetch if missing or out of date
#   scripts/fetch-testmedia.sh --force    fetch again regardless
#
# Needs: bash, curl, tar, and sha256sum (or shasum). The download is checked against the
# SHA-256 below before anything is unpacked. To publish a new version: build the archive, upload
# it as a release asset, then update VERSION, ARCHIVE and SHA256 here.
set -euo pipefail

VERSION="testmedia-v1"
ARCHIVE="testMedia-v1.tar.gz"
SHA256="8389498ab00f2bba7118a1a3b853cc56421df3ee42f3f25bdb51c68ef10d8025"
URL="https://github.com/TobyKLight/Plippasoft/releases/download/$VERSION/$ARCHIVE"

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DEST="$ROOT/assets/testMedia"
MARK="$DEST/.testmedia-version"

if [ "${1:-}" != "--force" ] && [ -f "$MARK" ] && [ "$(cat "$MARK")" = "$VERSION" ]; then
  echo "test media $VERSION already present in $DEST"
  exit 0
fi

sha256() { if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1"; else shasum -a 256 "$1"; fi | cut -d' ' -f1; }

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

echo "downloading $URL"
curl -fL --retry 3 -o "$tmp/$ARCHIVE" "$URL"

got="$(sha256 "$tmp/$ARCHIVE")"
if [ "$got" != "$SHA256" ]; then
  echo "checksum mismatch for $ARCHIVE" >&2
  echo "  expected $SHA256" >&2
  echo "  got      $got" >&2
  exit 1
fi

mkdir -p "$ROOT/assets"
rm -rf "$DEST"
tar -xzf "$tmp/$ARCHIVE" -C "$ROOT/assets"
printf '%s\n' "$VERSION" > "$MARK"
echo "test media $VERSION unpacked to $DEST"
