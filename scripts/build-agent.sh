#!/bin/sh
set -eu
cd "$(dirname "$0")/.."
arch=${1:-$(uname -m)}
case "$arch" in
    arm64|aarch64) platform=linux/arm64; arch=aarch64 ;;
    amd64|x86_64) platform=linux/amd64; arch=x86_64 ;;
    darwin-aarch64|windows-x86_64) platform= ;;
    *) echo 'Usage: scripts/build-agent.sh [aarch64|x86_64|darwin-aarch64|windows-x86_64]' >&2; exit 2 ;;
esac
# build.rs hashes the sources it compiles, so the artifact and the coordinator agree on a
# build id without passing a revision in here.
if [ -n "$platform" ]; then
    docker buildx build --platform "$platform" -f docker/agent.Dockerfile \
        --target artifact --output "type=local,dest=dist/agent/$arch" .
    printf '\nFARM_AGENT_PATH=%s/dist/agent/%s/farm-agent cargo run -- serve\n' "$PWD" "$arch"
    exit 0
fi
# The macOS link needs a MacOSX.sdk (only its .tbd stubs are read); on a Mac, Xcode's.
if [ "$arch" = darwin-aarch64 ]; then
    sdk=${MACOS_SDK:-$(xcrun --show-sdk-path 2>/dev/null || true)}
    [ -d "$sdk" ] || { echo 'set MACOS_SDK to a MacOSX.sdk directory' >&2; exit 2; }
else
    sdk=$(mktemp -d)
    trap 'rmdir "$sdk"' EXIT
fi
docker buildx build -f docker/agent-cross.Dockerfile --build-arg "TARGET=$arch" \
    --build-context "sdk=$sdk" --target artifact --output "type=local,dest=dist/agent/$arch" .
