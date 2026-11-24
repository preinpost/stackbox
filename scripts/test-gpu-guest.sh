#!/bin/sh
# Runs the virtual GPU's nvidia-smi in busybox against a fake Nova metadata document.
# The metadata is read from a mounted file and from a busybox HTTP server, as it is in
# an instance. Every run also checks that stderr stayed empty: Telegraf reads stdout and
# stderr together and would fail on a stray diagnostic.
#
#   sh scripts/test-gpu-guest.sh
#
# Docker is required; no stackbox VM is touched.

set -eu

root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

command -v docker >/dev/null 2>&1 || {
    echo "docker is required to run the guest script tests" >&2
    exit 1
}

cat >"$work/meta_data.json" <<'JSON'
{
  "uuid": "11111111-2222-3333-4444-555555555555",
  "name": "gpu-test",
  "meta": {
    "stackbox_gpu_count": "2",
    "stackbox_gpu_0": "v1;p:sim-8g;q:constant;n:77;x:77;d:1;m:30;u:77;id:GPU-aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee",
    "stackbox_gpu_1": "v1;p:sim-16g;q:constant;n:77;x:77;d:1;m:50;u:77;id:GPU-bbbbbbbb-bbbb-cccc-dddd-eeeeeeeeeeee"
  }
}
JSON
cat >"$work/zero.json" <<'JSON'
{"uuid":"11111111-2222-3333-4444-555555555555","meta":{"stackbox_gpu_count":"0"}}
JSON

# run <label> <metadata file> [arguments...]
run() {
    label=$1
    metadata=$2
    shift 2
    if ! docker run --rm \
        -v "$root/guest/gpu:/gpu:ro" \
        -v "$work:/meta:ro" \
        -e "STACKBOX_GPU_METADATA_FILE=$metadata" \
        -e STACKBOX_GPU_CACHE_DIR=/tmp/stackbox-gpu-test \
        busybox:1.36 sh /gpu/nvidia-smi.sh "$@" \
        >"$work/out" 2>"$work/err"; then
        echo "FAIL: $label exited non-zero" >&2
        cat "$work/out" >&2
        exit 1
    fi
    if [ -s "$work/err" ]; then
        echo "FAIL: $label wrote to stderr" >&2
        cat "$work/err" >&2
        exit 1
    fi
}

expect() {
    label=$1
    pattern=$2
    file=$3
    if ! grep "$pattern" "$file" >/dev/null; then
        echo "FAIL: $label: missing '$pattern' in $file" >&2
        cat "$file" >&2
        exit 1
    fi
}

run "xml" /meta/meta_data.json -q -x
expect "xml doctype" 'nvsmi_device_v11.dtd' "$work/out"
expect "xml count" '<attached_gpus>2</attached_gpus>' "$work/out"
expect "xml first gpu" '<gpu id="0000:84:00.0">' "$work/out"
expect "xml second gpu" '<gpu id="0000:85:00.0">' "$work/out"
expect "xml utilization" '<gpu_util>77 %</gpu_util>' "$work/out"
expect "xml power draw" '<power_draw>118.9 W</power_draw>' "$work/out"
expect "xml power limit" '<power_limit>150.00 W</power_limit>' "$work/out"
expect "xml memory" '<total>8192 MiB</total>' "$work/out"
expect "xml uuid" 'GPU-aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee' "$work/out"
[ "$(grep -c '<gpu id=' "$work/out")" -eq 2 ] || {
    echo "FAIL: xml has the wrong number of <gpu> elements" >&2
    exit 1
}

run "csv" /meta/meta_data.json --query-gpu=index,name,uuid,utilization.gpu,memory.total,memory.used,temperature.gpu,power.draw,power.limit --format=csv
expect "csv header" 'utilization.gpu \[%\]' "$work/out"
expect "csv power header" 'power.draw \[W\]' "$work/out"
expect "csv first row" '^0, stackbox-virtual-gpu, GPU-aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee, 77 %, 8192 MiB, 2457 MiB, 72 C, 118.9 W, 150.00 W$' "$work/out"
expect "csv second row" '^1, stackbox-virtual-gpu, GPU-bbbbbbbb-bbbb-cccc-dddd-eeeeeeeeeeee, 77 %, 16384 MiB, 8192 MiB, 72 C, 198.2 W, 250.00 W$' "$work/out"

run "csv no units" /meta/meta_data.json --query-gpu=utilization.gpu,memory.used,power.draw --format=csv,noheader,nounits
expect "nounits value" '^77, 2457, 118.9$' "$work/out"
[ "$(wc -l <"$work/out" | tr -d ' ')" -eq 2 ] || {
    echo "FAIL: noheader printed more than one line per GPU" >&2
    exit 1
}

run "list" /meta/meta_data.json -L
expect "list first" '^GPU 0: stackbox-virtual-gpu (UUID: GPU-aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee)$' "$work/out"
expect "list second" '^GPU 1: stackbox-virtual-gpu (UUID: GPU-bbbbbbbb-bbbb-cccc-dddd-eeeeeeeeeeee)$' "$work/out"

run "version" /meta/meta_data.json --version
expect "version" 'Driver version           : 550.54.14' "$work/out"

run "text" /meta/meta_data.json -q
expect "text attached" 'Attached GPUs                             : 2' "$work/out"
expect "text draw" 'Power Draw                        : 118.9 W' "$work/out"

# No metadata at all: the guest's default is one virtual GPU at 0%, not an error.
docker run --rm \
    -v "$root/guest/gpu:/gpu:ro" \
    -e STACKBOX_GPU_METADATA_FILE=/nonexistent \
    -e STACKBOX_GPU_CACHE_DIR=/tmp/stackbox-gpu-test \
    -e STACKBOX_GPU_METADATA_URL=http://127.0.0.1:1/meta_data.json \
    busybox:1.36 sh /gpu/nvidia-smi.sh --query-gpu=index,utilization.gpu,memory.total,power.draw --format=csv \
    >"$work/out" 2>"$work/err"
[ ! -s "$work/err" ] || {
    echo "FAIL: the no-metadata run wrote to stderr" >&2
    cat "$work/err" >&2
    exit 1
}
expect "default row" '^0, 0 %, 8192 MiB, 15.0 W$' "$work/out"

# An explicit zero count is how a stackbox says this instance has no virtual GPU.
docker run --rm \
    -v "$root/guest/gpu:/gpu:ro" \
    -v "$work:/meta:ro" \
    -e STACKBOX_GPU_METADATA_FILE=/meta/zero.json \
    busybox:1.36 sh /gpu/nvidia-smi.sh -q -x >"$work/out" 2>"$work/err" || status=$?
[ "${status:-0}" -eq 6 ] || {
    echo "FAIL: a zero GPU count did not exit 6 (got ${status:-0})" >&2
    exit 1
}
expect "no devices" '^No devices were found$' "$work/out"
[ ! -s "$work/err" ] || {
    echo "FAIL: the no-devices run wrote to stderr" >&2
    exit 1
}
unset status

# The HTTP path an instance uses: busybox httpd serves the metadata document and the
# script's wget fallback fetches it. The cache stays inside the container.
docker run --rm \
    -v "$root/guest/gpu:/gpu:ro" \
    -v "$work:/meta:ro" \
    busybox:1.36 sh -c '
        httpd -f -p 18080 -h /meta >/dev/null 2>&1 &
        sleep 1
        STACKBOX_GPU_METADATA_URL=http://127.0.0.1:18080/meta_data.json \
        STACKBOX_GPU_CACHE_DIR=/tmp/stackbox-gpu-http \
        sh /gpu/nvidia-smi.sh --query-gpu=index,uuid,utilization.gpu --format=csv
    ' >"$work/out" 2>"$work/err"
[ ! -s "$work/err" ] || {
    echo "FAIL: the HTTP metadata run wrote to stderr" >&2
    cat "$work/err" >&2
    exit 1
}
expect "http first" '^0, GPU-aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee, 77 %$' "$work/out"
expect "http second" '^1, GPU-bbbbbbbb-bbbb-cccc-dddd-eeeeeeeeeeee, 77 %$' "$work/out"

echo "guest nvidia-smi OK: XML, CSV, list, version, defaults, no-devices, HTTP metadata"
