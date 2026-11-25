set -eu
export PATH="$HOME/.microsandbox/bin:$HOME/.local/bin:$PATH"
export MSB_BACKEND=local
field() { printf 'STACKBOX\t%s\t%s\n' "$1" "$2"; }
field os "$(uname -s)"
field arch "$(uname -m)"
if [ -r /dev/kvm ] && [ -w /dev/kvm ]; then field kvm 1; else field kvm 0; fi
if [ "$(id -u)" = 0 ] || sudo -n true >/dev/null 2>&1; then field sudo 1; else field sudo 0; fi
field memory_total "$(awk '/^MemTotal:/ {print int($2/1024)}' /proc/meminfo 2>/dev/null || echo 0)"
field memory_available "$(awk '/^MemAvailable:/ {print int($2/1024)}' /proc/meminfo 2>/dev/null || echo 0)"
field disk_available "$(df -Pk "$HOME" | awk 'END {print int($4/1024)}')"
field msb "$(msb --version 2>/dev/null || true)"
# The upstream and fork msb share one version string, so ask the run help whether this msb takes
# both PCI flags. The help text is kept in a variable and matched by case, never piped.
msb_pci=0
msb_help=$(timeout 10 msb run --help 2>&1 || true)
case "$msb_help" in
    *--pci*--iommu* | *--iommu*--pci*) msb_pci=1 ;;
esac
field msb_pci "$msb_pci"
if timeout 10 docker info >/dev/null 2>&1; then field docker 1; else field docker 0; fi
field golden "$(if timeout 15 msb snapshot inspect '@GOLDEN@' >/dev/null 2>&1; then echo 1; else echo 0; fi)"
# A kernel host preparation installed under ~/.local/share/stackbox/libkrunfw and this host can
# boot: its manifest names this architecture and the msb the host runs. An installation that
# cannot be used is reported as the reason instead, so the coordinator can show it.
kernel=
kernel_note=
if [ -d "$HOME/.local/share/stackbox/libkrunfw" ]; then
    host_arch=$(uname -m)
    for dir in "$HOME/.local/share/stackbox/libkrunfw"/*; do
        [ -f "$dir/manifest.txt" ] || continue
        karch=$(sed -n 's/^arch //p' "$dir/manifest.txt")
        kmsb=$(sed -n 's/^msb //p' "$dir/manifest.txt")
        if [ "$karch" != "$host_arch" ]; then
            kernel_note="the installed kernel is for $karch, and this host is $host_arch"
            continue
        fi
        if [ "$kmsb" != "@MSB_VERSION@" ]; then
            kernel_note="the installed kernel was built for msb ${kmsb:-unknown}, and preparing this host installs @MSB_VERSION@"
            continue
        fi
        lib=$(sed -n 's#^artifact .*/\(libkrunfw\.so\.[^ ]*\) sha256 .*#\1#p' "$dir/manifest.txt")
        if [ -n "$lib" ] && [ -f "$dir/$lib" ]; then
            kernel="$dir/$lib"
        fi
    done
fi
field libkrunfw "$kernel"
field libkrunfw_note "$kernel_note"
# PCI passthrough: list only the devices bound to vfio-pci, the only ones a stackbox may pass through.
# A device is ready when its IOMMU group has no member bound to another driver and the group's
# /dev/vfio file is readable and writable by this user. The groups are read here, not by the
# coordinator, so the answer is the host's own.
field vfio "$(if [ -e /dev/vfio/vfio ]; then echo 1; else echo 0; fi)"
# Candidates for passthrough (every PCI device but bridges) and the protection rules that mark
# the devices a host must keep: the root disk's controller, the default route's NIC and the USB
# controllers with devices attached. The coordinator reads these; a stackbox is still refused
# any device marked protected.
SYSFS=/sys
@PCI_GUARD@
pci_root=$(findmnt -rno MAJ:MIN / 2>/dev/null || true)
pci_routes=$(ip route show default 2>/dev/null || true)
pci_protect_setup "$pci_root" "$pci_routes"
for dev in /sys/bus/pci/devices/*; do
    [ -e "$dev" ] || continue
    [ -L "$dev/driver" ] || continue
    driver=$(basename "$(readlink "$dev/driver")")
    [ "$driver" = vfio-pci ] || continue
    bdf=$(basename "$dev")
    vendor=$(sed 's/^0x//' "$dev/vendor" 2>/dev/null || true)
    product=$(sed 's/^0x//' "$dev/device" 2>/dev/null || true)
    class=$(sed 's/^0x//' "$dev/class" 2>/dev/null || true)
    group=
    group_link=$(readlink "$dev/iommu_group" 2>/dev/null || true)
    if [ -n "$group_link" ]; then group=$(basename "$group_link"); fi
    exclusive=0
    ready=0
    if [ -n "$group" ]; then
        exclusive=1
        for member in /sys/kernel/iommu_groups/"$group"/devices/*; do
            [ -e "$member" ] || continue
            [ -L "$member/driver" ] || continue
            member_driver=$(basename "$(readlink "$member/driver")")
            if [ "$member_driver" != vfio-pci ]; then exclusive=0; fi
        done
        if [ "$exclusive" = 1 ] && [ -r "/dev/vfio/$group" ] && [ -w "/dev/vfio/$group" ]; then ready=1; fi
    fi
    printf 'STACKBOX\tpci_device\t%s,%s,%s,%s,%s,%s,%s,%s\n' "$bdf" "$vendor" "$product" "$class" "$group" "$driver" "$exclusive" "$ready"
done
for dev in "$SYSFS"/bus/pci/devices/*; do
    [ -e "$dev" ] || continue
    class=$(sed 's/^0x//' "$dev/class" 2>/dev/null || true)
    case "$class" in
        06*) continue ;;
    esac
    bdf=$(basename "$dev")
    vendor=$(sed 's/^0x//' "$dev/vendor" 2>/dev/null || true)
    product=$(sed 's/^0x//' "$dev/device" 2>/dev/null || true)
    driver_link=$(readlink "$dev/driver" 2>/dev/null || true)
    driver=${driver_link##*/}
    group_link=$(readlink "$dev/iommu_group" 2>/dev/null || true)
    group=${group_link##*/}
    exclusive=0
    access=0
    if [ -n "$group" ]; then
        exclusive=1
        for member in "$SYSFS"/kernel/iommu_groups/"$group"/devices/*; do
            [ -e "$member" ] || continue
            if [ "$(basename "$member")" = "$bdf" ]; then continue; fi
            member_link=$(readlink "$member/driver" 2>/dev/null || true)
            member_driver=${member_link##*/}
            case "$member_driver" in
                '' | vfio-pci) ;;
                *) exclusive=0 ;;
            esac
        done
        if [ -r "/dev/vfio/$group" ] && [ -w "/dev/vfio/$group" ]; then access=1; fi
    fi
    pci_protection "$bdf" "$group" "$exclusive"
    warning=
    if [ "$(cat "$dev/boot_vga" 2>/dev/null || true)" = 1 ]; then warning=boot_vga; fi
    printf 'STACKBOX\tpci_candidate\t%s,%s,%s,%s,%s,%s,%s,%s,%s,%s\n' "$bdf" "$vendor" "$product" "$class" "$group" "$driver" "$exclusive" "$access" "$protected" "$warning"
done
# The stackbox-vfio-bind helper, installed root-owned and 0755, answers --check through the
# sudoers rule. The protocol it reports is the one the coordinator speaks to it.
helper=/usr/local/libexec/stackbox-vfio-bind
bind_helper=0
bind_helper_version=
if [ -x "$helper" ] && [ "$(stat -c '%u:%a' "$helper" 2>/dev/null || true)" = 0:755 ]; then
    helper_check=$(sudo -n "$helper" --check 2>/dev/null || true)
    case "$helper_check" in
        *"stackbox-vfio-bind protocol="*)
            helper_rest=${helper_check#*protocol=}
            helper_version=${helper_rest%%[!0-9]*}
            if [ -n "$helper_version" ]; then
                bind_helper=1
                bind_helper_version=$helper_version
            fi
            ;;
    esac
fi
field pci_bind_helper "$bind_helper"
field pci_bind_helper_version "$bind_helper_version"
