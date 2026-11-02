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
if timeout 15 msb snapshot inspect '@GOLDEN@' >/dev/null 2>&1; then field golden 1; else field golden 0; fi
