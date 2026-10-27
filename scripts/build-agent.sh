#!/bin/sh
set -eu
cd "$(dirname "$0")/.."
arch=${1:-$(uname -m)}
case "$arch" in
    arm64|aarch64) platform=linux/arm64; arch=aarch64 ;;
    amd64|x86_64) platform=linux/amd64; arch=x86_64 ;;
    *) echo 'Usage: scripts/build-agent.sh [aarch64|x86_64]' >&2; exit 2 ;;
esac
# The agent records the revision it was built from, so the coordinator can tell a stale
# artifact from a current one.
if [ -n "${FARM_BUILD_ID:-}" ]; then
    build_id=$FARM_BUILD_ID
elif git rev-parse --short=12 HEAD >/dev/null 2>&1; then
    build_id=$(git rev-parse --short=12 HEAD)
    [ -z "$(git status --porcelain)" ] || build_id="$build_id-dirty"
else
    build_id=unknown
fi
docker buildx build --platform "$platform" -f docker/agent.Dockerfile \
    --build-arg "FARM_BUILD_ID=$build_id" \
    --target artifact --output "type=local,dest=dist/agent/$arch" .
printf '\nFARM_AGENT_PATH=%s/dist/agent/%s/farm-agent cargo run -- serve\n' "$PWD" "$arch"
