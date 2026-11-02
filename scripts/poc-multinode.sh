#!/bin/sh
# PoC: one control and two compute VMs joined by farm-link and deployed with Kolla multinode,
# then a compute isolated and its instance evacuated (see docs/poc-l2-link.md).
# Needs the former Docker-built farm-golden:rocky10 image; see "다른 머신에서 이어가기" there.
#   sh scripts/poc-multinode.sh up              start the switch, create and link the VMs
#   sh scripts/poc-multinode.sh deploy [FROM [TO]]  install kolla-ansible on ctl and deploy
#   sh scripts/poc-multinode.sh evacuate        boot an instance on c1, isolate c1, evacuate
#   sh scripts/poc-multinode.sh isolate|heal N  cut or restore both links of node N
#   sh scripts/poc-multinode.sh down            stop the switch and remove the VMs
# Each VM has two links: farm0 (10.254.0.x, Kolla's network and API interface) and farm1
# (no address, OVS br-tenant carrying tenant VLANs on physnet1).
set -eu
cd "$(dirname "$0")/.."
NODES="ctl c1 c2"
GUEST=scripts/poc-multinode
vm() { echo "farm-mn-$1"; }
addr() { case $1 in ctl) echo 10.254.0.10 ;; c1) echo 10.254.0.11 ;; c2) echo 10.254.0.12 ;; esac; }
on() { n=$1; shift; msb exec "$(vm "$n")" -- "$@"; }
impair() { for port in "$1" "$1-t"; do curl -s -XPOST "127.0.0.1:7700/impair?name=$port&loss=$2"; done; }

case "${1:-}" in
up)
  cargo build --release --bin farm-link
  docker run --rm --platform linux/arm64 -v "$PWD":/build -w /build \
    -v farm-poc-cargo:/usr/local/cargo/registry -v farm-poc-target:/target -e CARGO_TARGET_DIR=/target \
    rust:1.99.1-bookworm sh -c 'cargo build --release --bin farm-link && cp /target/release/farm-link /build/target/farm-link-linux'
  pgrep -f "farm-link switch" >/dev/null || (./target/release/farm-link switch --listen 127.0.0.1:7700 >/tmp/poc-switch.log 2>&1 &)
  for n in $NODES; do
    case $n in ctl) cpus=4 mem=6G ;; *) cpus=2 mem=3G ;; esac
    msb list | grep -q "^$(vm "$n") " || msb create -q --name "$(vm "$n")" -c $cpus -m $mem --root-disk 20G \
      --mount-owned /var/lib/docker:kind=disk,size=32G --mount-owned /var/lib/containerd:kind=disk,size=32G \
      --init /sbin/init -H "mn-$n" --net public,host farm-golden:rocky10
    on "$n" sh -c 'for _ in $(seq 120); do case $(systemctl is-system-running 2>/dev/null) in running|degraded) exit 0;; esac; sleep 1; done; exit 1'
    msb copy target/farm-link-linux "$(vm "$n")":/usr/local/bin/farm-link
    msb copy "$GUEST/farm-link@.service" "$(vm "$n")":/etc/systemd/system/farm-link@.service
    msb copy "$GUEST/prepare.sh" "$(vm "$n")":/usr/local/sbin/farm-prepare
    on "$n" sh -c "set -e
      chmod 755 /usr/local/bin/farm-link /usr/local/sbin/farm-prepare
      printf 'SWITCH=ws://host.microsandbox.internal:7700/link?name=$n\nADDRESS=$(addr "$n")/24\n' > /etc/farm/link-farm0.conf
      printf 'SWITCH=ws://host.microsandbox.internal:7700/link?name=$n-t\nADDRESS=\n' > /etc/farm/link-farm1.conf
      printf 'FARM_HOSTNAME=mn-$n\nFARM_ADDRESS=$(addr "$n")\nFARM_PREFIX=24\nFARM_PORTS=\"22\"\n' > /etc/farm/identity.conf
      systemctl daemon-reload
      systemctl enable --now farm-link@farm0 farm-link@farm1
      systemctl restart farm-identity farm-hostname
      farm-prepare $n"
  done
  msb copy src/kolla/neutron_sitecustomize.py "$(vm ctl)":/etc/farm/neutron-sitecustomize.py
  curl -s 127.0.0.1:7700/ports
  for n in c1 c2; do on ctl ping -c 2 -W 2 "$(addr "$n")" | tail -1; done
  ;;
deploy)
  for f in install.sh deploy.sh; do msb copy "$GUEST/$f" "$(vm ctl)":/root/$f; done
  on ctl bash /root/install.sh
  # The control VM runs Ansible and reaches the computes over farm0. The image's
  # authorized_keys has no final newline.
  key=$(on ctl cat /root/.ssh/id_ed25519.pub)
  for n in $NODES; do
    on "$n" sh -c "f=/root/.ssh/authorized_keys; sed -i '/^\$/d; \\|^$key\$|d; s| *$key\$||; \$a\\' \$f; echo '$key' >> \$f"
  done
  on ctl bash /root/deploy.sh "${2:-configure}" "${3:-post-deploy}"
  ;;
evacuate)
  msb copy "$GUEST/evacuate.sh" "$(vm ctl)":/root/evacuate.sh
  on ctl bash /root/evacuate.sh boot
  echo "==> isolate c1 ($(date +%T))"
  impair c1 1
  on ctl bash /root/evacuate.sh evacuate
  ;;
isolate) impair "$2" 1 ;;
heal) impair "$2" 0 ;;
down)
  pkill -f "farm-link switch" || true
  for n in $NODES; do msb stop "$(vm "$n")" 2>/dev/null || true; msb rm "$(vm "$n")" 2>/dev/null || true; done
  ;;
*) sed -n '2,10p' "$0"; exit 2 ;;
esac
