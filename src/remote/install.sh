set -eu
export PATH="$HOME/.microsandbox/bin:$HOME/.local/bin:$PATH"
command -v curl >/dev/null
command -v tar >/dev/null
command -v sha256sum >/dev/null
command -v flock >/dev/null
mkdir -p "$HOME/.local/share/farm"
exec 9>"$HOME/.local/share/farm/bootstrap.lock"
flock -n 9
# Leave an existing installation intact. A different version needs an explicit operator migration.
if command -v msb >/dev/null; then
    msb --version | grep -Eq '(^| )0\.7\.8$'
    exit 0
fi
case "$(uname -m)" in aarch64|x86_64) arch=$(uname -m);; *) exit 1;; esac
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
cd "$work"
bundle="microsandbox-linux-$arch.tar.gz"
base=https://github.com/preinpost/microsandbox/releases/download/v0.7.8
curl --fail --silent --show-error --location --proto '=https' --tlsv1.2 --max-time 300 "$base/$bundle" -o "$bundle"
curl --fail --silent --show-error --location --proto '=https' --tlsv1.2 --max-time 60 "$base/checksums.sha256" -o checksums.sha256
awk -v file="$bundle" '$2 == file || $2 == "*" file { print; found++ } END { if (found != 1) exit 1 }' checksums.sha256 > selected.sha256
sha256sum --check selected.sha256
tar -xzf "$bundle"
set -- libkrunfw.so.*.*.*
[ "$#" -eq 1 ] && [ -f "$1" ]
library=$1
abi=${library#libkrunfw.so.}
abi=${abi%%.*}
mkdir -p "$HOME/.microsandbox/bin" "$HOME/.microsandbox/lib"
install -m 755 msb "$HOME/.microsandbox/bin/msb"
install -m 644 "$library" "$HOME/.microsandbox/lib/$library"
ln -sf "$library" "$HOME/.microsandbox/lib/libkrunfw.so.$abi"
ln -sf "libkrunfw.so.$abi" "$HOME/.microsandbox/lib/libkrunfw.so"
msb --version | grep -Eq '(^| )0\.7\.8$'
