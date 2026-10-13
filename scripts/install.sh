#!/bin/sh
# Draft installer for the unpublished v0.1.0 binary assets.
set -eu

version=0.1.0
repository=SmallThingz/ldu

fail() {
    printf 'ldu: %s\n' "$*" >&2
    exit 1
}

command -v curl >/dev/null 2>&1 || fail 'curl is required'
command -v od >/dev/null 2>&1 || fail 'od is required'

system=$(uname -s)
architecture=$(uname -m)
case "$system/$architecture" in
    Linux/x86_64|Linux/amd64) target=x86_64-unknown-linux-musl ;;
    Linux/aarch64|Linux/arm64) target=aarch64-unknown-linux-musl ;;
    Linux/armv7l|Linux/armv7) target=armv7-unknown-linux-musleabihf ;;
    Linux/riscv64) target=riscv64gc-unknown-linux-musl ;;
    Darwin/arm64|Darwin/aarch64) target=aarch64-apple-darwin ;;
    Darwin/x86_64)
        if [ "$(sysctl -n hw.optional.arm64 2>/dev/null || true)" = 1 ]; then
            target=aarch64-apple-darwin
        else
            target=x86_64-apple-darwin
        fi
        ;;
    FreeBSD/amd64|FreeBSD/x86_64) target=x86_64-unknown-freebsd ;;
    FreeBSD/arm64|FreeBSD/aarch64) target=aarch64-unknown-freebsd ;;
    *) fail "no binary for $system/$architecture" ;;
esac

install_dir=${LDU_INSTALL_DIR:-${HOME:?HOME is not set}/.local/bin}
[ -n "$install_dir" ] || fail 'installation directory is empty'
case "$install_dir" in
    /*) ;;
    *) install_dir=$PWD/$install_dir ;;
esac
mkdir -p "$install_dir"
[ ! -d "$install_dir/ldu" ] || fail "$install_dir/ldu is a directory"
work_dir=$(mktemp -d "$install_dir/.ldu-install.XXXXXXXX")
cleanup() {
    rm -rf "$work_dir"
}
trap cleanup 0
trap 'exit 130' INT
trap 'exit 143' TERM
trap 'exit 129' HUP

binary=$work_dir/ldu
url=https://github.com/$repository/releases/download/v$version/ldu-v$version-$target
curl --proto '=https' --tlsv1.2 --fail --location --retry 2 \
    --connect-timeout 10 --max-time 120 --max-filesize 16777216 \
    --output "$binary" "$url"
[ -s "$binary" ] || fail 'downloaded binary is empty'
# Inspect the complete fixed header without executing the download. od -v
# retains repeated zero bytes, and byte-sized fields are independent of host endian.
header_bytes=$(od -An -v -tx1 -N64 "$binary")
set -- $header_bytes
bad_header() {
    fail "downloaded binary has an invalid header for $target"
}
[ "$#" -ge 4 ] || bad_header
magic=$1$2$3$4
case "$system/$magic" in
    Darwin/cffaedfe)
        [ "$#" -ge 32 ] || bad_header
        case "$target/${5}${6}${7}${8}" in
            x86_64-apple-darwin/07000001|aarch64-apple-darwin/0c000001) ;;
            *) bad_header ;;
        esac
        [ "${13}${14}${15}${16}" = 02000000 ] || bad_header
        ;;
    Linux/7f454c46|FreeBSD/7f454c46)
        [ "$#" -ge 52 ] || bad_header
        [ "$6$7" = 0101 ] || bad_header
        case "$system/$8" in
            Linux/00|Linux/03|FreeBSD/09) ;;
            *) bad_header ;;
        esac
        case "${17}${18}" in
            0200|0300) ;;
            *) bad_header ;;
        esac
        [ "${21}${22}${23}${24}" = 01000000 ] || bad_header
        case "$target/$5/${19}${20}" in
            armv7-unknown-linux-musleabihf/01/2800)
                [ "${41}${42}" = 3400 ] || bad_header
                # ARM EABI v5, hard-float calling convention, never soft-float.
                [ "${40}" = 05 ] && [ "$((0x${38} & 6))" -eq 4 ] || bad_header
                ;;
            x86_64-*/02/3e00|aarch64-*/02/b700|riscv64gc-*/02/f300)
                [ "$#" -ge 64 ] || bad_header
                [ "${53}${54}" = 4000 ] || bad_header
                if [ "$target" = riscv64gc-unknown-linux-musl ]; then
                    [ "$((0x${49} & 6))" -eq 4 ] || bad_header
                fi
                ;;
            *) bad_header ;;
        esac
        ;;
    *) bad_header ;;
esac
chmod 0755 "$binary"
# Staging keeps replacement on one filesystem. Passing the parent also rejects
# an ldu directory created during download instead of installing inside it.
mv -f "$binary" "$install_dir/"
printf 'Installed ldu %s to %s/ldu\n' "$version" "$install_dir"
