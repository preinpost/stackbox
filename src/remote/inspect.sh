set -eu
export PATH="$HOME/.microsandbox/bin:$HOME/.local/bin:$PATH"
export MSB_BACKEND=local
field() { printf 'FARM\t%s\t%s\n' "$1" "$2"; }
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
# A kernel host preparation installed under ~/.local/share/farm/libkrunfw and this host can
# boot: its manifest names this architecture and the msb the host runs. An installation that
# cannot be used is reported as the reason instead, so the coordinator can show it.
kernel=
kernel_note=
if [ -d "$HOME/.local/share/farm/libkrunfw" ]; then
    host_arch=$(uname -m)
    for dir in "$HOME/.local/share/farm/libkrunfw"/*; do
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
# PCI passthrough: list only the devices bound to vfio-pci, the only ones a farm may pass through.
# A device is ready when its IOMMU group has no member bound to another driver and the group's
# /dev/vfio file is readable and writable by this user. The groups are read here, not by the
# coordinator, so the answer is the host's own.
field vfio "$(if [ -e /dev/vfio/vfio ]; then echo 1; else echo 0; fi)"
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
    printf 'FARM\tpci_device\t%s,%s,%s,%s,%s,%s,%s,%s\n' "$bdf" "$vendor" "$product" "$class" "$group" "$driver" "$exclusive" "$ready"
done
