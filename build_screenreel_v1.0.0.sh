#!/bin/sh
# build_screenreel_v1.0.0.sh — build helper for ScreenReel v1.0.0
#
#   ./build_screenreel_v1.0.0.sh native        # portable glibc build: Rust/FLTK/JPEG/ALSA static,
#                                              #   only system X11 + fontconfig/freetype + libc dynamic
#   ./build_screenreel_v1.0.0.sh linux-static  # fully static musl ELF (runs inside alpine via podman/docker,
#                                              #   or directly when already on Alpine)
#   ./build_screenreel_v1.0.0.sh windows       # static .exe via mingw-w64 cross (x86_64-pc-windows-gnu)
#   ./build_screenreel_v1.0.0.sh selftest      # headless capture/edit/export test under Xvfb
#
# Output goes to ./dist/. Every step is idempotent; temporary changes (hidden .so
# symlinks inside the Alpine container) never touch the host.
set -eu

VERSION="1.0.0"
TAG="screenreel_v${VERSION}"
ALSA_VER="1.2.12"
HERE="$(cd "$(dirname "$0")" && pwd)"
DIST="$HERE/dist"
mkdir -p "$DIST"

log() { printf '\033[1;36m[%s]\033[0m %s\n' "$TAG" "$*"; }
die() { printf '\033[1;31m[%s] ERROR:\033[0m %s\n' "$TAG" "$*" >&2; exit 1; }
need() { command -v "$1" >/dev/null 2>&1 || die "missing tool: $1"; }

# --- static libasound (alsa-lib) from source into a private prefix -----------------
build_alsa_static() {
    prefix="$1"
    [ -f "$prefix/lib/libasound.a" ] && return 0
    log "building static alsa-lib $ALSA_VER -> $prefix"
    tmp="$(mktemp -d)"
    git clone -q --depth 1 --branch "v$ALSA_VER" https://github.com/alsa-project/alsa-lib.git "$tmp/alsa-lib"
    ( cd "$tmp/alsa-lib" && autoreconf -fi >/dev/null 2>&1 \
      && ./configure --prefix="$prefix" --enable-static --disable-shared --disable-python \
           --disable-topology --disable-ucm --without-debug --with-configdir=/usr/share/alsa >/dev/null \
      && make -j"$(nproc)" >/dev/null && make install >/dev/null )
    rm -rf "$tmp"
}

cmd_native() {
    need cargo; need cmake; need g++; need pkg-config
    build_alsa_static "$HERE/.deps/alsa"
    log "building portable glibc release"
    PKG_CONFIG_PATH="$HERE/.deps/alsa/lib/pkgconfig" PKG_CONFIG_ALL_STATIC=1 \
        cargo build --release --manifest-path "$HERE/Cargo.toml"
    cp "$HERE/target/release/screenreel" "$DIST/${TAG}-linux-x86_64-glibc"
    log "done: $DIST/${TAG}-linux-x86_64-glibc"
    ldd "$DIST/${TAG}-linux-x86_64-glibc" | sed 's/^/    /'
}

# --- fully static musl build (must run on Alpine) ------------------------------------
# X libs: prefer Alpine's *-static packages; any that are missing are built from the
# official X.Org tarballs with --enable-static.
XLIBS="libXau-1.0.12 libXdmcp-1.1.5 libXext-1.3.6 libXrender-0.9.12 libXfixes-6.0.1 libXinerama-1.1.5 libXcursor-1.2.3 libXft-2.3.9"

alpine_inner() {
    apk add --no-cache build-base cmake ninja git perl linux-headers pkgconf curl xz \
        autoconf automake libtool util-macros xorgproto \
        libx11-dev libxext-dev libxinerama-dev libxcursor-dev libxrender-dev libxfixes-dev libxft-dev \
        fontconfig-dev freetype-dev libxcb-dev libxau-dev libxdmcp-dev \
        libx11-static libxcb-static fontconfig-static freetype-static expat-static zlib-static \
        bzip2-static libpng-static brotli-static libstdc++-dev file rustup >/dev/null
    for p in libxext-static libxau-static libxdmcp-static libxrender-static libxfixes-static \
             libxinerama-static libxcursor-static libxft-static; do
        apk add --no-cache "$p" >/dev/null 2>&1 || log "no package $p (will build from source if needed)"
    done
    for x in $XLIBS; do
        name="${x%-*}"
        [ -f "/usr/lib/${name}.a" ] && continue
        log "building $x statically"
        t="$(mktemp -d)"
        curl -fsSL "https://www.x.org/releases/individual/lib/${x}.tar.xz" | tar -xJ -C "$t"
        ( cd "$t/$x" && ./configure --prefix=/usr --enable-static --disable-shared >/dev/null \
          && make -j"$(nproc)" >/dev/null && make install >/dev/null )
        rm -rf "$t"
    done
    build_alsa_static /opt/alsa-static
    # Hide shared-object dev symlinks so every -lX resolves to the .a archive.
    # (fltk-sys emits dylib=X11 etc.; with only .a present the result is fully static.)
    for l in X11 Xext Xinerama Xcursor Xrender Xfixes Xft fontconfig xcb Xau Xdmcp freetype png16 png z bz2 \
             brotlidec brotlicommon expat stdc++ asound; do rm -f "/usr/lib/lib$l.so" "/lib/lib$l.so"; done
    command -v cargo >/dev/null 2>&1 || { rustup-init -y --profile minimal >/dev/null; . "$HOME/.cargo/env"; }
    # fltk-sys hard-codes musl-gcc/musl-g++ for *-linux-musl targets; on Alpine the
    # native toolchain already targets musl, so alias those names to it.
    for c in gcc g++; do
        command -v "musl-$c" >/dev/null 2>&1 || ln -sf "$(command -v $c)" "/usr/local/bin/musl-$c"
    done
    export CC_x86_64_unknown_linux_musl=gcc CXX_x86_64_unknown_linux_musl=g++
    # Non-PIE fully static: with -static every -l resolves to a .a, never libc.so.
    RF="-C target-feature=+crt-static -C relocation-model=static -C link-arg=-static"
    # Static archives are order-sensitive: resolve the whole X/font/runtime stack as one
    # linker group (Xft->Xrender->Xext->X11->xcb..., libgcc for __popcountdi2, libc last).
    RF="$RF -C link-arg=-Wl,--start-group"
    for l in Xft Xrender Xcursor Xfixes Xinerama Xext X11 xcb Xau Xdmcp fontconfig freetype png16 z bz2 \
             brotlidec brotlicommon expat stdc++ gcc c; do RF="$RF -C link-arg=-l$l"; done
    RF="$RF -C link-arg=-Wl,--end-group"
    export RUSTFLAGS="$RF" PKG_CONFIG_PATH=/opt/alsa-static/lib/pkgconfig PKG_CONFIG_ALL_STATIC=1
    cargo build --release --target x86_64-unknown-linux-musl --manifest-path /src/Cargo.toml --target-dir /src/target-musl
    out="/src/dist/${TAG}-linux-x86_64-static"
    cp /src/target-musl/x86_64-unknown-linux-musl/release/screenreel "$out"
    file "$out" 2>/dev/null || true
    readelf -lW "$out" | grep -i interp || true
    readelf -dW "$out" | grep NEEDED || true
    if readelf -lW "$out" | grep -qi 'INTERP' || readelf -dW "$out" | grep -q NEEDED; then
        die "binary is not fully static"
    fi
    log "fully static: $out"
}

cmd_linux_static() {
    if [ -f /etc/alpine-release ]; then
        mkdir -p /src && [ "$HERE" = /src ] || die "on Alpine, run from a checkout mounted at /src"
        alpine_inner; return
    fi
    eng=""
    for e in podman docker; do command -v "$e" >/dev/null 2>&1 && { eng="$e"; break; }; done
    [ -n "$eng" ] || die "need podman or docker (or run this script on Alpine with the repo at /src)"
    log "building inside alpine:3.20 via $eng"
    "$eng" run --rm -v "$HERE:/src:Z" -w /src alpine:3.20 sh -c "./$(basename "$0") linux-static"
}

cmd_windows() {
    need cargo; need cmake; need x86_64-w64-mingw32-g++
    rustup target add x86_64-pc-windows-gnu >/dev/null
    log "cross-building static Windows exe (mingw-w64)"
    # Static CRT + static libgcc/libstdc++/winpthread => single self-contained .exe.
    RUSTFLAGS="-C target-feature=+crt-static -C link-args=-static" \
        cargo build --release --target x86_64-pc-windows-gnu --manifest-path "$HERE/Cargo.toml"
    cp "$HERE/target/x86_64-pc-windows-gnu/release/screenreel.exe" "$DIST/${TAG}-windows-x86_64.exe"
    log "done: $DIST/${TAG}-windows-x86_64.exe"
}

cmd_selftest() {
    need Xvfb
    bin="${1:-$HERE/target/release/screenreel}"
    [ -x "$bin" ] || die "binary not found: $bin (build first)"
    Xvfb :87 -screen 0 1280x720x24 >/dev/null 2>&1 &
    xp=$!
    trap 'kill $xp 2>/dev/null || true' EXIT INT TERM
    sleep 1
    out="$(mktemp -d)"
    DISPLAY=:87 "$bin" --selftest "$out" 3
    if command -v ffprobe >/dev/null 2>&1; then
        ffprobe -v error -show_entries stream=codec_name,width,height,sample_rate -of compact "$out/selftest.mkv"
        ffmpeg -v error -i "$out/selftest.mkv" -f null - && log "ffmpeg decode OK"
    fi
    rm -rf "$out"
}

case "${1:-}" in
    native) cmd_native ;;
    linux-static) cmd_linux_static ;;
    windows) cmd_windows ;;
    selftest) shift; cmd_selftest "$@" ;;
    *) sed -n '2,12p' "$0"; exit 1 ;;
esac
