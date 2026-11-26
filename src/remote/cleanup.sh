set -u
# Removes what stackbox put on a host, after its VMs and golden groups are gone (`purge` with
# remote files). Every line it prints is `STACKBOX-CLEANUP <kept|removed|failed> <item>`; the
# coordinator lists the kept and failed items for the operator. Each step is safe to repeat: a
# path that is already gone is skipped.
export PATH="$HOME/.microsandbox/bin:$HOME/.local/bin:$PATH"
say() { printf 'STACKBOX-CLEANUP %s\n' "$1"; }
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
# The golden build lock is empty once no build runs; the golden groups were removed first.
if rm -f "$HOME/.microsandbox/stackbox-golden-rocky10.lock"; then
    say "removed golden build lock"
else
    say "failed golden build lock"
fi
if [ -e "$state" ]; then
    if rm -rf "$state"; then
        say "removed $HOME/.local/share/stackbox"
    else
        say "failed $HOME/.local/share/stackbox"
    fi
else
    say "removed $HOME/.local/share/stackbox"
fi
