#!/bin/sh
# Succeeds when a GitHub release published the stackbox-agents for the build of a coordinator
# binary: the binary's stamp must match the build_id of v<version>/manifest.json there.
#
#   scripts/agents-published.sh [BINARY] [VERSION]
#       BINARY defaults to target/release/stackbox; VERSION defaults to the package version in
#       Cargo.toml. A coordinator downloads the published agents, so none need building.
set -eu
cd "$(dirname "$0")/.."
bin=${1:-target/release/stackbox}
stamp=$(grep -aoE 'stackbox-agent-build:[0-9a-f]{16}' "$bin" 2>/dev/null | head -n 1)
[ -n "$stamp" ] || exit 1
build_id=${stamp#stackbox-agent-build:}
version=${2:-$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -n 1)}
[ -n "$version" ] || exit 1
url=${STACKBOX_AGENT_URL-https://github.com/preinpost/stackbox/releases/download}
[ -n "$url" ] || exit 1
manifest=$(curl -fsS -m 20 "$url/v$version/manifest.json") || exit 1
printf '%s\n' "$manifest" |
    grep -Eq "\"build_id\"[[:space:]]*:[[:space:]]*\"$build_id\""
