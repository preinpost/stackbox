#!/bin/sh
# PoC: two msb VMs on one L2 carried over WebSocket by farm-link (see docs/poc-l2-link.md).
#   sh scripts/poc-l2-link.sh up      build, create farm-poc-a/b, start the switch and links
#   sh scripts/poc-l2-link.sh bench   ping and iperf3 between them
#   sh scripts/poc-l2-link.sh down    stop the switch and remove the VMs
# Impair a link while it runs: curl -XPOST '127.0.0.1:7700/impair?name=b&delay_ms=50&loss=0.01'
set -eu
cd "$(dirname "$0")/.."
MTU=${MTU:-1500}
case "${1:-up}" in
up)
  cargo build --release --bin farm-link
  docker run --rm --platform linux/arm64 -v "$PWD":/build -w /build \
    -v farm-poc-cargo:/usr/local/cargo/registry -v farm-poc-target:/target -e CARGO_TARGET_DIR=/target \
    rust:1.99.1-bookworm sh -c 'cargo build --release --bin farm-link && cp /target/release/farm-link /build/target/farm-link-linux'
  pkill -f "farm-link switch" || true
  (./target/release/farm-link switch --listen 127.0.0.1:7700 >/tmp/poc-switch.log 2>&1 &)
  i=2
  for n in a b; do
    msb list | grep -q "^farm-poc-$n " || msb create -q --name farm-poc-$n -c 2 -m 1G --net public,host farm-golden:rocky10
    msb exec farm-poc-$n -- sh -c 'command -v iperf3 >/dev/null || dnf install -y -q iperf3 >/dev/null; pkill -f "[f]arm-link link" || true; ip link del farm0 2>/dev/null || true'
    msb copy target/farm-link-linux farm-poc-$n:/usr/local/bin/farm-link
    # The guest reaches the host's loopback as host.microsandbox.internal.
    msb exec farm-poc-$n -- sh -c "chmod +x /usr/local/bin/farm-link
      nohup farm-link link --switch 'ws://host.microsandbox.internal:7700/link?name=$n' --dev farm0 >/var/log/farm-link.log 2>&1 &
      sleep 1; ip link set farm0 mtu $MTU up; ip addr add 10.254.0.$i/24 dev farm0"
    i=$((i + 1))
  done
  curl -s 127.0.0.1:7700/ports
  ;;
bench)
  msb exec farm-poc-b -- sh -c 'pgrep iperf3 >/dev/null || iperf3 -s -D'
  msb exec farm-poc-a -- sh -c 'ping -c 200 -i 0.05 10.254.0.3 | tail -1'
  msb exec farm-poc-a -- iperf3 -c 10.254.0.3 -t 8 -f m | grep -E 'sender|receiver'
  msb exec farm-poc-a -- iperf3 -c 10.254.0.3 -t 8 -f m -R | grep receiver
  ;;
down)
  pkill -f "farm-link switch" || true
  msb stop farm-poc-a farm-poc-b 2>/dev/null || true
  msb rm farm-poc-a farm-poc-b 2>/dev/null || true
  ;;
esac
