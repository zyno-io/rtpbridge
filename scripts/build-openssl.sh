#!/bin/sh
# Build the same patched static OpenSSL for development, CI and the container.
set -eu
version=3.6.4
sha256=9bffaa1ad1e07b354c21bd3324ec02fa15579f45a7d0494b3e74bc449b7333ef
prefix=${1:?Usage: sh scripts/build-openssl.sh INSTALL_DIRECTORY}
mkdir -p "$prefix"
prefix=$(cd "$prefix" && pwd)
if [ -f "$prefix/.rtpbridge-openssl-sha256" ] && [ "$(cat "$prefix/.rtpbridge-openssl-sha256")" = "$sha256" ] && [ -f "$prefix/lib/libssl.a" ] && [ -f "$prefix/lib/libcrypto.a" ]; then
    exit 0
fi
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT HUP INT TERM
curl --fail --location --retry 3 --proto '=https' --tlsv1.2 \
    "https://github.com/openssl/openssl/releases/download/openssl-$version/openssl-$version.tar.gz" \
    -o "$work/openssl.tar.gz"
if command -v sha256sum >/dev/null 2>&1; then
    actual=$(sha256sum "$work/openssl.tar.gz")
else
    actual=$(shasum -a 256 "$work/openssl.tar.gz")
fi
[ "${actual%% *}" = "$sha256" ] || { echo 'OpenSSL checksum mismatch' >&2; exit 1; }
tar -xzf "$work/openssl.tar.gz" -C "$work"
cd "$work/openssl-$version"
./Configure --prefix="$prefix" --openssldir="$prefix/ssl" --libdir=lib no-shared no-tests
make -j 4
make install_sw
printf '%s\n' "$sha256" > "$prefix/.rtpbridge-openssl-sha256"
