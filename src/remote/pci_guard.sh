# Shared by inspect.sh and stackbox-vfio-bind: which PCI controllers carry the host's root disk,
# its default route and its physical USB devices. The caller sets SYSFS (the sysfs root) and
# calls pci_protect_setup once; pci_protection then answers for each device.
# Every command substitution reads its whole output: nothing here stops reading a pipe early.

# Appends the PCI address on the resolved path $1 to pci_found: the last one, which is the
# controller a device hangs off. Nothing is appended when the path has no PCI address.
pci_note_controller() {
    pci_last=
    pci_rest=$1
    while [ -n "$pci_rest" ]; do
        pci_part=${pci_rest%%/*}
        case "$pci_rest" in
            */*) pci_rest=${pci_rest#*/} ;;
            *) pci_rest= ;;
        esac
        case "$pci_part" in
            [0-9a-f][0-9a-f][0-9a-f][0-9a-f]:[0-9a-f][0-9a-f]:[0-9a-f][0-9a-f].[0-7]) pci_last=$pci_part ;;
        esac
    done
    if [ -n "$pci_last" ]; then pci_found="$pci_found $pci_last"; fi
}

# Adds the controllers under block device $1 (a sysfs path), following device-mapper and LVM
# slaves down to the disks under them.
pci_block_controllers() {
    pci_real=$(readlink -f "$1" 2>/dev/null || true)
    [ -n "$pci_real" ] || return 0
    pci_note_controller "$pci_real"
    for pci_slave in "$pci_real"/slaves/*; do
        [ -e "$pci_slave" ] || continue
        pci_block_controllers "$pci_slave"
    done
}

# Adds the controllers of network device $1 (a sysfs path), following bonding lower devices and
# bridge ports, so a physical NIC under a bridge that carries the default route is protected.
pci_net_controllers() {
    pci_real=$(readlink -f "$1" 2>/dev/null || true)
    [ -n "$pci_real" ] || return 0
    pci_note_controller "$pci_real"
    for pci_lower in "$pci_real"/lower_* "$pci_real"/brif/*; do
        [ -e "$pci_lower" ] || continue
        pci_net_controllers "$pci_lower"
    done
}

# The interface of the default route in the text $1 (`ip route show default`), if any.
pci_default_nic() {
    pci_nic=
    pci_take=0
    for pci_token in $1; do
        if [ "$pci_take" = 1 ]; then
            pci_nic=$pci_token
            break
        fi
        if [ "$pci_token" = dev ]; then pci_take=1; fi
    done
}

# pci_protect_setup ROOT_DEV ROUTES: ROOT_DEV is the root filesystem's MAJ:MIN (empty when it
# cannot be found) and ROUTES the default-route text. Whitespace is stripped from ROOT_DEV first,
# since findmnt pads its column; a value that is not exactly MAJ:MIN counts as unknown. Sets
# root_known (0 when the root disk cannot be resolved, which protects every device), and the
# controller lists root_ctrls, nic_ctrls and usb_ctrls.
pci_protect_setup() {
    root_known=0
    root_ctrls=
    nic_ctrls=
    usb_ctrls=
    pci_root_dev=$(printf '%s' "$1" | tr -d '[:space:]')
    case "$pci_root_dev" in
        '' | *[!0-9:]* | :* | *: | *:*:*) pci_root_dev= ;;
        *:*) ;;
        *) pci_root_dev= ;;
    esac
    if [ -n "$pci_root_dev" ] && [ -e "$SYSFS/dev/block/$pci_root_dev" ]; then
        root_known=1
        pci_found=
        pci_block_controllers "$SYSFS/dev/block/$pci_root_dev"
        root_ctrls=$pci_found
    fi
    pci_default_nic "$2"
    if [ -n "$pci_nic" ]; then
        case "$pci_nic" in
            */* | .* | '') ;;
            *)
                pci_found=
                pci_net_controllers "$SYSFS/class/net/$pci_nic"
                nic_ctrls=$pci_found
                ;;
        esac
    fi
    pci_found=
    for pci_dev in "$SYSFS"/bus/usb/devices/*; do
        [ -e "$pci_dev" ] || continue
        pci_name=${pci_dev##*/}
        case "$pci_name" in
            usb*) continue ;;
        esac
        pci_real=$(readlink -f "$pci_dev" 2>/dev/null || true)
        pci_note_controller "$pci_real"
    done
    usb_ctrls=$pci_found
}

# pci_protection BDF GROUP EXCLUSIVE: sets protected to why BDF must not be passed through, or
# to nothing. GROUP is empty for a device without an IOMMU group; EXCLUSIVE is 1 when no other
# member of its group is bound to another driver.
pci_protection() {
    protected=
    if [ -z "$2" ]; then
        protected=no_iommu_group
    elif [ "$3" != 1 ]; then
        protected=shared_iommu_group
    elif [ "$root_known" = 0 ]; then
        protected=root_disk
    else
        case " $root_ctrls " in
            *" $1 "*) protected=root_disk ;;
        esac
        if [ -z "$protected" ]; then
            case " $nic_ctrls " in
                *" $1 "*) protected=default_route_nic ;;
            esac
        fi
        if [ -z "$protected" ]; then
            case " $usb_ctrls " in
                *" $1 "*) protected=usb_in_use ;;
            esac
        fi
    fi
}
