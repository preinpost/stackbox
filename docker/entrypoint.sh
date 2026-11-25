#!/usr/bin/env bash
# Runs the stackbox coordinator API (loopback) and the web portal (container port 10020) together.
# Exits as soon as either process ends, stopping the other; SIGTERM/SIGINT stop both and exit 0.
set -euo pipefail

STACKBOX_STATE_DIR="${STACKBOX_STATE_DIR:-/data}"
STACKBOX_API_BIND="${STACKBOX_API_BIND:-127.0.0.1:10010}"
PORT="${PORT:-10020}"
PORTAL_HOST="${PORTAL_HOST:-0.0.0.0}"
STACKBOX_API_URL="${STACKBOX_API_URL:-http://$STACKBOX_API_BIND}"
export STACKBOX_API_BIND PORT PORTAL_HOST STACKBOX_API_URL

# `stackbox serve` rejects an empty token, so a blank STACKBOX_API_TOKEN means "no token".
if [ -z "${STACKBOX_API_TOKEN:-}" ]; then
  unset STACKBOX_API_TOKEN
  echo "entrypoint: warning: STACKBOX_API_TOKEN is not set; the API has no bearer authentication" >&2
fi

# Debugging escape hatch: `docker run ... stackbox-entrypoint <command>` runs that command instead.
if [ "$#" -gt 0 ]; then
  exec "$@"
fi

mkdir -p "$STACKBOX_STATE_DIR"

api_pid=""
portal_pid=""

stop_all() {
  local code=$1 pid
  trap '' INT TERM
  for pid in "$portal_pid" "$api_pid"; do
    [ -n "$pid" ] && kill -TERM "$pid" 2>/dev/null || true
  done
  for pid in "$portal_pid" "$api_pid"; do
    [ -n "$pid" ] && wait "$pid" 2>/dev/null || true
  done
  exit "$code"
}
trap 'stop_all 0' INT TERM

# The binary reads STACKBOX_API_TOKEN from the environment itself; --state-dir is a flag only.
stackbox --state-dir "$STACKBOX_STATE_DIR" serve --bind "$STACKBOX_API_BIND" &
api_pid=$!

attempts=0
until curl -sS -o /dev/null "http://$STACKBOX_API_BIND/" 2>/dev/null; do
  attempts=$((attempts + 1))
  if [ "$attempts" -ge 60 ] || ! kill -0 "$api_pid" 2>/dev/null; then
    echo "entrypoint: stackbox serve did not become ready on $STACKBOX_API_BIND" >&2
    stop_all 1
  fi
  sleep 0.5
done

cd /app/web
node server/index.mjs &
portal_pid=$!
echo "entrypoint: stackbox API on $STACKBOX_API_BIND, portal on $PORTAL_HOST:$PORT" >&2

# wait -n returns when the first child exits (a trapped signal interrupts it instead).
code=0
exited=""
wait -n -p exited || code=$?
if [ "$exited" = "$api_pid" ]; then name=stackbox; else name=portal; fi
[ "$code" -eq 0 ] && code=1
echo "entrypoint: $name process exited with status $code; stopping the other process" >&2
stop_all "$code"
