#!/bin/sh
# Describes one built libkrunfw for the stackbox-libkrunfw bucket, the way src/kernel.rs reads it.
#
#   scripts/libkrunfw-publish.sh kid MANIFEST
#       the kernel id: the first 16 hex digits of SHA-256 over the manifest's bytes, which is
#       src/kernel.rs::id_for. The bucket keys <kid>/ on it.
#   scripts/libkrunfw-publish.sh index MANIFEST OS ARCH
#       the msb-<version>/<os>-<arch>.json document for MANIFEST, on stdout. The library it
#       names is read from the build directory beside the manifest's architecture directory.
#   scripts/libkrunfw-publish.sh file-name MANIFEST
#       the library's file name, for the object path <kid>/<os>-<arch>/<file-name>.
#
# OS is the bucket's name for the platform: linux or darwin.
set -eu

die() {
    echo "$*" >&2
    exit 1
}

sha256_of() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | cut -d' ' -f1
    else
        shasum -a 256 "$1" | cut -d' ' -f1
    fi
}

# The manifest's artifact line for the platform: "<path> sha256 <digest>". A dylib on darwin,
# the libkrunfw.so.* library everywhere else, as Manifest::artifact_for picks it.
artifact_line() {
    case $2 in
    darwin) pattern='\.dylib$' ;;
    *) pattern='/libkrunfw\.so\.[^/]*$|^libkrunfw\.so\.[^/]*$' ;;
    esac
    awk '$1 == "artifact" && $3 == "sha256" { print $2, $4 }' "$1" |
        grep -E "$pattern" | head -n 1
}

[ $# -ge 2 ] || die "usage: $0 kid|index|file-name MANIFEST [OS ARCH]"
command=$1
manifest=$2
[ -f "$manifest" ] || die "$manifest is not a file"

case $command in
kid)
    sha256_of "$manifest" | cut -c1-16
    ;;
file-name)
    line=$(artifact_line "$manifest" linux)
    [ -n "$line" ] || die "$manifest names no libkrunfw.so library"
    printf '%s\n' "${line%% *}" | sed 's|.*/||'
    ;;
index)
    [ $# -eq 4 ] || die "usage: $0 index MANIFEST OS ARCH"
    os=$3
    arch=$4
    case $os in linux | darwin) ;; *) die "unknown os $os" ;; esac
    msb=$(awk '$1 == "msb" { print $2; exit }' "$manifest")
    [ -n "$msb" ] && [ "$msb" != none ] || die "$manifest states no msb"
    revision=$(awk '$1 == "revision" { print $2; exit }' "$manifest")
    manifest_arch=$(awk '$1 == "arch" { print $2; exit }' "$manifest")
    [ "$manifest_arch" = "$arch" ] || die "$manifest is for $manifest_arch, not $arch"
    line=$(artifact_line "$manifest" "$os")
    [ -n "$line" ] || die "$manifest names no library for $os"
    rel=${line%% *}
    digest=${line#* }
    file="$(dirname "$manifest")/../$rel"
    [ -f "$file" ] || die "$file is missing"
    [ "$(sha256_of "$file")" = "$digest" ] || die "$file does not match its manifest sha256"
    kid=$(sha256_of "$manifest" | cut -c1-16)
    name=${rel##*/}
    size=$(wc -c <"$file" | tr -d ' ')
    printf '{\n'
    printf '  "kid": "%s",\n' "$kid"
    printf '  "msb": "%s",\n' "$msb"
    printf '  "os": "%s",\n' "$os"
    printf '  "arch": "%s",\n' "$arch"
    printf '  "revision": "%s",\n' "$revision"
    printf '  "manifest_path": "%s/manifest.txt",\n' "$kid"
    printf '  "artifact_path": "%s/%s-%s/%s",\n' "$kid" "$os" "$arch" "$name"
    printf '  "sha256": "%s",\n' "$digest"
    printf '  "size": %s\n' "$size"
    printf '}\n'
    ;;
*)
    die "unknown command $command"
    ;;
esac
