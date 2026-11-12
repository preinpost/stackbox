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
