#!/bin/sh
# shellcheck disable=SC2016 # the awk programs below are single-quoted on purpose
# Records a release version everywhere the repo states one: the crate and the portal.
# Usage: sh scripts/set-version.sh 1.2.3
set -eu

version=${1:?usage: set-version.sh X.Y.Z}
if ! printf '%s\n' "$version" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$'; then
    echo "not a X.Y.Z version: $version" >&2
    exit 1
fi
cd "$(dirname "$0")/.."

# Rewrites a file through awk in place, keeping its permissions.
rewrite() {
    file=$1
    shift
    awk -v v="$version" "$@" "$file" >"$file.tmp"
    cat "$file.tmp" >"$file"
    rm "$file.tmp"
}

# The first version line is the [package] one; dependency versions never start a line.
rewrite Cargo.toml '!done && /^version = "/ { $0 = "version = \"" v "\""; done = 1 } 1'
rewrite Cargo.lock 'prev == "name = \"farm\"" && /^version = "/ { $0 = "version = \"" v "\"" } { prev = $0; print }'

node -e '
const fs = require("fs");
const [version, ...files] = process.argv.slice(1);
for (const file of files) {
  const json = JSON.parse(fs.readFileSync(file, "utf8"));
  json.version = version;
  if (json.packages?.[""]) json.packages[""].version = version;
  fs.writeFileSync(file, JSON.stringify(json, null, 2) + "\n");
}
' "$version" web/package.json web/package-lock.json
