#!/usr/bin/env bash
# Build ferrite packages across (distro, arch). Web bundle is built
# once on the host (arch-independent) and bundled into the source
# tarball — sidesteps the lightningcss / @tailwindcss/oxide native-
# binding gap on non-x86 archs. The riscv64/arm64 target builds rely
# on docker buildx + qemu-user-static + binfmt_misc on the host.
#
# First cut: ubuntu:24.04 amd64 only. Add more rows to TARGETS once
# the single-arch path is green.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

VERSION="$(awk -F'"' '/^version *=/{print $2; exit}' server/Cargo.toml)"
BUILD_CTX="$ROOT/packaging/build-ctx"
TARBALL_NAME="ferrite_${VERSION}.tar.xz"

# (image-tag, platform, BASE, kind)  kind ∈ {deb, rpm} → packaging/Dockerfile.<kind>
#
# Multi-arch rows (linux/arm64, linux/riscv64) need qemu-user-static +
# binfmt_misc on the host. One-time setup:
#   sudo apt install qemu-user-static binfmt-support docker-buildx
#   docker run --privileged --rm tonistiigi/binfmt --install all
# riscv64 is omitted from debian:12 (bookworm) and fedora:40 because
# their official images don't ship a riscv64 manifest; debian:trixie
# does.
TARGETS=(
  "ubuntu-24.04-amd64   linux/amd64   ubuntu:24.04   deb"
  "debian-12-amd64      linux/amd64   debian:12      deb"
  "fedora-40-amd64      linux/amd64   fedora:40      rpm"
  # riscv64 + arm64 rows — re-enable once docker buildx + qemu-user-static
  # are installed on the host (see header comment).
  # "debian-trixie-riscv64 linux/riscv64 debian:trixie deb"
)

mkdir -p "$BUILD_CTX"
rm -f "$BUILD_CTX"/*.tar.xz "$BUILD_CTX"/*.spec

echo ">>> 1. Build web bundle on host (arch-independent, copied into tarball)"
pnpm --filter @ferrite/web build

echo ">>> 2. Stage source tarball ($BUILD_CTX/$TARBALL_NAME)"
TMPDIR=$(mktemp -d)
trap 'rm -rf "$TMPDIR"' EXIT
SRCDIR="$TMPDIR/ferrite-${VERSION}"
mkdir -p "$SRCDIR"
# rsync the worktree minus build artifacts and minus packaging/build-ctx
# (which holds *this* tarball — exclude or we recurse). web/build/ is
# explicitly *kept* so containers don't have to rebuild it.
rsync -a \
    --exclude='target/' \
    --exclude='node_modules/' \
    --exclude='soapysdr/' \
    --exclude='soapysdr-src/' \
    --exclude='packaging/build-ctx/' \
    --exclude='.git/' \
    --exclude='dev-server.log' \
    "$ROOT/" "$SRCDIR/"
tar -cJf "$BUILD_CTX/$TARBALL_NAME" -C "$TMPDIR" "ferrite-${VERSION}"

# rpmbuild reads the spec from a separate file (not inside the tarball);
# stage it next to the tarball in the build context.
cp "$ROOT/rpm/ferrite.spec" "$BUILD_CTX/ferrite.spec"

echo ">>> 3. Build matrix"
for row in "${TARGETS[@]}"; do
    read -r tag platform base kind <<<"$row"
    image="ferrite-pkg:$tag"
    echo ">>> Building $image  (BASE=$base, platform=$platform, kind=$kind)"
    docker build \
        --platform "$platform" \
        -f "packaging/Dockerfile.$kind" \
        --build-arg BASE="$base" \
        -t "$image" \
        "$BUILD_CTX"
done

echo ">>> Done. Built images:"
docker images "ferrite-pkg" 2>/dev/null | head -20
