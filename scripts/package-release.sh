#!/usr/bin/env bash
# Package Sashiko release binaries and source archives into release tarballs.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"

TAG="${1:-$(git -C "${ROOT_DIR}" describe --tags --always)}"
TARGET="${2:-$(rustc -vV | sed -n 's/^host: //p')}"
DIST_DIR="${ROOT_DIR}/dist"

echo "==> Packaging Sashiko release for ${TAG} (${TARGET})..."

# Build release binaries
cargo build --manifest-path="${ROOT_DIR}/Cargo.toml" --release --locked --target "${TARGET}" --bin sashiko --bin sashiko-cli

BIN_DIR="${ROOT_DIR}/target/${TARGET}/release"
ARCHIVE_NAME="sashiko-${TAG}-${TARGET}"
STAGING_DIR="${DIST_DIR}/${ARCHIVE_NAME}"

rm -rf "${STAGING_DIR}"
mkdir -p "${STAGING_DIR}" "${DIST_DIR}"

# Copy binaries
cp "${BIN_DIR}/sashiko" "${STAGING_DIR}/"
cp "${BIN_DIR}/sashiko-cli" "${STAGING_DIR}/"

# Strip symbols to minimize binary size
strip "${STAGING_DIR}/sashiko" 2>/dev/null || true
strip "${STAGING_DIR}/sashiko-cli" 2>/dev/null || true

# Copy distribution metadata
cp "${ROOT_DIR}/README.md" "${STAGING_DIR}/"
cp "${ROOT_DIR}/LICENSE" "${STAGING_DIR}/"
cp "${ROOT_DIR}/Settings.toml" "${STAGING_DIR}/"

# Create binary archive
TARBALL="${DIST_DIR}/${ARCHIVE_NAME}.tar.gz"
tar -czf "${TARBALL}" -C "${DIST_DIR}" "${ARCHIVE_NAME}"
(cd "${DIST_DIR}" && sha256sum "${ARCHIVE_NAME}.tar.gz" > "${ARCHIVE_NAME}.tar.gz.sha256")
rm -rf "${STAGING_DIR}"

# Create source archive from git
SRC_ARCHIVE="sashiko-${TAG}-src.tar.gz"
git -C "${ROOT_DIR}" archive --format=tar.gz --prefix="sashiko-${TAG}/" -o "${DIST_DIR}/${SRC_ARCHIVE}" HEAD
(cd "${DIST_DIR}" && sha256sum "${SRC_ARCHIVE}" > "${SRC_ARCHIVE}.sha256")

echo "==> Release packaging complete:"
ls -lh "${DIST_DIR}"/sashiko-${TAG}*
