set -eu
# A umask such as 0002 would leave ~/.microsandbox group-writable, which msb refuses.
umask 022
export PATH="$HOME/.microsandbox/bin:$HOME/.local/bin:$PATH"
command -v curl >/dev/null
command -v tar >/dev/null
case "$(uname -s)-$(uname -m)" in
    Linux-aarch64 | Linux-x86_64) os=linux arch=$(uname -m) ;;
    Darwin-arm64) os=darwin arch=aarch64 ;;
    *) exit 1 ;;
esac
if [ "$os" = linux ]; then
    command -v sha256sum >/dev/null
    command -v flock >/dev/null
    mkdir -p "$HOME/.local/share/stackbox"
    exec 9>"$HOME/.local/share/stackbox/bootstrap.lock"
    flock -n 9
fi
# macOS has no flock(1); the stackbox-agent that runs this there holds the install lock.
want=@MSB_VERSION@
# msb 0.8.1 refuses a catalog under a group- or world-writable ancestor. Earlier installs
# could leave ~/.microsandbox that way, so fix it even when msb itself is already current.
if [ -d "$HOME/.microsandbox" ]; then
    chmod go-w "$HOME/.microsandbox"
fi
installed() { msb --version 2>/dev/null | awk '{ print $NF }'; }
# Whether this host had an msb before this script ran. Only an msb installed fresh by stackbox is
# recorded as ours, so unregistering with remote files removes it and never one the operator had.
had_msb=yes
command -v msb >/dev/null || had_msb=no
record_install() {
    [ "$had_msb" = no ] || return 0
    marker="$HOME/.local/share/stackbox/msb-install.json"
    [ -e "$marker" ] && return 0
    mkdir -p "$HOME/.local/share/stackbox"
    printf '{"installed_by":"stackbox","had_msb":false,"version":"%s","installed_at":"%s"}\n' \
        "$want" "$(date -u +%Y-%m-%dT%H:%M:%SZ)" >"$marker.tmp"
    mv "$marker.tmp" "$marker"
}
# STACKBOX_MSB_BIN names a custom msb build (a fork), installed as it is: no release is downloaded
# and the release is never fetched over it. It must report the version this build expects.
if [ -n "${STACKBOX_MSB_BIN:-}" ]; then
    [ -f "$STACKBOX_MSB_BIN" ] && [ -x "$STACKBOX_MSB_BIN" ] || {
        echo "stackbox: STACKBOX_MSB_BIN $STACKBOX_MSB_BIN is not an executable file" >&2
        exit 1
    }
    mkdir -p "$HOME/.microsandbox/bin"
    install -m 755 "$STACKBOX_MSB_BIN" "$HOME/.microsandbox/bin/msb"
    [ "$(installed)" = "$want" ] || {
        echo "stackbox: STACKBOX_MSB_BIN reports msb '$(installed)', expected $want" >&2
        exit 1
    }
    record_install
    exit 0
fi
# An older msb is upgraded in place, as msb's own installer does: running sandboxes keep the
# runtime they started with. A newer one would need a state downgrade, so it is left alone.
if command -v msb >/dev/null; then
    have=$(installed)
    [ "$have" = "$want" ] && exit 0
    [ "$(printf '%s\n%s\n' "$have" "$want" | sort -V | head -n 1)" = "$have" ]
fi
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
cd "$work"
bundle="microsandbox-$os-$arch.tar.gz"
# STACKBOX_MSB_RELEASE_BASE overrides where the release bundle and its checksums come from; the
# default is the preinpost/microsandbox fork's release of the same version.
base=${STACKBOX_MSB_RELEASE_BASE:-https://github.com/preinpost/microsandbox/releases/download/v$want}
curl --fail --silent --show-error --location --proto '=https' --tlsv1.2 --max-time 300 "$base/$bundle" -o "$bundle"
curl --fail --silent --show-error --location --proto '=https' --tlsv1.2 --max-time 60 "$base/checksums.sha256" -o checksums.sha256
awk -v file="$bundle" '$2 == file || $2 == "*" file { print; found++ } END { if (found != 1) exit 1 }' checksums.sha256 > selected.sha256
if [ "$os" = linux ]; then
    sha256sum --check selected.sha256
else
    shasum -a 256 --check selected.sha256
fi
tar -xzf "$bundle"
mkdir -p "$HOME/.microsandbox/bin" "$HOME/.microsandbox/lib"
lib="$HOME/.microsandbox/lib"
if [ "$os" = linux ]; then
    set -- libkrunfw.so.*.*.*
    [ "$#" -eq 1 ] && [ -f "$1" ]
    library=$1
    abi=${library#libkrunfw.so.}
    abi=${abi%%.*}
    install -m 755 msb "$HOME/.microsandbox/bin/msb"
    install -m 644 "$library" "$lib/$library"
    ln -sf "$library" "$lib/libkrunfw.so.$abi"
    ln -sf "libkrunfw.so.$abi" "$lib/libkrunfw.so"
else
    set -- libkrunfw.*.dylib
    [ "$#" -eq 1 ] && [ -f "$1" ]
    library=$1
    install -m 755 msb "$HOME/.microsandbox/bin/msb"
    # macOS caches code signatures per vnode, so a loaded library is replaced, never rewritten.
    cp "$library" "$lib/$library.tmp"
    mv "$lib/$library.tmp" "$lib/$library"
    ln -sf "$library" "$lib/libkrunfw.dylib"
fi
[ "$(installed)" = "$want" ]
record_install
