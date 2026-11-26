# Stops before anything is removed when HOME is unset or empty, which would otherwise point the
# paths below at the root.
: "${HOME:?}"
set -u
# Removes what stackbox put on a host, after its VMs and golden groups are gone (`purge` with
# remote files). Every line it prints is `STACKBOX-CLEANUP <kept|removed|failed> <item>`; the
# coordinator lists the kept and failed items for the operator. Each step is safe to repeat: a
# path that is already gone is skipped.
export PATH="$HOME/.microsandbox/bin:$HOME/.local/bin:$PATH"
say() { printf 'STACKBOX-CLEANUP %s\n' "$1"; }
# Prints the first directory between $HOME and $1 (excluding $HOME itself and $1) that is a symbolic
# link, if any. `rm` follows such a link, so a removal under one would reach whatever it points to.
# $HOME itself may be a link: it is where the user keeps stackbox, so it is used as it is.
linked_component() {
    cl_rest=${1#"$HOME"/}
    cl_path=$HOME
    while [ "$cl_rest" != "${cl_rest#*/}" ]; do
        cl_path="$cl_path/${cl_rest%%/*}"
        cl_rest=${cl_rest#*/}
        if [ -L "$cl_path" ]; then
            printf '%s\n' "$cl_path"
            return 0
        fi
    done
    return 1
}
# A relative or root HOME would resolve the removals below inside the working directory or the
# root, so the script refuses to run with one.
case "$HOME" in
    /) say "failed HOME is the root directory; nothing removed"; exit 1 ;;
    /*) ;;
    *) say "failed HOME is not an absolute path; nothing removed"; exit 1 ;;
esac
state="$HOME/.local/share/stackbox"
marker="$state/msb-install.json"
msb_bin="$HOME/.microsandbox/bin/msb"
msb_lib="$HOME/.microsandbox/lib/libkrunfw"
# A path behind a link is skipped, not followed: it is reported as failed so it shows up as left.
msb_link=$(linked_component "$msb_bin")
lib_link=$(linked_component "$msb_lib")
# msb goes only when stackbox installed it on an empty host: the marker says so, msb lists no
# sandboxes, and the list itself must succeed. Any doubt keeps it.
if [ ! -f "$marker" ] || ! grep -q '"installed_by":"stackbox"' "$marker" || ! grep -q '"had_msb":false' "$marker"; then
    say "kept msb: stackbox did not install it on this host"
elif [ -n "$msb_link" ]; then
    # Checked before `msb list`, so the msb reached through the link is never run.
    say "failed skipped $msb_bin: path goes through a symbolic link"
elif ! names=$(msb list -q </dev/null 2>/dev/null); then
    say "kept msb: cannot list its sandboxes"
elif [ -n "$names" ]; then
    say "kept msb: sandboxes remain in its home"
elif [ -n "$lib_link" ]; then
    if rm -f "$msb_bin"; then
        say "removed msb"
    else
        say "failed msb"
    fi
    say "failed skipped $msb_lib*: path goes through a symbolic link"
else
    if rm -f "$msb_bin" "$HOME/.microsandbox/lib"/libkrunfw*; then
        say "removed msb and libkrunfw"
    else
        say "failed msb and libkrunfw"
    fi
fi
# The golden build lock is not removed: other processes lock that file by name, so an empty file
# stays. The golden groups themselves were removed first.
state_link=$(linked_component "$state")
if [ -n "$state_link" ]; then
    # A link above the state directory is not followed, so nothing under it is removed.
    say "failed skipped $state: path goes through a symbolic link"
elif [ -L "$state" ]; then
    # A link is removed alone; whatever it points to is not stackbox's to delete.
    if rm -f "$state"; then
        say "removed $HOME/.local/share/stackbox"
    else
        say "failed $HOME/.local/share/stackbox"
    fi
elif [ -e "$state" ]; then
    # Only the stackbox state directory under this HOME is ever removed recursively.
    case "$state" in
        /?*/.local/share/stackbox) ;;
        *) say "failed $state is not the stackbox state directory; nothing removed"; exit 1 ;;
    esac
    if rm -rf "$state"; then
        say "removed $HOME/.local/share/stackbox"
    else
        say "failed $HOME/.local/share/stackbox"
    fi
else
    say "removed $HOME/.local/share/stackbox"
fi
