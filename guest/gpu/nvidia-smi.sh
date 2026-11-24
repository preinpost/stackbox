#!/bin/sh
# stackbox virtual GPU: a stand-in for `nvidia-smi` on an instance that has no GPU.
#
# stackbox writes the load into Nova instance metadata as `stackbox_gpu_count` and
# `stackbox_gpu_<i>` (`v1;p:sim-8g;q:sine;n:10;x:90;d:60;m:30;u:...;id:GPU-...`).
# The values change slowly (Nova caches metadata for about 15 seconds), so this
# script reads the metadata document at most once every STACKBOX_GPU_CACHE_TTL
# seconds and keeps the last document in /run/stackbox-gpu when a read fails.
#
# Supported:
#   nvidia-smi -q -x                  XML, schema v11 (Telegraf inputs.nvidia_smi)
#   nvidia-smi -q                     short text report
#   nvidia-smi --query-gpu=... --format=csv[,noheader][,nounits]
#   nvidia-smi -L                     one line per virtual GPU
#   nvidia-smi --version
#   nvidia-smi                        short summary
#
# Every diagnostic goes to /dev/null: Telegraf reads stdout and stderr together
# and would fail on anything beside the document. No real GPU is touched, and
# nothing computes on these values.

exec 2>/dev/null

META_URL=${STACKBOX_GPU_METADATA_URL:-http://169.254.169.254/openstack/latest/meta_data.json}
# A file overrides the metadata service, for tests and for images without curl/wget.
META_FILE=${STACKBOX_GPU_METADATA_FILE:-}
CACHE_DIR=${STACKBOX_GPU_CACHE_DIR:-/run/stackbox-gpu}
CACHE_TTL=${STACKBOX_GPU_CACHE_TTL:-2}
DRIVER=550.54.14
CUDA=12.4
PRODUCT=stackbox-virtual-gpu

# ---------------------------------------------------------------------------
# metadata

fetch_doc() {
    if [ -n "$META_FILE" ] && [ -r "$META_FILE" ]; then
        cat "$META_FILE"
        return 0
    fi
    now=$(date +%s)
    case "$now" in '' | *[!0-9]*) now=0 ;; esac
    cache="$CACHE_DIR/meta.json"
    if [ -f "$cache" ]; then
        stamp=$(sed -n '1p' "$cache")
        case "$stamp" in '' | *[!0-9]*) stamp=0 ;; esac
        age=$((now - stamp))
        if [ "$age" -ge 0 ] && [ "$age" -lt "$CACHE_TTL" ]; then
            sed -n '2,$p' "$cache"
            return 0
        fi
    fi
    doc=
    tries=0
    while [ "$tries" -lt 2 ]; do
        if command -v curl >/dev/null 2>&1; then
            doc=$(curl -fsS --max-time 2 "$META_URL")
        elif command -v wget >/dev/null 2>&1; then
            doc=$(wget -q -O - -T 2 "$META_URL")
        fi
        [ -n "$doc" ] && break
        tries=$((tries + 1))
    done
    if [ -z "$doc" ]; then
        # The last document beats the built-in defaults while Nova is unreachable.
        [ -f "$cache" ] && sed -n '2,$p' "$cache"
        return 0
    fi
    mkdir -p "$CACHE_DIR"
    if [ -d "$CACHE_DIR" ]; then
        printf '%s\n%s\n' "$now" "$doc" >"$CACHE_DIR/meta.$$" && mv "$CACHE_DIR/meta.$$" "$cache"
    fi
    printf '%s' "$doc"
    return 0
}

doc=$(fetch_doc)
# Nova pretty-prints the document; one line lets each key be read with one sed
# expression. Values stackbox writes never contain quotes or newlines.
flat=$(printf '%s' "$doc" | tr '\n\r\t' '   ')

# The string value of a metadata key, or nothing.
meta_str() {
    printf '%s' "$flat" | sed -n 's/.*"'"$1"'"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p'
}

# The number value of a metadata key, or nothing. Nova usually quotes it.
meta_num() {
    value=$(printf '%s' "$flat" | sed -n 's/.*"'"$1"'"[[:space:]]*:[[:space:]]*"\([0-9][0-9]*\)".*/\1/p')
    [ -n "$value" ] || value=$(printf '%s' "$flat" | sed -n 's/.*"'"$1"'"[[:space:]]*:[[:space:]]*\([0-9][0-9]*\).*/\1/p')
    printf '%s' "$value"
}

# `key:value` inside a stackbox_gpu_<i> value.
field() {
    printf '%s' "$1" | sed -n 's/.*[;]'"$2"':\([^;]*\).*/\1/p'
}

# The 0-based sample `_want` of a comma-separated list.
sample_at() {
    _list=$1
    _want=$2
    _at=0
    _old=$IFS
    IFS=,
    for _sample in $_list; do
        if [ "$_at" -eq "$_want" ]; then
            IFS=$_old
            printf '%s' "$_sample"
            return 0
        fi
        _at=$((_at + 1))
    done
    IFS=$_old
    printf '%s' "$_list" | sed 's/,.*//'
}

instance_uuid=$(meta_str uuid)
[ -n "$instance_uuid" ] || instance_uuid=00000000-0000-0000-0000-000000000000

count_raw=$(meta_num stackbox_gpu_count)
case "$count_raw" in '' | *[!0-9]*) count_raw= ;; esac
if [ "$count_raw" = 0 ]; then
    printf 'No devices were found\n'
    exit 6
fi
if [ -n "$count_raw" ]; then
    count=$count_raw
else
    count=0
    i=0
    while [ "$i" -lt 24 ]; do
        [ -n "$(meta_str "stackbox_gpu_$i")" ] && count=$((i + 1))
        i=$((i + 1))
    done
    [ "$count" -gt 0 ] || count=1
fi
[ "$count" -le 24 ] || count=24

# ---------------------------------------------------------------------------
# one GPU's values, derived from its utilization sample with integer arithmetic

load_gpu() {
    g_index=$1
    g_raw=$(meta_str "stackbox_gpu_$g_index")
    g_profile=$(field "$g_raw" p)
    case "$g_profile" in
        sim-8g | sim-16g | sim-40g | sim-80g) ;;
        *) g_profile=sim-8g ;;
    esac
    g_period=$(field "$g_raw" d)
    case "$g_period" in '' | *[!0-9]*) g_period=60 ;; esac
    [ "$g_period" -gt 0 ] || g_period=60
    g_ratio=$(field "$g_raw" m)
    case "$g_ratio" in '' | *[!0-9]*) g_ratio=30 ;; esac
    [ "$g_ratio" -le 100 ] || g_ratio=100
    g_samples=$(field "$g_raw" u)
    [ -n "$g_samples" ] || g_samples=0
    g_uuid=$(field "$g_raw" id)
    [ -n "$g_uuid" ] || g_uuid="GPU-$instance_uuid"
    case "$g_profile" in
        sim-8g)
            g_mem_total=8192
            g_power_limit=150
            g_sm_base=300
            g_sm_max=1500
            g_mem_clock=7000
            ;;
        sim-16g)
            g_mem_total=16384
            g_power_limit=250
            g_sm_base=300
            g_sm_max=1800
            g_mem_clock=8000
            ;;
        sim-40g)
            g_mem_total=40960
            g_power_limit=350
            g_sm_base=300
            g_sm_max=1950
            g_mem_clock=9000
            ;;
        sim-80g)
            g_mem_total=81920
            g_power_limit=400
            g_sm_base=300
            g_sm_max=2000
            g_mem_clock=10000
            ;;
    esac
    g_n=0
    g_old=$IFS
    IFS=,
    for _ in $g_samples; do
        g_n=$((g_n + 1))
    done
    IFS=$g_old
    [ "$g_n" -gt 0 ] || {
        g_samples=0
        g_n=1
    }
    g_epoch=$(date +%s)
    case "$g_epoch" in '' | *[!0-9]*) g_epoch=0 ;; esac
    g_index_in_period=$((g_epoch % g_period))
    g_at=$((g_index_in_period * g_n / g_period))
    g_util=$(sample_at "$g_samples" "$g_at")
    case "$g_util" in '' | *[!0-9]*) g_util=0 ;; esac
    [ "$g_util" -le 100 ] || g_util=100
    g_mem_used=$((g_mem_total * g_ratio / 100))
    g_mem_free=$((g_mem_total - g_mem_used))
    g_mem_util=$g_ratio
    g_temp=$((30 + 55 * g_util / 100))
    g_fan=$((20 + 80 * g_util / 100))
    g_sm=$((g_sm_base + (g_sm_max - g_sm_base) * g_util / 100))
    g_video=$((g_sm_base + (g_sm_max - g_sm_base) * g_util / 120))
    # At least 10% of the limit, and the rest follows utilization; tenths keep
    # one decimal place in every format, as the real tool reports it.
    g_draw10=$((g_power_limit * (100 + 9 * g_util) / 100))
    g_pstate=P8
    [ "$g_util" -ge 10 ] && g_pstate=P2
    [ "$g_util" -ge 50 ] && g_pstate=P0
    g_bus=$(printf '0000:%02x:00.0' $((0x84 + g_index)))
    g_bus8=$(printf '00000000:%02x:00.0' $((0x84 + g_index)))
    g_pci_bus=$(printf '0x%02x' $((0x84 + g_index)))
}

# ---------------------------------------------------------------------------
# output helpers

unit_of() {
    case "$1" in
        utilization.gpu | utilization.memory | utilization.encoder | utilization.decoder | fan.speed)
            printf '%%'
            ;;
        memory.total | memory.used | memory.free)
            printf 'MiB'
            ;;
        temperature.gpu)
            printf 'C'
            ;;
        power.draw | power.limit)
            printf 'W'
            ;;
        clocks.sm | clocks.gr | clocks.mem | clocks.video | clocks.current.sm | clocks.current.graphics | clocks.current.memory | clocks.current.video)
            printf 'MHz'
            ;;
        *) printf '' ;;
    esac
}

value_of() {
    case "$1" in
        index) printf '%s' "$g_index" ;;
        name) printf '%s' "$PRODUCT" ;;
        uuid) printf '%s' "$g_uuid" ;;
        serial) printf '0000000000000' ;;
        driver_version) printf '%s' "$DRIVER" ;;
        cuda_version) printf '%s' "$CUDA" ;;
        vbios_version) printf '0.0' ;;
        pstate) printf '%s' "$g_pstate" ;;
        compute_mode) printf 'Default' ;;
        persistence_mode) printf 'Disabled' ;;
        display_mode | display_active) printf 'Disabled' ;;
        ecc.mode.current) printf 'N/A' ;;
        gpu_bus_id | pci.bus_id) printf '%s' "$g_bus8" ;;
        utilization.gpu) printf '%s' "$g_util" ;;
        utilization.memory) printf '%s' "$g_mem_util" ;;
        utilization.encoder | utilization.decoder) printf '0' ;;
        memory.total) printf '%s' "$g_mem_total" ;;
        memory.used) printf '%s' "$g_mem_used" ;;
        memory.free) printf '%s' "$g_mem_free" ;;
        temperature.gpu) printf '%s' "$g_temp" ;;
        power.draw) printf '%s.%s' $((g_draw10 / 10)) $((g_draw10 % 10)) ;;
        power.limit) printf '%s.00' "$g_power_limit" ;;
        fan.speed) printf '%s' "$g_fan" ;;
        clocks.sm | clocks.gr | clocks.current.sm | clocks.current.graphics) printf '%s' "$g_sm" ;;
        clocks.mem | clocks.current.memory) printf '%s' "$g_mem_clock" ;;
        clocks.video | clocks.current.video) printf '%s' "$g_video" ;;
        pcie.link.gen.current) printf '4' ;;
        pcie.link.width.current) printf '16' ;;
        *) printf 'N/A' ;;
    esac
}

xml_gpu() {
    load_gpu "$1"
    cat <<XML
        <gpu id="$g_bus">
                <product_name>$PRODUCT</product_name>
                <product_brand>stackbox</product_brand>
                <serial>0000000000000</serial>
                <uuid>$g_uuid</uuid>
                <vbios_version>0.0</vbios_version>
                <fan_speed>$g_fan %</fan_speed>
                <performance_state>$g_pstate</performance_state>
                <compute_mode>Default</compute_mode>
                <display_mode>Disabled</display_mode>
                <display_active>Disabled</display_active>
                <persistence_mode>Disabled</persistence_mode>
                <ecc_mode>
                        <current_ecc>N/A</current_ecc>
                </ecc_mode>
                <utilization>
                        <gpu_util>$g_util %</gpu_util>
                        <memory_util>$g_mem_util %</memory_util>
                        <encoder_util>0 %</encoder_util>
                        <decoder_util>0 %</decoder_util>
                </utilization>
                <temperature>
                        <gpu_temp>$g_temp C</gpu_temp>
                </temperature>
                <power_readings>
                        <power_draw>$((g_draw10 / 10)).$((g_draw10 % 10)) W</power_draw>
                        <power_limit>$g_power_limit.00 W</power_limit>
                </power_readings>
                <fb_memory_usage>
                        <total>$g_mem_total MiB</total>
                        <used>$g_mem_used MiB</used>
                        <free>$g_mem_free MiB</free>
                </fb_memory_usage>
                <clocks>
                        <graphics_clock>$g_sm MHz</graphics_clock>
                        <sm_clock>$g_sm MHz</sm_clock>
                        <mem_clock>$g_mem_clock MHz</mem_clock>
                        <video_clock>$g_video MHz</video_clock>
                </clocks>
                <pci>
                        <pci_bus>$g_pci_bus</pci_bus>
                        <pci_gpu_link_info>
                                <pcie_gen>
                                        <current_link_gen>4</current_link_gen>
                                </pcie_gen>
                                <link_widths>
                                        <current_link_width>16</current_link_width>
                                </link_widths>
                        </pci_gpu_link_info>
                </pci>
                <clocks_throttle_reasons>
                        <clocks_throttle_reason_sw_power_cap>Not Active</clocks_throttle_reason_sw_power_cap>
                        <clocks_throttle_reason_sw_thermal_slowdown>Not Active</clocks_throttle_reason_sw_thermal_slowdown>
                        <clocks_throttle_reason_hw_slowdown>Not Active</clocks_throttle_reason_hw_slowdown>
                        <clocks_throttle_reason_gpu_idle>Not Active</clocks_throttle_reason_gpu_idle>
                </clocks_throttle_reasons>
        </gpu>
XML
}

xml_output() {
    printf '<?xml version="1.0" ?>\n'
    printf '<!DOCTYPE nvidia_smi_log SYSTEM "nvsmi_device_v11.dtd">\n'
    printf '<nvidia_smi_log>\n'
    printf '        <timestamp>%s</timestamp>\n' "$(date '+%a %b %d %H:%M:%S %Y')"
    printf '        <driver_version>%s</driver_version>\n' "$DRIVER"
    printf '        <cuda_version>%s</cuda_version>\n' "$CUDA"
    printf '        <attached_gpus>%s</attached_gpus>\n' "$count"
    i=0
    while [ "$i" -lt "$count" ]; do
        xml_gpu "$i"
        i=$((i + 1))
    done
    printf '</nvidia_smi_log>\n'
}

text_gpu() {
    load_gpu "$1"
    printf '\nGPU %s\n' "$g_bus8"
    printf '    Product Name                          : %s\n' "$PRODUCT"
    printf '    GPU UUID                              : %s\n' "$g_uuid"
    printf '    Fan Speed                             : %s %%\n' "$g_fan"
    printf '    Performance State                     : %s\n' "$g_pstate"
    printf '    FB Memory Usage\n'
    printf '        Total                             : %s MiB\n' "$g_mem_total"
    printf '        Used                              : %s MiB\n' "$g_mem_used"
    printf '        Free                              : %s MiB\n' "$g_mem_free"
    printf '    Utilization\n'
    printf '        Gpu                               : %s %%\n' "$g_util"
    printf '        Memory                            : %s %%\n' "$g_mem_util"
    printf '    Temperature\n'
    printf '        GPU Current Temp                  : %s C\n' "$g_temp"
    printf '    Power Readings\n'
    printf '        Power Draw                        : %s.%s W\n' $((g_draw10 / 10)) $((g_draw10 % 10))
    printf '        Power Limit                       : %s.00 W\n' "$g_power_limit"
    printf '    Clocks\n'
    printf '        Graphics                          : %s MHz\n' "$g_sm"
    printf '        SM                                : %s MHz\n' "$g_sm"
    printf '        Memory                            : %s MHz\n' "$g_mem_clock"
}

text_output() {
    printf '==============NVSMI LOG==============\n\n'
    printf 'Timestamp                                 : %s\n' "$(date '+%a %b %d %H:%M:%S %Y')"
    printf 'Driver Version                            : %s\n' "$DRIVER"
    printf 'CUDA Version                              : %s\n' "$CUDA"
    printf '\nAttached GPUs                             : %s\n' "$count"
    i=0
    while [ "$i" -lt "$count" ]; do
        text_gpu "$i"
        i=$((i + 1))
    done
}

csv_output() {
    query=$(printf '%s' "$1" | tr -d ' ')
    format=$(printf '%s' "$format" | tr -d ' ')
    noheader=0
    nounits=0
    old=$IFS
    IFS=,
    for token in $format; do
        [ "$token" = noheader ] && noheader=1
        [ "$token" = nounits ] && nounits=1
    done
    IFS=$old
    if [ "$noheader" -eq 0 ]; then
        line=
        old=$IFS
        IFS=,
        for field_name in $query; do
            unit=$(unit_of "$field_name")
            if [ "$nounits" -eq 1 ]; then
                unit=
            fi
            if [ -z "$line" ]; then
                line=$field_name
            else
                line="$line, $field_name"
            fi
            [ -n "$unit" ] && line="$line [$unit]"
        done
        IFS=$old
        printf '%s\n' "$line"
    fi
    i=0
    while [ "$i" -lt "$count" ]; do
        load_gpu "$i"
        line=
        old=$IFS
        IFS=,
        for field_name in $query; do
            value=$(value_of "$field_name")
            unit=$(unit_of "$field_name")
            if [ "$nounits" -eq 1 ]; then
                unit=
            fi
            if [ -n "$unit" ]; then
                value="$value $unit"
            fi
            if [ -z "$line" ]; then
                line=$value
            else
                line="$line, $value"
            fi
        done
        IFS=$old
        printf '%s\n' "$line"
        i=$((i + 1))
    done
}

list_output() {
    i=0
    while [ "$i" -lt "$count" ]; do
        load_gpu "$i"
        printf 'GPU %s: %s (UUID: %s)\n' "$i" "$PRODUCT" "$g_uuid"
        i=$((i + 1))
    done
}

summary_output() {
    printf '%s: %s virtual GPU(s); values come from instance metadata\n' "$PRODUCT" "$count"
    i=0
    while [ "$i" -lt "$count" ]; do
        load_gpu "$i"
        printf 'GPU %s  %s  %s  %s %%  %s/%s MiB  %s C  %s.%s W\n' \
            "$i" "$PRODUCT" "$g_bus8" "$g_util" "$g_mem_used" "$g_mem_total" "$g_temp" \
            $((g_draw10 / 10)) $((g_draw10 % 10))
        i=$((i + 1))
    done
}

usage() {
    cat <<'USAGE'
Usage: nvidia-smi [options]
  -q, --query              short text report
  -q -x                    XML report (schema v11)
  --query-gpu=FIELDS       comma-separated fields, with --format=csv[,noheader][,nounits]
  -L, --list-gpus          one line per virtual GPU
  --version                version information
  -h, --help               this help

stackbox virtual GPU: the values come from the instance's Nova metadata and are
controlled from the stackbox portal. This is not a real GPU.
USAGE
}

# ---------------------------------------------------------------------------
# arguments

query=
format=csv
mode=
xml=0
skip=0
unsupported=0
for arg in "$@"; do
    if [ "$skip" -eq 1 ]; then
        skip=0
        continue
    fi
    case "$arg" in
        --query-gpu=*) query=${arg#--query-gpu=} ;;
        --format=*) format=${arg#--format=} ;;
        -q | --query) mode=q ;;
        -x | --xml) xml=1 ;;
        -L | --list-gpus) mode=list ;;
        --version) mode=version ;;
        -h | --help) mode=help ;;
        -i | --id) skip=1 ;;
        --id=* | --filename=* | --gpu-id=*) : ;;
        *) unsupported=1 ;;
    esac
done

if [ "$unsupported" -eq 1 ] && [ -z "$query" ] && [ "$xml" -eq 0 ] && [ -z "$mode" ]; then
    exit 1
fi

if [ -n "$query" ]; then
    csv_output "$query"
elif [ "$xml" -eq 1 ]; then
    xml_output
elif [ "$mode" = list ]; then
    list_output
elif [ "$mode" = version ]; then
    printf 'stackbox-virtual-gpu version : %s\n' "$DRIVER"
    printf 'Driver version           : %s\n' "$DRIVER"
    printf 'Compute API version      : %s\n' "$CUDA"
elif [ "$mode" = help ]; then
    usage
elif [ "$mode" = q ]; then
    text_output
else
    summary_output
fi
exit 0
