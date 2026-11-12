#!/bin/sh
# Turns the stock rockylinux/rockylinux:10 image into the farm golden image. It runs once per
# host in a VM booted without init; the VM's disks then become the golden snapshot.
# Farm files (identity, hostname, external network) are added to each farm VM instead,
# so this script only installs what any farm guest needs from the distribution.
set -eu
# systemd boots the guest. Kolla's bootstrap-servers installs Docker; the Kolla controller
# runs in the guest and builds its Python dependencies with gcc and the -devel headers.
# python3-dbus serves Kolla tasks delegated to this guest under the system python.
# systemd-udev is separate from systemd: libvirt's virNodeDeviceCreateXML runs `mdevctl
# start` and waits for udev to register the new mediated device, so a compute without udev
# never sees an mdev and a vGPU instance fails to build.
dnf install -y -q \
  systemd systemd-udev dbus-broker \
  openssh-server openssh-clients sudo \
  python3 python3-pip python3-libselinux python3-dbus git \
  iproute iputils procps-ng hostname which chrony tar nftables \
  gcc python3-devel libffi-devel openssl-devel
dnf clean all
systemctl enable sshd chronyd
mkdir -p /root/.ssh /etc/farm
chmod 700 /root/.ssh
