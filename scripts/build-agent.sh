#!/bin/sh
set -eu
cd "$(dirname "$0")/.."
arch=${1:-$(uname -m)}
case "$arch" in
    arm64|aarch64) platform=linux/arm64; arch=aarch64 ;;
    amd64|x86_64) platform=linux/amd64; arch=x86_64 ;;
    *) echo 'Usage: scripts/build-agent.sh [aarch64|x86_64]' >&2; exit 2 ;;
esac
# build.rs hashes the sources it compiles, so the artifact and the coordinator agree on a
# build id without passing a revision in here.
docker buildx build --platform "$platform" -f docker/agent.Dockerfile \
    --target artifact --output "type=local,dest=dist/agent/$arch" .
printf '\nFARM_AGENT_PATH=%s/dist/agent/%s/farm-agent cargo run -- serve\n' "$PWD" "$arch"
