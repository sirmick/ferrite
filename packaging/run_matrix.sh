#!/usr/bin/env bash
# Build ferrite packages across (distro, arch). Web bundle is built
# once on the host (arch-independent) and bundled into the source
# tarball — sidesteps the lightningcss / @tailwindcss/oxide native-
# binding gap on non-x86 archs. The riscv64/arm64 target builds rely
# on docker buildx + qemu-user-static + binfmt_misc on the host.
#
# First cut: ubuntu:24.04 amd64 only. Add more rows to TARGETS once
# the single-arch path is green.
set -uo pipefail   # not -e: we want the loop to continue past row failures

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

VERSION="$(awk -F'"' '/^version *=/{print $2; exit}' server/Cargo.toml)"
BUILD_CTX="$ROOT/packaging/build-ctx"
DIST="$ROOT/dist"
PKG_DIR="$DIST/packages"
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
  "ubuntu-24.04-amd64    linux/amd64    ubuntu:24.04   deb"
  "ubuntu-24.04-arm64    linux/arm64    ubuntu:24.04   deb"
  "ubuntu-24.04-riscv64  linux/riscv64  ubuntu:24.04   deb"
  "debian-12-amd64       linux/amd64    debian:12      deb"
  "debian-12-arm64       linux/arm64    debian:12      deb"
  # debian:12 (bookworm) has no official riscv64 manifest; use trixie:
  "debian-trixie-riscv64 linux/riscv64  debian:trixie  deb"
  "fedora-40-amd64       linux/amd64    fedora:40      rpm"
  "fedora-40-arm64       linux/arm64    fedora:40      rpm"
)

mkdir -p "$BUILD_CTX" "$DIST" "$PKG_DIR"
rm -f "$BUILD_CTX"/*.tar.xz "$BUILD_CTX"/*.spec

set -e   # fatal up to the matrix loop — host-side staging must succeed.
echo ">>> 1. Build web bundle on host (arch-independent, copied into tarball)"
pnpm --filter @ferrite/web build

echo ">>> 2. Stage source tarball ($BUILD_CTX/$TARBALL_NAME)"
TMPDIR=$(mktemp -d)
# Combined EXIT trap: clean tmpdir + sweep orphaned build containers from
# any failed row.
trap 'rm -rf "$TMPDIR"; docker container prune -f >/dev/null 2>&1 || true' EXIT
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

set +e   # past staging — each row may fail independently.

echo ">>> 3. Build matrix"
results=()
for row in "${TARGETS[@]}"; do
    read -r tag platform base kind <<<"$row"
    image="ferrite-pkg:$tag"
    log="$DIST/${tag}.log"
    pkg_out="$PKG_DIR/$tag"

    echo
    echo "============================================================"
    echo "[matrix] $tag  (BASE=$base, platform=$platform, kind=$kind)"
    echo "[matrix] log: $log"
    echo "============================================================"

    if ! docker build \
            --platform "$platform" \
            -f "packaging/Dockerfile.$kind" \
            --build-arg BASE="$base" \
            -t "$image" \
            "$BUILD_CTX" 2>&1 | tee "$log"; then
        results+=("$tag: BUILD_FAIL  (see $log)")
        continue
    fi

    # Extract built .deb / .rpm out of the test image to dist/packages/<tag>/
    rm -rf "$pkg_out"; mkdir -p "$pkg_out"
    cid="$(docker create "$image")"
    docker cp "$cid:/pkg/." "$pkg_out/" >/dev/null 2>&1 || true
    docker rm "$cid" >/dev/null 2>&1 || true
    artifact="$(find "$pkg_out" -maxdepth 1 \( -name '*.deb' -o -name '*.rpm' \) | sort | head -1)"
    if [[ -n "$artifact" ]]; then
        size="$(du -h "$artifact" | cut -f1)"
        results+=("$tag: OK   $size $(basename "$artifact")")
    else
        results+=("$tag: BUILD_OK but NO ARTIFACT (see $log)")
    fi
done

echo
echo "============================================================"
echo "[matrix] SUMMARY"
echo "============================================================"
for r in "${results[@]}"; do echo "  $r"; done
echo
echo "Built images:"
docker images "ferrite-pkg" 2>/dev/null | head -20
echo
echo "Packages: $PKG_DIR/<tag>/"
echo "Logs:     $DIST/<tag>.log"
