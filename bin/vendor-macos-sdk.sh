#!/usr/bin/env bash

# Extracts the local macOS SDK and uploads it to the private bucket the
# CI-macOS Cloud Build step reads from (see the header comment in
# cloudbuild.yaml). Run this on a Mac with Xcode or the Command Line Tools
# installed, and re-run it whenever the SDK needs to be refreshed.

set -euo pipefail

SDK_PATH="$(xcrun --show-sdk-path)"
echo "Using SDK: ${SDK_PATH}"

ARCHIVE="$(mktemp -d "${TMPDIR:-/tmp}/macos-sdk.XXXXXX")/macos-sdk.tar.xz"
tar -cJf "${ARCHIVE}" -C "$(dirname "${SDK_PATH}")" "$(basename "${SDK_PATH}")"

SIZE=$(stat -f%z "${ARCHIVE}" 2>/dev/null || stat -c%s "${ARCHIVE}")
echo "Archive size: ${SIZE} bytes"

# A real macOS SDK is several hundred MB; anything under 1 MB means the tar
# step silently produced an empty or truncated archive, which is exactly what
# happened the first time this was done by hand.
if [ "${SIZE}" -lt 1000000 ]; then
  echo "ERROR: archive is suspiciously small (${SIZE} bytes); refusing to upload" >&2
  exit 1
fi

PROJECT_ID="$(gcloud config get-value project)"
DEST="gs://${PROJECT_ID}_cloudbuild/zaz-cache/macos-sdk.tar.xz"
echo "Uploading to ${DEST}"
gsutil cp "${ARCHIVE}" "${DEST}"

rm -rf "$(dirname "${ARCHIVE}")"
echo "Done."
