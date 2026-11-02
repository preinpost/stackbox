#!/bin/bash
# Runs on the control VM. The host isolates mn-c1 between the two steps.
#   evacuate.sh boot       Cirros on mn-c1 behind a router on public; ping it from the router
#   evacuate.sh evacuate   wait until mn-c1 is down, force it down, evacuate to mn-c2, ping again
set -euo pipefail
. /etc/kolla/admin-openrc.sh
os=/opt/osc/bin/openstack
name=farm-evac
log() { echo "==> $* ($(date +%T))"; }

wait_active() {
  for _ in $(seq 90); do
    status=$($os server show "$name" -f value -c status)
    case "$status" in
      ACTIVE) return 0 ;;
      ERROR) $os server show "$name" -f value -c fault >&2; return 1 ;;
    esac
    sleep 10
  done
  echo "server status: $status" >&2
  return 1
}

ping_from_router() {
  router=$($os router show "$name" -f value -c id)
  fixed=$($os port list --server "$name" -f json -c "Fixed IP Addresses" | python3 -c 'import json,sys; print(json.load(sys.stdin)[0]["Fixed IP Addresses"][0]["ip_address"])')
  for _ in $(seq 60); do
    ip netns exec "qrouter-$router" ping -c1 -W2 "$fixed" >/dev/null && break
    sleep 10
  done
  ip netns exec "qrouter-$router" ping -c3 -W2 "$fixed"
}

show() { $os server show "$name" -f value -c OS-EXT-SRV-ATTR:host -c status -c addresses; }

case "${1:-}" in
boot)
  log "image, flavor and network"
  $os flavor show m1.tiny >/dev/null 2>&1 || $os flavor create --ram 512 --disk 1 --vcpus 1 m1.tiny >/dev/null
  if ! $os image show cirros >/dev/null 2>&1; then
    if [ "$(uname -m)" = aarch64 ]; then
      url=https://download.cirros-cloud.net/0.6.3/cirros-0.6.3-aarch64-disk.img
      props='--property hw_firmware_type=uefi --property hw_machine_type=virt'
    else
      url=https://download.cirros-cloud.net/0.6.3/cirros-0.6.3-x86_64-disk.img
      props=''
    fi
    curl -fsSL -o /tmp/cirros.img "$url"
    # shellcheck disable=SC2086
    $os image create --disk-format qcow2 --container-format bare --property os_distro=cirros $props --file /tmp/cirros.img cirros >/dev/null
  fi
  $os security group rule create --ingress --protocol icmp --ethertype IPv4 default >/dev/null 2>&1 || true
  # A VLAN network on physnet1: frames between mn-ctl and the computes cross farm1.
  $os network show "$name" >/dev/null 2>&1 || $os network create "$name" >/dev/null
  $os subnet show "$name" >/dev/null 2>&1 || $os subnet create --network "$name" --subnet-range 10.197.0.0/24 "$name" >/dev/null
  if ! $os router show "$name" >/dev/null 2>&1; then
    $os router create --external-gateway public "$name" >/dev/null
    $os router add subnet "$name" "$name"
  fi
  $os network show "$name" -f value -c provider:network_type -c provider:physical_network -c provider:segmentation_id
  log "boot on mn-c1"
  $os server show "$name" >/dev/null 2>&1 || $os server create --flavor m1.tiny --image cirros --network "$name" \
    --availability-zone nova:mn-c1 "$name" >/dev/null
  wait_active
  show
  log "ping from the router on mn-ctl"
  ping_from_router
  ;;
evacuate)
  log "wait for nova to see mn-c1 down"
  started=$(date +%s)
  for _ in $(seq 60); do
    state=$($os compute service list --host mn-c1 --service nova-compute -f value -c State)
    [ "$state" = down ] && break
    sleep 5
  done
  [ "$state" = down ] || { echo "mn-c1 is still $state" >&2; exit 1; }
  echo "mn-c1 down after $(( $(date +%s) - started ))s"
  # Fencing stand-in: the isolated VM can no longer reach anything, so mark it forced down.
  $os --os-compute-api-version 2.11 compute service set --down mn-c1 nova-compute
  log "evacuate to mn-c2"
  started=$(date +%s)
  $os --os-compute-api-version 2.29 server evacuate --host mn-c2 "$name" >/dev/null
  sleep 5
  wait_active
  echo "ACTIVE after $(( $(date +%s) - started ))s"
  show
  log "ping from the router on mn-ctl"
  ping_from_router
  $os server event list "$name" -f value -c Action -c "Start Time"
  ;;
*) sed -n '2,4p' "$0"; exit 2 ;;
esac
