#!/bin/sh
# Builds a libkrunfw whose guest kernel carries the settings of the fragments in
# scripts/libkrunfw/fragments. msb's own kernel has neither module loading nor VFIO, so the
# fragments add what a test needs: today the VFIO/mdev framework and the kernel sample mtty
# driver, so a sandbox can stand in for a host with an mdev-capable device while Nova's vGPU
# path is tested without a GPU.
#
# One fragment is one feature and one file: <name>.config applies to every architecture and
# <name>.<arch>.config only to that one, so the next feature is one more file rather than an
# edit here. Every fragment applies by default; --fragments names a subset and
# --list-fragments prints what there is.
#
# The kernel is the one libkrunfw already ships: only settings change. The revision defaults
# to the one the installed msb pins, so the result matches the runtime stackbox runs on today.
#
# Either architecture is built from any host: the kernel is cross-compiled inside the build
# container with the distribution's cross toolchain, never under docker --platform emulation.
#
# Usage: scripts/build-libkrunfw.sh [--fragments a,b] [--list-fragments] [--revision SHA]
#                                   [--arch aarch64|x86_64|all] [--out DIR] [--force]
#
# The Linux tree is unpacked inside the build container, never on the host: it has files
# that differ only in case (xt_CONNMARK.c and xt_connmark.c), which a default macOS volume
# cannot hold. Artifacts land in <out>/<arch>/: the Linux library for every architecture,
# plus a dylib for this machine's architecture on macOS (only that one can be loaded here),
# and a manifest.txt that states what the kernel was built from.
set -eu
cd "$(dirname "$0")/.."

fragments_dir=scripts/libkrunfw/fragments
image=${LIBKRUNFW_IMAGE:-ubuntu:24.04}
# LIBKRUNFW_DOCKER_COPY=1 copies the sources in and the kernel out with docker cp, for a docker
# daemon that cannot see this filesystem (a CI job on a docker-in-docker runner). Bind mounts
# are the default, since they keep the container's build tree on this machine.
docker_copy=${LIBKRUNFW_DOCKER_COPY:-}
remote=${LIBKRUNFW_REMOTE:-https://github.com/superradcompany/libkrunfw.git}
tab=$(printf '\t')

usage() {
    cat <<'USAGE'
Usage: scripts/build-libkrunfw.sh [options]

  --fragments NAMES
                  comma-separated fragments to apply; the default is every fragment in
                  scripts/libkrunfw/fragments. They always apply in name order, whatever
                  the order here. LIBKRUNFW_FRAGMENTS names them the same way.
  --list-fragments
                  print each fragment with the purpose stated at the top of its file, then
                  exit
  --revision SHA  libkrunfw revision to build (default: the one the installed msb's
                  microsandbox version pins in vendor/libkrunfw)
  --msb VERSION   msb version the built kernel is for, recorded in the manifest (default:
                  the installed msb when it pins this revision). stackbox installs a kernel
                  only on hosts whose msb matches, so pass this with a custom --revision.
  --arch ARCH     aarch64, x86_64 or all (default: this machine). An architecture other
                  than this machine's is cross-compiled in the build container.
  --out DIR       output directory (default: target/libkrunfw); artifacts go to DIR/ARCH
  --force         rebuild even though the previous build matches this revision and config
USAGE
}

die() {
    echo "$*" >&2
    exit 1
}

# A fragment file carries its architecture when its name ends in .aarch64 or .x86_64:
# vfio-mtty.config applies to both, vfio-mtty.aarch64.config only to aarch64. --fragments
# names the part before both, so one feature stays one name.
fragment_name() {
    fn_name=${1##*/}
    fn_name=${fn_name%.config}
    case ${fn_name##*.} in
    aarch64 | x86_64) fn_name=${fn_name%.*} ;;
    esac
    printf '%s\n' "$fn_name"
}

# The architectures a fragment file applies to.
fragment_scope() {
    fs_name=${1##*/}
    fs_name=${fs_name%.config}
    case ${fs_name##*.} in
    aarch64 | x86_64) printf '%s\n' "${fs_name##*.}" ;;
    *) printf '%s\n' 'aarch64, x86_64' ;;
    esac
}

# $1 name, $2 arch: the common file first, then the arch-specific one. Empty when the name
# has no file for that architecture.
fragment_files() {
    if [ -f "$fragments_dir/$1.config" ]; then
        printf '%s\n' "$fragments_dir/$1.config"
    fi
    if [ -f "$fragments_dir/$1.$2.config" ]; then
        printf '%s\n' "$fragments_dir/$1.$2.config"
    fi
}

# Every file of one name, for --list-fragments, which spans both architectures.
fragment_all_files() {
    for suffix in '' .aarch64 .x86_64; do
        if [ -f "$fragments_dir/$1$suffix.config" ]; then
            printf '%s\n' "$fragments_dir/$1$suffix.config"
        fi
    done
}

fragment_names() {
    for file in "$fragments_dir"/*.config; do
        [ -f "$file" ] || continue
        fragment_name "$file"
    done | LC_ALL=C sort -u
}

# What a fragment is for: the comment block at the top of its file.
fragment_purpose() {
    awk '/^#/ { sub(/^# ?/, ""); print "    " $0; next } { exit }' "$1"
}

is_number() {
    case $1 in
    -*) number=${1#-} ;;
    *) number=$1 ;;
    esac
    case $number in
    '' | *[!0-9]*)
        case $number in
        0x?*) ;;
        *) return 1 ;;
        esac
        case ${number#0x} in
        '' | *[!0-9a-fA-F]*) return 1 ;;
        esac
        ;;
    esac
    return 0
}

# Reads one fragment file into the files prepare_fragments sets up. The grammar is the
# kernel's own minus modules: CONFIG_X=y, CONFIG_X="string", CONFIG_X=<number> and
# "# CONFIG_X is not set"; other comment lines and blank lines are skipped.
parse_fragment() {
    parse_file=$1
    parse_line=0
    while IFS= read -r line || [ -n "$line" ]; do
        parse_line=$((parse_line + 1))
        case $line in
        '# CONFIG_'*' is not set')
            frag_symbol=${line#'# CONFIG_'}
            frag_symbol=${frag_symbol%' is not set'}
            frag_op=disable
            frag_value=
            frag_line=$line
            ;;
        '' | '#' | '# '*) continue ;;
        'CONFIG_'*'='*)
            frag_symbol=${line%%=*}
            frag_symbol=${frag_symbol#CONFIG_}
            frag_value=${line#*=}
            frag_line=$line
            case $frag_value in
            y)
                frag_op=enable
                frag_value=
                ;;
            m)
                die "$parse_file:$parse_line: CONFIG_$frag_symbol=m asks for a module, and the guest kernel cannot load modules; write =y"
                ;;
            n)
                die "$parse_file:$parse_line: CONFIG_$frag_symbol=n is not how a symbol is turned off; write '# CONFIG_$frag_symbol is not set'"
                ;;
            '')
                die "$parse_file:$parse_line: CONFIG_$frag_symbol= has no value"
                ;;
            '"'*'"')
                frag_op=set-str
                frag_value=${frag_value#\"}
                frag_value=${frag_value%\"}
                ;;
            *)
                is_number "$frag_value" ||
                    die "$parse_file:$parse_line: CONFIG_$frag_symbol=$frag_value is not y, a number or a quoted string"
                frag_op=set-val
                ;;
            esac
            ;;
        *)
            die "$parse_file:$parse_line: not a kernel config line: $line"
            ;;
        esac
        case $frag_symbol in
        '' | *[!A-Za-z0-9_]*)
            die "$parse_file:$parse_line: not a CONFIG_ symbol: CONFIG_$frag_symbol"
            ;;
        esac
        printf '%s\t%s\t%s\n' "$frag_op" "CONFIG_$frag_symbol" "$frag_value" >>"$ops_file"
        printf '%s\t%s\t%s\n' "CONFIG_$frag_symbol" "$frag_line" "$parse_file" >>"$lines_file"
    done <"$parse_file"
}

# $1 arch: turns the selected fragments into three files in that architecture's build
# directory. fragments.ops is what scripts/config applies inside the container,
# fragments.lines holds every setting with the file that asks for it, and fragments.files
# lists the fragment files for the cache key and the manifest. All of it happens before the
# container starts, so a fragment that cannot work fails the build here.
prepare_fragments() {
    prepare_arch=$1
    prepare_work=$out/build-$prepare_arch
    mkdir -p "$prepare_work"
    ops_file=$prepare_work/fragments.ops
    lines_file=$prepare_work/fragments.lines
    : >"$ops_file"
    : >"$lines_file"
    : >"$prepare_work/fragments.files"
    # A name is a literal word, so a `*` that came in through --fragments must not turn
    # into the files of the working tree.
    set -f
    for fragment in $selected; do
        fragment_file_list=$(fragment_files "$fragment" "$prepare_arch")
        if [ -z "$fragment_file_list" ]; then
            # An architecture-specific fragment has no file for the other architecture; it is
            # skipped there. A name given with --fragments is a request, so that one fails.
            [ -z "$fragments" ] || die "fragment $fragment has no file for $prepare_arch"
            continue
        fi
        for file in $fragment_file_list; do
            printf '%s\n' "$file" >>"$prepare_work/fragments.files"
            parse_fragment "$file"
        done
    done
    set +f
    # Two fragments cannot ask for one symbol with two values. Say which lines disagree
    # instead of failing later on a kernel config that silently kept one of them.
    LC_ALL=C sort -t"$tab" -k1,1 -s "$lines_file" >"$prepare_work/fragments.sorted"
    conflicts=$(LC_ALL=C awk -F"$tab" '
        $1 == symbol && $2 != expected {
            printf "  %s: %s asks %s, %s asks %s\n", $1, file, expected, $3, $2
        }
        { symbol = $1; expected = $2; file = $3 }
    ' "$prepare_work/fragments.sorted")
    [ -z "$conflicts" ] || die "fragments disagree about these $prepare_arch settings:
$conflicts
A symbol takes one value, so one of the two lines has to go."
}

revision=${LIBKRUNFW_REVISION:-}
msb_version=${LIBKRUNFW_MSB:-}
revision_given=
arch=${LIBKRUNFW_ARCH:-}
out=${LIBKRUNFW_OUT:-target/libkrunfw}
fragments=${LIBKRUNFW_FRAGMENTS:-}
force=
list_fragments=
while [ $# -gt 0 ]; do
    case $1 in
    --revision)
        [ $# -ge 2 ] || die '--revision needs a value'
        revision=$2
        revision_given=1
        shift 2
        ;;
    --msb)
        [ $# -ge 2 ] || die '--msb needs a value'
        msb_version=$2
        shift 2
        ;;
    --arch)
        [ $# -ge 2 ] || die '--arch needs a value'
        arch=$2
        shift 2
        ;;
    --out)
        [ $# -ge 2 ] || die '--out needs a value'
        out=$2
        shift 2
        ;;
    --fragments)
        [ $# -ge 2 ] || die '--fragments needs a value'
        [ -n "$2" ] || die '--fragments needs at least one name'
        fragments=$2
        shift 2
        ;;
    --list-fragments) list_fragments=1; shift ;;
    --force) force=1; shift ;;
    -h | --help) usage; exit 0 ;;
    *) usage >&2; die "unknown option: $1" ;;
    esac
done

host_arch=$(uname -m)
case $host_arch in
arm64 | aarch64) host_arch=aarch64 ;;
x86_64 | amd64) host_arch=x86_64 ;;
*) die "unsupported machine architecture: $(uname -m)" ;;
esac
host_os=$(uname -s)
case ${arch:-$host_arch} in
aarch64 | arm64) arches=aarch64 ;;
x86_64 | amd64) arches=x86_64 ;;
all)
    # The container runs natively, so the compiler is only ever told which kernel to build.
    if [ "$host_arch" = aarch64 ]; then arches='aarch64 x86_64'; else arches='x86_64 aarch64'; fi
    ;;
*) die "--arch takes aarch64, x86_64 or all, not $arch" ;;
esac

[ -d "$fragments_dir" ] || die "$fragments_dir is missing"
available=$(fragment_names)
[ -n "$available" ] || die "$fragments_dir holds no *.config fragment"

if [ -n "$list_fragments" ]; then
    echo "Fragments in $fragments_dir. Every one applies unless --fragments names some, and"
    echo "they apply in name order: the common file first, then the architecture's own."
    for fragment in $available; do
        echo
        descriptor=
        for file in $(fragment_all_files "$fragment"); do
            [ -z "$descriptor" ] || descriptor="$descriptor; "
            descriptor="$descriptor${file##*/} ($(fragment_scope "$file"))"
        done
        printf '  %s  (%s)\n' "$fragment" "$descriptor"
        if [ -f "$fragments_dir/$fragment.config" ]; then
            fragment_purpose "$fragments_dir/$fragment.config"
        else
            fragment_purpose "$(fragment_all_files "$fragment" | head -n 1)"
        fi
    done
    exit 0
fi

selected=$available
if [ -n "$fragments" ]; then
    selected=
    while IFS= read -r fragment; do
        fragment=$(printf '%s\n' "$fragment" | tr -d '[:space:]')
        [ -n "$fragment" ] ||
            die "--fragments (or LIBKRUNFW_FRAGMENTS) has an empty name: $fragments"
        case " $(printf '%s' "$available" | tr '\n' ' ') " in
        *" $fragment "*)
            if [ -z "$selected" ]; then
                selected=$fragment
            else
                selected="$selected
$fragment"
            fi
            ;;
        *) die "no such fragment: $fragment (available: $(printf '%s' "$available" | tr '\n' ' '))" ;;
        esac
    done <<EOF
$(printf '%s\n' "$fragments" | tr ',' '\n')
EOF
fi
selected=$(printf '%s\n' "$selected" | sed '/^$/d' | LC_ALL=C sort -u)
selected_names=$(printf '%s' "$selected" | tr '\n' ' ' | sed -e 's/ *$//' -e 's/^ *//')

mkdir -p "$out"
out=$(cd "$out" && pwd)
for arch in $arches; do
    prepare_fragments "$arch"
done

for tool in docker git curl; do
    command -v "$tool" >/dev/null 2>&1 || die "$tool is required"
done
# Only a dylib is linked on this machine; a Linux library comes out of the container, so an
# x86_64 run on an arm64 Mac needs neither cc nor make here.
if [ "$host_os" = Darwin ]; then
    case " $arches " in
    *" $host_arch "*)
        command -v cc >/dev/null 2>&1 || die 'cc is required to link the dylib'
        command -v make >/dev/null 2>&1 || die 'make is required to link the dylib'
        ;;
    esac
fi
docker version --format '{{.Server.Version}}' >/dev/null 2>&1 ||
    die 'the docker daemon is not reachable'
# The build container is half of the toolchain, so its image belongs in the cache key.
# A tag alone would keep a kernel built with another image; the local id is what the tag
# resolves to now, and an image that is not pulled yet lets the next run miss once.
image_id=$(docker image inspect --format '{{.Id}}' "$image" 2>/dev/null || true)

# GET $1 from the GitHub API. GITHUB_TOKEN, when set, authenticates the request: CI runners share
# unauthenticated rate limits.
github_api() {
    if [ -n "${GITHUB_TOKEN:-}" ]; then
        curl -fsSL -m 30 -H "Authorization: Bearer ${GITHUB_TOKEN}" "$1"
    else
        curl -fsSL -m 30 "$1"
    fi
}

# Reads the libkrunfw revision microsandbox version $1 pins in its vendor/ gitlink.
pinned_revision() {
    version=$1
    echo "Looking up the libkrunfw that microsandbox v$version pins" >&2
    entry=$(github_api \
        "https://api.github.com/repos/microsandbox/microsandbox/contents/vendor/libkrunfw?ref=v$version") ||
        die "cannot read vendor/libkrunfw at microsandbox v$version; pass --revision"
    case $entry in
    *'"type": "submodule"'*) ;;
    *) die "vendor/libkrunfw is not a submodule at microsandbox v$version; pass --revision" ;;
    esac
    printf '%s\n' "$entry" |
        sed -n 's/^ *"sha": "\([0-9a-f]\{40\}\)",\{0,1\}$/\1/p' | head -n 1
}

hash_file() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum | cut -d' ' -f1
    else
        shasum -a 256 | cut -d' ' -f1
    fi
}

# Fails unless the final .config still holds every line the fragments ask for. olddefconfig
# drops or rewrites a setting when the Kconfig dependencies do not allow it, so this is where
# a fragment that cannot hold shows up, with the line it asked for and the one kept instead.
verify_fragments() {
    changed=
    while IFS=$tab read -r expected_symbol expected_line expected_file; do
        grep -qxF -- "$expected_line" "$1" && continue
        actual=$(grep -E "^(# )?${expected_symbol}(=| is not set)" "$1" | head -n 1)
        [ -n "$actual" ] || actual='no such setting'
        changed="$changed
  $expected_file asks $expected_line; $1 has $actual"
    done <"$2"
    [ -z "$changed" ] || die "$1 does not keep what the fragments ask:$changed

olddefconfig keeps a symbol only when the Kconfig dependencies are met and the symbol exists
for this architecture; add the missing setting to the same fragment."
}

# kbuild names and libkrunfw's Makefile both read ARCH; x86_64 is the spelling that keeps
# libkrunfw's GUESTARCH (config file and bundle type) right. The build container always sets
# ARCH explicitly, so kbuild never guesses from the container's uname.
kbuild_arch() {
    case $1 in
    aarch64) printf 'arm64' ;;
    x86_64) printf 'x86_64' ;;
    esac
}

# A cross toolchain for the container, unless the kernel matches the container's own arch.
# The distribution builds natively, so the compiler still runs at native speed.
toolchain() {
    if [ "$1" = "$host_arch" ]; then
        printf '%s\n' ''
    elif [ "$1" = aarch64 ]; then
        printf 'aarch64-linux-gnu-\n'
    else
        printf 'x86_64-linux-gnu-\n'
    fi
}

# The compiler package plus the target libc headers: the Linux library needs crti.o, which
# the compiler package alone does not bring in.
toolchain_package() {
    if [ "$1" = "$host_arch" ]; then
        printf '%s\n' ''
    elif [ "$1" = aarch64 ]; then
        printf 'gcc-aarch64-linux-gnu libc6-dev-arm64-cross\n'
    else
        printf 'gcc-x86-64-linux-gnu libc6-dev-amd64-cross\n'
    fi
}

# $1 arch: what the artifacts next to this manifest were built from, so stackbox or a person can
# tell later which kernel a library is.
write_manifest() {
    {
        printf 'revision %s\n' "$revision"
        printf 'msb %s\n' "$msb_version"
        printf 'kernel %s\n' "$kernel"
        printf 'arch %s\n' "$1"
        printf 'kbuild-arch %s\n' "$(kbuild_arch "$1")"
        printf 'toolchain %s\n' "$(toolchain "$1" | sed 's/^$/native/')"
        while IFS= read -r file; do
            printf 'fragment %s sha256 %s\n' "$file" "$(hash_file <"$file")"
        done <"$out/build-$1/fragments.files"
        for artifact in "$out/$1/libkrunfw.$abi.dylib" "$out/$1/libkrunfw.so.$full"; do
            [ -f "$artifact" ] || continue
            printf 'artifact %s sha256 %s\n' "${artifact#"$out"/}" "$(hash_file <"$artifact")"
        done
    } >"$out/$1/manifest.txt"
}

# The installed msb pins the revision stackbox should build against; its version also goes into
# the manifest, which is how a host decides whether it may boot the kernel.
msb_bin=${MSB_PATH:-msb}
msb_installed=
if command -v "$msb_bin" >/dev/null 2>&1; then
    msb_installed=$("$msb_bin" --version 2>/dev/null | sed -n '1s/.* //p')
fi

src=$out/libkrunfw
if [ -z "$revision" ]; then
    [ -n "$msb_installed" ] ||
        die "msb not found (set MSB_PATH); pass --revision and --msb to skip it"
    revision=$(pinned_revision "$msb_installed")
fi
[ -n "$revision" ] || die 'cannot tell which libkrunfw revision to build; pass --revision'
# git prints the revision in lower case, so accept one typed in upper case as the same.
revision=$(printf '%s' "$revision" | tr 'ABCDEF' 'abcdef')
case $revision in
*[!0-9a-f]*) die "not a libkrunfw revision: $revision" ;;
esac
# --msb declares the version; an automatically resolved revision is that msb's. A revision
# given by hand is checked against the installed msb's pin, and only then recorded as its:
# a kernel built for another msb is not installed on hosts by default.
if [ -z "$msb_version" ]; then
    msb_version=$msb_installed
    if [ -n "$msb_installed" ] && [ -n "$revision_given" ]; then
        pinned=$(pinned_revision "$msb_installed" 2>/dev/null || true)
        [ "$pinned" = "$revision" ] || msb_version=none
    fi
fi
[ -n "$msb_version" ] || msb_version=none
case $msb_version in
none) ;;
*[!0-9A-Za-z._-]*) die "not an msb version: $msb_version" ;;
esac

if [ ! -d "$src/.git" ]; then
    if [ -e "$src" ]; then
        die "$src exists and is not a libkrunfw checkout"
    fi
    git init -q "$src"
    git -C "$src" remote add origin "$remote"
fi
# A checkout already at the revision is the tree this build wants, so a rerun needs no
# fetch; the cache key below decides whether the kernel itself is rebuilt. `git` writes
# kernel.c into this tree to link a dylib, which does not move HEAD.
checked_out=$(git -C "$src" rev-parse -q --verify HEAD || true)
if [ "$checked_out" != "$revision" ]; then
    # Fetch the revision alone when the server allows it; a full fetch is the fallback.
    git -C "$src" fetch -q --depth 1 origin "$revision" 2>/dev/null ||
        git -C "$src" fetch -q origin --tags ||
        die "cannot fetch $revision from $remote"
    git -C "$src" checkout -q -f --detach "$revision"
fi

kernel=$(sed -n 's/^KERNEL_VERSION *= *//p' "$src/Makefile" | head -n 1)
abi=$(sed -n 's/^ABI_VERSION *= *//p' "$src/Makefile" | head -n 1)
full=$(sed -n 's/^FULL_VERSION *= *//p' "$src/Makefile" | head -n 1)
[ -n "$kernel" ] && [ -n "$abi" ] && [ -n "$full" ] ||
    die "$src/Makefile does not state KERNEL_VERSION, ABI_VERSION and FULL_VERSION"
kernel_remote=$(sed -n 's/^KERNEL_REMOTE *= *//p' "$src/Makefile" | head -n 1)
kernel_remote=$(printf '%s\n' "$kernel_remote" | sed "s/\$(KERNEL_VERSION)/$kernel/")
echo "Building libkrunfw $revision for $arches (kernel $kernel) into $out"
echo "Fragments: $selected_names"

artifacts=
for arch in $arches; do
    base=$src/config-libkrunfw_$arch
    [ -f "$base" ] || die "$revision has no config-libkrunfw_$arch"
    guest_kbuild_arch=$(kbuild_arch "$arch")
    cross=$(toolchain "$arch")
    cross_package=$(toolchain_package "$arch")
    work=$out/build-$arch
    mkdir -p "$out/$arch"
    so=$out/$arch/libkrunfw.so.$full
    # A dylib links for this machine only; macOS cannot load the other architecture's.
    if [ "$host_os" = Darwin ] && [ "$arch" = "$host_arch" ]; then
        dylib=$out/$arch/libkrunfw.$abi.dylib
    else
        dylib=
    fi

    # Everything the build depends on, so a rerun after nothing changed only reports the
    # artifacts. The toolchain is part of the key: a cross-built .config records it, and the
    # container image is the other half of the compiler. The fragments go in by name and
    # content, in the order they apply; the leading token bumps when the way they are
    # applied changes, which no fragment edit would show.
    key=$({
        printf '%s\n' fragments-v2 "$revision" "$arch" "$kernel" "$guest_kbuild_arch" \
            "${cross:-native}" "$image" "${image_id:-unpulled}"
        cat "$base"
        while IFS= read -r file; do
            printf '%s\n' "$file"
            cat "$file"
        done <"$work/fragments.files"
    } | hash_file)
    if [ -z "$force" ] && [ -f "$work/kernel.config" ] && [ -f "$work/kernel.c" ] &&
        [ -f "$work/libkrunfw.so.$full" ] &&
        [ "$(cat "$work/stamp" 2>/dev/null || true)" = "$key" ]; then
        verify_fragments "$work/kernel.config" "$work/fragments.lines"
        cp "$work/libkrunfw.so.$full" "$so"
        echo "$arch: reusing the kernel built in $work (unchanged settings)"
    else
        mkdir -p "$work"
        if [ -n "$cross" ]; then
            echo "$arch: cross-compiling with $cross in $image (this machine is $host_arch)"
        else
            echo "$arch: compiling in $image (this machine's architecture)"
        fi
        # No --platform here on purpose: the container runs natively and the cross toolchain
        # does the work, while docker --platform would emulate every compile step.
        # The container's script is the text below, run as one bash script with the same
        # environment whichever way the sources reach the container.
        cat >"$work/container.sh" <<'CONTAINER'
            set -o pipefail
            export DEBIAN_FRONTEND=noninteractive
            # The container is throwaway, so the toolchain is installed on every build; a
            # configuration or revision that did not change is skipped before reaching docker.
            apt-get update -qq
            # shellcheck disable=SC2086 # zero or more package names, unquoted on purpose
            apt-get install -y -qq --no-install-recommends \
                build-essential flex bison libelf-dev libssl-dev bc \
                python3 python3-pyelftools curl ca-certificates cpio kmod zstd rsync \
                $CROSS_PACKAGE
            mkdir -p /work/repo
            rsync -a --delete --exclude .git --exclude "linux-*" --exclude tarballs \
                --exclude kernel.c --exclude "libkrunfw*" --exclude "*.dylib" --exclude "*.so*" \
                /host/ /work/repo/
            cd /work/repo
            if [ ! -f "tarballs/$KERNEL.tar.gz" ]; then
                mkdir -p tarballs
                curl --fail --location --retry 5 --retry-delay 2 \
                    "$KERNEL_REMOTE" -o "tarballs/$KERNEL.tar.gz"
            fi
            if [ ! -d "$KERNEL" ]; then
                tar xf "tarballs/$KERNEL.tar.gz"
                ./scripts/apply-kernel-patches.sh "$KERNEL" $(find patches/ -name "0*.patch" | sort)
            fi
            # The tree keeps its dependencies resolved, so only the settings that changed are
            # applied again; the key covers the base config, the fragments and the toolchain.
            # fragments.ops is one setting per line, written here on the host.
            if [ "$(cat "$KERNEL/.config-key" 2>/dev/null || true)" != "$CONFIG_KEY" ]; then
                cp "config-libkrunfw_$GUEST_ARCH" "$KERNEL/.config"
                while IFS=$(printf "\t") read -r op symbol value; do
                    case $op in
                    enable) "$KERNEL/scripts/config" --file "$KERNEL/.config" --enable "$symbol" ;;
                    disable) "$KERNEL/scripts/config" --file "$KERNEL/.config" --disable "$symbol" ;;
                    set-str) "$KERNEL/scripts/config" --file "$KERNEL/.config" --set-str "$symbol" "$value" ;;
                    set-val) "$KERNEL/scripts/config" --file "$KERNEL/.config" --set-val "$symbol" "$value" ;;
                    esac
                done < /out/fragments.ops
                ( cd "$KERNEL" && make ARCH="$KBUILD_ARCH" CROSS_COMPILE="$CROSS_COMPILE" \
                    olddefconfig )
                printf "%s\n" "$CONFIG_KEY" > "$KERNEL/.config-key"
            fi
            touch "$KERNEL"
            make ARCH="$KBUILD_ARCH" CROSS_COMPILE="$CROSS_COMPILE" -j"$(nproc)" kernel.c
            cp "$KERNEL/.config" /out/kernel.config
            cp kernel.c /out/kernel.c
            # The Linux library is always linked here, with the compiler of the target arch:
            # on a macOS host there is no Linux cc, and on a cross build the host cc would be
            # the wrong one.
            "${CROSS_COMPILE}gcc" -fPIC -DABI_VERSION="$ABI" -shared \
                "-Wl,-soname,libkrunfw.so.$ABI" -o "/out/libkrunfw.so.$FULL" /out/kernel.c
            "${CROSS_COMPILE}strip" "/out/libkrunfw.so.$FULL" 2>/dev/null || true
CONTAINER
        build_script=$(cat "$work/container.sh")
        set -- -e "KERNEL=$kernel" -e "KERNEL_REMOTE=$kernel_remote" \
            -e "GUEST_ARCH=$arch" -e "KBUILD_ARCH=$guest_kbuild_arch" \
            -e "CROSS_COMPILE=$cross" -e "CROSS_PACKAGE=$cross_package" \
            -e "CONFIG_KEY=$key" -e "ABI=$abi" -e "FULL=$full"
        if [ -n "$docker_copy" ]; then
            cid=$(docker create "$@" -w /work "$image" bash -eu -c "$build_script") ||
                die "cannot create the $arch build container"
            docker cp "$work/." "$cid:/out" && docker cp "$src/." "$cid:/host" || {
                docker rm -f "$cid" >/dev/null 2>&1
                die "cannot copy the libkrunfw sources into the build container"
            }
            if ! docker start -a "$cid"; then
                docker rm -f "$cid" >/dev/null 2>&1
                die "the $arch kernel build failed"
            fi
            docker cp "$cid:/out/." "$work/" || die "cannot copy the $arch kernel out of the container"
            docker rm -f "$cid" >/dev/null 2>&1 || true
        else
            docker run --rm "$@" -v "$src:/host:ro" -v "$work:/out" -v "stackbox-libkrunfw-$arch:/work" \
                -w /work "$image" bash -eu -c "$build_script" || die "the $arch kernel build failed"
        fi

        verify_fragments "$work/kernel.config" "$work/fragments.lines"
        head -n 1 "$work/kernel.c" | grep "kernel version: ${kernel#linux-}" >/dev/null ||
            die "$work/kernel.c bundles another kernel version"
        cp "$work/libkrunfw.so.$full" "$so"
        printf '%s\n' "$key" >"$work/stamp"
    fi
    [ -s "$so" ] || die "the $arch Linux library was not linked"

    if [ -n "$dylib" ]; then
        # The bundle is complete, so the Makefile here only links it (its kernel.c rule does
        # nothing when the file exists, and the kernel itself is the job of the container).
        cp "$work/kernel.c" "$src/kernel.c"
        ( cd "$src" && make ) || die 'linking the dylib failed'
        cp "$src/libkrunfw.$abi.dylib" "$dylib"
        [ -s "$dylib" ] || die "the $arch dylib was not linked"
    fi

    write_manifest "$arch"
    applied_names=$(while IFS= read -r file; do fragment_name "$file"; done <"$work/fragments.files" |
        LC_ALL=C sort -u | tr '\n' ' ' | sed 's/ *$//')

    echo
    echo "Built $so"
    echo "  kernel   $kernel, $(wc -c <"$so" | tr -d ' ') bytes"
    echo "  revision $revision"
    echo "  msb      $msb_version"
    echo "  settings $(wc -l <"$work/fragments.lines" | tr -d ' ') from $applied_names, verified in $work/kernel.config"
    echo "  manifest $out/$arch/manifest.txt"
    echo "  sha256   $(hash_file <"$so")"
    if [ -n "$dylib" ]; then
        echo "Built $dylib"
        echo "  sha256   $(hash_file <"$dylib")"
    fi
    artifacts="$artifacts $arch"
done

# Where a sandbox test would find msb, so the export lines below are ready to paste.
msb=${MSB_PATH:-}
if [ -z "$msb" ] && [ -x "$HOME/.microsandbox/bin/msb" ]; then
    msb=$HOME/.microsandbox/bin/msb
fi
if [ -z "$msb" ]; then
    msb=$(command -v msb || true)
fi

if [ "$host_os" = Darwin ]; then
    host_library=$out/$host_arch/libkrunfw.$abi.dylib
else
    host_library=$out/$host_arch/libkrunfw.so.$full
fi

echo
case " $artifacts " in
*" $host_arch "*)
    echo "This machine ($host_arch) loads $host_library:"
    ;;
*)
    echo "No library for this machine ($host_arch) was built; the artifacts of this run are Linux libraries:"
    host_library=
    for arch in $artifacts; do
        echo "  $arch: $out/$arch/libkrunfw.so.$full"
    done
    ;;
esac
echo
echo 'Run a sandbox with this kernel (the msb CLI wants MSB_PATH beside MSB_LIBKRUNFW_PATH):'
if [ -n "$msb" ]; then
    echo "  export MSB_PATH=\"$msb\""
else
    echo '  export MSB_PATH="/path/to/msb"'
fi
if [ -n "$host_library" ]; then
    echo "  export MSB_LIBKRUNFW_PATH=\"$host_library\""
else
    echo '  # copy the Linux library above to the matching host; set MSB_LIBKRUNFW_PATH there'
fi
echo '  msb run --no-tty -n mdev-check alpine:3.20 -- sh -c "ls /sys/class/mdev_bus"'
echo
echo 'stackbox installs this kernel on a host as it prepares it, under'
# shellcheck disable=SC2088 # printed documentation, not a path this script expands
echo '~/.local/share/stackbox/libkrunfw/<id>, and boots every VM of that host from it'
echo '(docs/gpu.md). The manifest states the msb it was built for, and a host whose msb'
echo 'does not match keeps booting the stock kernel until preparation upgrades msb.'
echo
echo 'STACKBOX_LIBKRUNFW_PATH overrides the kernel for one process, for a kernel kept elsewhere;'
echo 'scripts/libkrunfw-path.sh prints the built path on its own.'
