#!/bin/sh
# Succeeds when a release published the farm-agents for the build of a coordinator binary
# (default target/release/farm). That coordinator downloads them, so none need building.
set -eu
cd "$(dirname "$0")/.."
stamp=$(grep -aoE 'farm-agent-build:[0-9a-f]{16}' "${1:-target/release/farm}" 2>/dev/null | head -n 1)
[ -n "$stamp" ] || exit 1
url=${FARM_AGENT_URL-https://public-bucket.preinpost.in/farm-agent}
[ -n "$url" ] || exit 1
curl -fsI -m 10 "$url/${stamp#farm-agent-build:}/manifest.json" >/dev/null
