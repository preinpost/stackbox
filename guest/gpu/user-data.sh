#!/bin/sh
# farm virtual GPU installer (user-data for CirrOS and other images that do not
# read Nova vendor data). farm fills the marker below with guest/gpu/nvidia-smi.sh
# before serving this file; run it as root once the instance boots.
#
#   openstack server create ... --user-data farm-gpu-user-data.sh
#
# A machine with a real GPU (PCI vendor 10de) keeps its own driver and
# nvidia-smi: this installer leaves it alone.

if [ -e /dev/nvidia0 ] || [ -e /proc/driver/nvidia/version ]; then
    echo "farm-gpu: a real GPU is present; not installing the virtual GPU"
    exit 0
fi
if command -v lspci >/dev/null 2>&1; then
    pci=$(lspci -n 2>/dev/null || true)
    case "$pci" in
        *10de:*)
            echo "farm-gpu: a real GPU is present; not installing the virtual GPU"
            exit 0
            ;;
    esac
fi

mkdir -p /usr/local/bin 2>/dev/null || true
cat >/usr/bin/nvidia-smi <<'FARM_GPU_SCRIPT_EOF'
#__FARM_GPU_SCRIPT__
FARM_GPU_SCRIPT_EOF
chmod 0755 /usr/bin/nvidia-smi

# Show the first document on the serial console: the farm verification reads it
# there, and an operator sees the same values without logging in.
/usr/bin/nvidia-smi -q -x >/dev/console 2>&1 || true
echo "farm-gpu: installed /usr/bin/nvidia-smi (virtual GPU; values come from instance metadata)"
