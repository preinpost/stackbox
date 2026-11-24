#!/bin/sh
# Succeeds when a release published the stackbox-agents for the build of a coordinator binary
# (default target/release/stackbox). That coordinator downloads them, so none need building.
set -eu
cd "$(dirname "$0")/.."
stamp=$(grep -aoE 'stackbox-agent-build:[0-9a-f]{16}' "${1:-target/release/stackbox}" 2>/dev/null | head -n 1)
[ -n "$stamp" ] || exit 1
url=${STACKBOX_AGENT_URL-https://public-bucket.preinpost.in/stackbox-agent}
[ -n "$url" ] || exit 1
curl -fsI -m 10 "$url/${stamp#stackbox-agent-build:}/manifest.json" >/dev/null
