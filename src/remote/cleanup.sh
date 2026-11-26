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
# msb goes only when stackbox installed it on an empty host: the marker says so, msb lists no
# sandboxes, and the list itself must succeed. Any doubt keeps it.
if [ ! -f "$marker" ] || ! grep -q '"installed_by":"stackbox"' "$marker" || ! grep -q '"had_msb":false' "$marker"; then
    say "kept msb: stackbox did not install it on this host"
elif ! names=$(msb list -q </dev/null 2>/dev/null); then
    say "kept msb: cannot list its sandboxes"
elif [ -n "$names" ]; then
    say "kept msb: sandboxes remain in its home"
else
    if rm -f "$msb_bin" "$HOME/.microsandbox/lib"/libkrunfw*; then
        say "removed msb and libkrunfw"
    else
        say "failed msb and libkrunfw"
    fi
fi
# The golden build lock is not removed: other processes lock that file by name, so an empty file
# stays. The golden groups themselves were removed first.
if [ -L "$state" ]; then
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
