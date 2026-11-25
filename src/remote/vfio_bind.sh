#!/bin/sh
# stackbox-vfio-bind: binds one host PCI device to vfio-pci for a stackbox, or gives it back to
# its host driver. Installed as /usr/local/libexec/stackbox-vfio-bind (root:root 0755) and run
# only through the sudoers rule that render_vfio_helper_install_script writes. Protocol 1.
#
#   stackbox-vfio-bind --check          prints `stackbox-vfio-bind protocol=1`
#   stackbox-vfio-bind --version        prints the protocol number
#   stackbox-vfio-bind bind <bdf>       binds <bdf> to vfio-pci
#   stackbox-vfio-bind unbind <bdf>     returns <bdf> to its host driver when vfio-pci holds it
#
# Each action prints `STACKBOX<TAB>pci_bind|pci_unbind<TAB><bdf><TAB><driver>[<TAB><group>]`.
set -eu
PATH=/usr/sbin:/usr/bin:/sbin:/bin
export PATH

PROTOCOL_VERSION=1
CONF=/etc/stackbox/vfio.conf
SYSFS=/sys
DEV_ROOT=/dev
TEST_MODE=0

# Test seams. Only a non-root caller with STACKBOX_TEST_SYSFS_ROOT set reads them: sudo's
# env_reset clears the environment for the installed rule, and root never reads them.
if [ "$(id -u)" -ne 0 ] && [ -n "${STACKBOX_TEST_SYSFS_ROOT:-}" ]; then
    TEST_MODE=1
    SYSFS=$STACKBOX_TEST_SYSFS_ROOT
    DEV_ROOT=${STACKBOX_TEST_DEV_ROOT:-$DEV_ROOT}
    CONF=${STACKBOX_TEST_VFIO_CONF:-$CONF}
    ROOT_DEV=${STACKBOX_TEST_ROOT_DEV:-}
    ROUTES=${STACKBOX_TEST_DEFAULT_ROUTE:-}
else
    ROOT_DEV=$(findmnt -rno MAJ:MIN / 2>/dev/null || true)
    ROUTES=$(ip route show default 2>/dev/null || true)
fi

die() {
    printf 'stackbox-vfio-bind: %s\n' "$*" >&2
    exit 1
}

@PCI_GUARD@

# load_vfio_group: the group whose /dev/vfio node the stackbox's VM opens: GROUP= in the config,
# else kvm. Refuses a name that is not a plain group name.
load_vfio_group() {
    VFIO_GROUP=kvm
    if [ -r "$CONF" ]; then
        configured=$(sed -n 's/^GROUP=//p' "$CONF" || true)
        if [ -n "$configured" ]; then VFIO_GROUP=$configured; fi
    fi
    case "$VFIO_GROUP" in
        '' | [!a-z_]* | *[!a-z0-9_-]*) die "GROUP in $CONF must match [a-z_][a-z0-9_-]*" ;;
    esac
}

# validate_bdf BDF: a PCI address DDDD:BB:DD.F in lowercase hex, with a device of at most 0x1f and
# a function of 0-7. Dies on anything else.
validate_bdf() {
    case "$1" in
        [0-9a-f][0-9a-f][0-9a-f][0-9a-f]:[0-9a-f][0-9a-f]:[0-9a-f][0-9a-f].[0-7]) ;;
        *) die "invalid PCI address '$1' (expected DDDD:BB:DD.F in lowercase hex)" ;;
    esac
    devhex=${1#*:}
    devhex=${devhex#*:}
    devhex=${devhex%.*}
    case "$devhex" in
        [01][0-9a-f]) ;;
        *) die "PCI device above 0x1f in '$1'" ;;
    esac
}

# The driver bound to device directory $1, empty when none.
driver_of() {
    driver_link=$(readlink "$1/driver" 2>/dev/null || true)
    driver=${driver_link##*/}
}

# The IOMMU group of device directory $1, empty when none.
group_of() {
    group_link=$(readlink "$1/iommu_group" 2>/dev/null || true)
    group=${group_link##*/}
}

# Refuses a group that holds a member another driver uses. The device itself may be bound.
check_group() {
    for member in "$SYSFS/kernel/iommu_groups/$2/devices"/*; do
        [ -e "$member" ] || continue
        member_bdf=${member##*/}
        if [ "$member_bdf" = "$1" ]; then continue; fi
        driver_of "$member"
        case "$driver" in
            '' | vfio-pci) ;;
            *) die "IOMMU group $2 is shared with $member_bdf (bound to $driver): shared_iommu_group; bind the whole group or leave $1 alone" ;;
        esac
    done
}

# Set once the device has left its host driver: a failure after that point hands it back.
restore_needed=
bdf=
dev=
restore_on_exit() {
    status=$?
    if [ "$status" -ne 0 ] && [ -n "$restore_needed" ] && [ -d "$dev" ]; then
        printf '\n' > "$dev/driver_override" 2>/dev/null || true
        printf '%s\n' "$bdf" > "$SYSFS/bus/pci/drivers_probe" 2>/dev/null || true
    fi
    exit "$status"
}
trap restore_on_exit EXIT

# check_device_safe: refuses a bridge, a device without a usable IOMMU group, a group shared with
# another driver, and anything the protection rules name. Expects bdf, dev set; sets group.
check_device_safe() {
    class=$(sed 's/^0x//' "$dev/class" 2>/dev/null || true)
    case "$class" in
        06*) die "$bdf is a PCI bridge; bridges are never passed through" ;;
    esac
    group_of "$dev"
    if [ -n "$group" ] && [ ! -d "$SYSFS/kernel/iommu_groups/$group" ]; then
        die "IOMMU group $group of $bdf is missing from sysfs"
    fi
    [ -n "$group" ] || die "$bdf has no IOMMU group: no_iommu_group; enable the IOMMU in firmware and on the kernel command line"
    check_group "$bdf" "$group"
    pci_protect_setup "$ROOT_DEV" "$ROUTES"
    pci_protection "$bdf" "$group" 1
    if [ -n "$protected" ]; then
        die "$bdf is protected ($protected) and cannot be passed through"
    fi
}

# ensure_vfio_group_access: hands the device's /dev/vfio/<group> node to the configured group.
# Expects group set.
ensure_vfio_group_access() {
    node="$DEV_ROOT/vfio/$group"
    [ -e "$node" ] || die "$node does not exist"
    chgrp "$VFIO_GROUP" "$node"
    chmod 660 "$node"
}

# do_bind BDF: checks the device, moves it from its host driver to vfio-pci and hands over the
# group node. Prints the STACKBOX result line last.
do_bind() {
    bdf=$1
    validate_bdf "$bdf"
    dev="$SYSFS/bus/pci/devices/$bdf"
    [ -d "$dev" ] || die "no PCI device $bdf"
    check_device_safe
    driver_of "$dev"
    if [ "$TEST_MODE" = 0 ]; then
        modprobe vfio-pci || die "cannot load vfio-pci"
    fi
    if [ "$driver" != vfio-pci ]; then
        if [ -n "$driver" ]; then
            restore_needed=1
            printf '%s\n' "$bdf" > "$dev/driver/unbind"
        fi
        printf 'vfio-pci\n' > "$dev/driver_override"
        restore_needed=1
        printf '%s\n' "$bdf" > "$SYSFS/bus/pci/drivers/vfio-pci/bind"
        driver_of "$dev"
        if [ "$TEST_MODE" = 0 ] && [ "$driver" != vfio-pci ]; then
            die "$bdf did not bind to vfio-pci"
        fi
    fi
    restore_needed=
    ensure_vfio_group_access
    printf 'STACKBOX\tpci_bind\t%s\tvfio-pci\t%s\n' "$bdf" "$group"
}

# do_unbind BDF: returns the device to its host driver when vfio-pci holds it. Prints the
# STACKBOX result line with the driver the device now has, or none.
do_unbind() {
    bdf=$1
    validate_bdf "$bdf"
    dev="$SYSFS/bus/pci/devices/$bdf"
    [ -d "$dev" ] || die "no PCI device $bdf"
    driver_of "$dev"
    if [ "$driver" = vfio-pci ]; then
        printf '%s\n' "$bdf" > "$SYSFS/bus/pci/drivers/vfio-pci/unbind"
        printf '\n' > "$dev/driver_override"
        printf '%s\n' "$bdf" > "$SYSFS/bus/pci/drivers_probe"
        driver_of "$dev"
    fi
    printf 'STACKBOX\tpci_unbind\t%s\t%s\n' "$bdf" "${driver:-none}"
}

# main ARGS...: the verb dispatch. Every verb checks its own argument count.
main() {
    case "${1:-}" in
        --check)
            [ "$#" -eq 1 ] || die "usage: stackbox-vfio-bind --check"
            echo "stackbox-vfio-bind protocol=$PROTOCOL_VERSION"
            ;;
        --version)
            [ "$#" -eq 1 ] || die "usage: stackbox-vfio-bind --version"
            echo "$PROTOCOL_VERSION"
            ;;
        bind)
            [ "$#" -eq 2 ] || die "usage: stackbox-vfio-bind bind <bdf>"
            do_bind "$2"
            ;;
        unbind)
            [ "$#" -eq 2 ] || die "usage: stackbox-vfio-bind unbind <bdf>"
            do_unbind "$2"
            ;;
        *)
            die "usage: stackbox-vfio-bind --check | --version | bind <bdf> | unbind <bdf>"
            ;;
    esac
}

# The group is checked on every run, --check included, so a bad config fails loudly.
load_vfio_group
main "$@"
