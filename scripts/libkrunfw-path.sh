#!/bin/sh
# Prints the guest kernel (libkrunfw) scripts/build-libkrunfw.sh built for this OS and
# architecture: <out>/<arch>/libkrunfw.<abi>.dylib on macOS, <out>/<arch>/libkrunfw.so.<version>
# on Linux. farm installs that kernel on each host during preparation (docs/libkrunfw.md), so
# this is for checking the build or trying it with the msb CLI; FARM_LIBKRUNFW_PATH still
# overrides the kernel for one process.
#
# Only the path goes to stdout, so a caller can use it directly:
#   export FARM_LIBKRUNFW_PATH="$(sh scripts/libkrunfw-path.sh)"
#
# Usage: scripts/libkrunfw-path.sh [--build]
#
# LIBKRUNFW_OUT names the output directory (default: target/libkrunfw), as it does for the
# build script.
set -eu
cd "$(dirname "$0")/.."

usage() {
    cat <<'USAGE'
Usage: scripts/libkrunfw-path.sh [--build]

Print the custom guest kernel (libkrunfw) this machine loads, building it first when it is
missing. Only the path goes to stdout; build progress goes to stderr.

  --build     run the build script even though a kernel is already there, so a changed
              fragment or revision is picked up
  -h, --help  print this text
USAGE
}

die() {
    echo "$*" >&2
    exit 1
}

build=
while [ $# -gt 0 ]; do
    case $1 in
    --build) build=1 ;;
    -h | --help)
        usage
        exit 0
        ;;
    *) usage >&2; die "unknown option: $1" ;;
    esac
    shift
done

host_arch=$(uname -m)
case $host_arch in
arm64 | aarch64) host_arch=aarch64 ;;
x86_64 | amd64) host_arch=x86_64 ;;
*) die "unsupported machine architecture: $(uname -m)" ;;
esac
host_os=$(uname -s)
case $host_os in
Darwin) pattern='libkrunfw.*.dylib' ;;
Linux) pattern='libkrunfw.so.*' ;;
*) die "$host_os cannot load a custom libkrunfw; only macOS and Linux do" ;;
esac

out=${LIBKRUNFW_OUT:-target/libkrunfw}
case $out in
/*) ;;
*) out=$PWD/$out ;;
esac
dir=$out/$host_arch

# The manifest names the artifacts a build wrote, so a kernel built for other settings or
# another revision is not picked up by its name alone. An artifact the manifest does not
# list (an older build directory) still counts when a file matches.
artifact_path() {
    path=
    if [ -f "$dir/manifest.txt" ]; then
        while read -r kind relative _; do
            [ "$kind" = artifact ] || continue
            # shellcheck disable=SC2254 # the manifest names a glob (libkrunfw.*.dylib)
            case ${relative##*/} in
            $pattern) ;;
            *) continue ;;
            esac
            [ -s "$out/$relative" ] || continue
            path=$out/$relative
        done <"$dir/manifest.txt"
    fi
    if [ -z "$path" ]; then
        for candidate in "$dir"/$pattern; do
            [ -s "$candidate" ] || continue
            path=$candidate
        done
    fi
    [ -z "$path" ] || printf '%s\n' "$path"
}

kernel=$(artifact_path)
if [ -n "$build" ] || [ -z "$kernel" ]; then
    if [ -z "$kernel" ]; then
        echo "farm: no custom kernel for $host_arch in $dir yet; building it (docs/libkrunfw.md)" >&2
    else
        echo "farm: rebuilding the custom kernel for $host_arch in $dir" >&2
    fi
    # The build script reports what it does on stdout; keep stdout for the path alone.
    sh scripts/build-libkrunfw.sh >&2 || die 'the kernel build failed'
    kernel=$(artifact_path)
fi
[ -n "$kernel" ] || die "the kernel build left no $pattern in $dir"
printf '%s\n' "$kernel"
