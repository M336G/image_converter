#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SRC="$ROOT/libs/ImageMagick"
BUILD="$SRC/build"
PREFIX="$ROOT/build/install"

if [ ! -e "$SRC/configure" ]; then
    git -C "$ROOT" submodule update --init --depth 1 libs/ImageMagick
fi

if [ -f "$PREFIX/lib/pkgconfig/MagickWand.pc" ]; then
    echo "ImageMagick is already built in $PREFIX"
    exit 0
fi

JOBS="$(nproc 2>/dev/null || sysctl -n hw.ncpu)"

mkdir -p "$BUILD"
cd "$BUILD"

../configure \
    --prefix="$PREFIX" \
    --enable-shared --disable-static \
    --enable-zero-configuration \
    --disable-docs --without-x \
    --with-magick-plus-plus=no --with-perl=no
make -j"$JOBS"
make install

echo "ImageMagick installed in $PREFIX"